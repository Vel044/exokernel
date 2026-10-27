# 从 LiteEth 看 MMIO：建立与解除映射

## ① 建立映射：从 liteeth_probe 开始

### 1. LiteEth 要映射哪两段硬件资源

Linux [liteeth_probe](../../../linux/drivers/net/ethernet/litex/litex_liteeth.c#L242)：

```c
priv->base = devm_platform_ioremap_resource_byname(pdev, "mac");
if (IS_ERR(priv->base))
	return PTR_ERR(priv->base);

buf_base = devm_platform_ioremap_resource_byname(pdev, "buffer");
if (IS_ERR(buf_base))
	return PTR_ERR(buf_base);
```

Rust [Driver::liteeth_probe](../../libos/src/drivers/liteeth.rs)：

```rust
let base = devm_platform_ioremap_resource_byname(&pdev, "mac")?;
let buf_base = devm_platform_ioremap_resource_byname(&pdev, "buffer")?;
```

两边都申请两段映射：

| 资源名   | 映射的硬件                | LiteEth 后续用途                                     |
| -------- | ------------------------- | ---------------------------------------------------- |
| `mac`    | 控制、状态寄存器区（CSR） | 读取槽号、长度、pending；写 START、EV_ENABLE 等      |
| `buffer` | 设备收发 SRAM             | `memcpy_fromio` 取出 RX 帧；`memcpy_toio` 放入 TX 帧 |

`mac` 这里是资源名，不是 6 字节 MAC 地址。两段都是设备 MMIO；映射不会申请 DMA 内存。

### 2. Linux：取资源 → 占用物理范围 → 建立内核映射

从 LiteEth 的调用继续往下：

```text
liteeth_probe
  → devm_platform_ioremap_resource_byname(pdev, "mac" / "buffer")
    → platform_get_resource_byname(..., IORESOURCE_MEM, name)
    → devm_ioremap_resource
      → __devm_ioremap_resource
        → devm_request_mem_region       申请占用这段设备物理地址范围
        → __devm_ioremap → ioremap      建立 PA → 内核 VA 映射
        → devres_add                   登记自动回收记录
```

第一层在 [drivers/base/platform.c](../../../linux/drivers/base/platform.c#L142)：

```c
devm_platform_ioremap_resource_byname(struct platform_device *pdev,
				      const char *name)
{
	struct resource *res;
	res = platform_get_resource_byname(pdev, IORESOURCE_MEM, name);//找IORESOURCE_MEM资源
	return devm_ioremap_resource(&pdev->dev, res);//IO_MAP
}
```

主要操作在 [lib/devres.c::__devm_ioremap_resource](../../../linux/lib/devres.c#L125)：

```c
// 资源必须存在，并且属于内存地址资源。
if (!res || resource_type(res) != IORESOURCE_MEM)
	return IOMEM_ERR_PTR(...);
size = resource_size(res);

// 在 Linux 资源树登记占用；存在冲突时失败。
if (!devm_request_mem_region(dev, res->start, size, pretty_name))
	return IOMEM_ERR_PTR(...);

// 建立映射，并登记 devm 回收动作。
dest_ptr = __devm_ioremap(dev, res->start, size, type);
```

[__devm_ioremap](../../../linux/lib/devres.c#L29) 调用 `ioremap`，把映射地址交给 `devres_add` 保存。普通 ARM64 `ioremap` 使用 Device-nGnRE 属性；带 `IORESOURCE_MEM_NONPOSTED` 的资源会选择 `ioremap_np`，使用 Device-nGnRnE，不能把所有 Linux 设备映射都视为同一属性。

[mm/ioremap.c::generic_ioremap_prot](../../../linux/mm/ioremap.c#L15) 负责页对齐、选择内核虚拟地址区间，再调用 `ioremap_page_range` 建立页表。ARM64 入口及属性见 [arch/arm64/mm/ioremap.c](../../../linux/arch/arm64/mm/ioremap.c#L18) 和 [asm/io.h](../../../linux/arch/arm64/include/asm/io.h#L280)。

**返回给 Linux LiteEth 的是内核 VA。** 这一整条链已经运行在内核中，没有从驱动执行用户态系统调用。

### 3. Rust：从 liteeth_probe 的映射调用开始

[liteeth_probe](../../libos/src/drivers/liteeth.rs) 同样调用两次，保留 `IS_ERR/PTR_ERR` 错误分支：

```rust
(*r#priv).base = devm_platform_ioremap_resource_byname(pdev, c"mac".as_ptr());
if IS_ERR((*r#priv).base) { return PTR_ERR((*r#priv).base) as i32; }
let buf_base = devm_platform_ioremap_resource_byname(pdev, c"buffer".as_ptr());
if IS_ERR(buf_base) { return PTR_ERR(buf_base) as i32; }
```

[platform.rs](../../libos/src/linux_api/platform.rs) 按名字取资源、检查物理范围占用、按页对齐，再登记 devm 记录：

```text
devm_platform_ioremap_resource_byname(pdev, name)
  → 在 pdev.resources 找 mac / buffer，冲突返回 ERR_PTR(-EBUSY)
  → exo::map → runtime::map_mmio(PA, 长度, EL0 VA)
  → SYS_MAP_MMIO(6) → EL1 校验授权并建立 Device-nGnRE 页表
  → 保存实际映射的 VA / 长度和资源占用；返回设备 VA（含页内偏移）
```

Linux 返回内核 VA；这里返回 EL0 VA。驱动调用处没有额外后端参数，也不使用 `ExoMapper`。

### 4. 建好以后，LiteEth 怎样使用映射

从两边的 `liteeth_rx` 看实际调用，名字保持一致。Linux：

```c
rx_slot = litex_read8(priv->base + LITEETH_WRITER_SLOT);
len = litex_read32(priv->base + LITEETH_WRITER_LENGTH);
memcpy_fromio(data, priv->rx_base + rx_slot * priv->slot_size, len);
```

Rust [liteeth_rx](../../libos/src/drivers/liteeth.rs)：

```rust
let rx_slot = litex_read8((*r#priv).base.byte_add(LITEETH_WRITER_SLOT));
let len = litex_read32((*r#priv).base.byte_add(LITEETH_WRITER_LENGTH)) as i32;
memcpy_fromio(data, (*r#priv).rx_base.byte_add(rx_slot as usize * (*r#priv).slot_size as usize), len as usize);
```

`.byte_add(offset)` 对应 C 的 `base + offset`。同名 I/O 接口收到完整 VA，由 [exo.rs](../../libos/src/linux_api/exo.rs) 执行 volatile 访问和 `dmb osh`；映射后每次读写不再系统调用。

## ② 解除映射：从 liteeth_remove 开始

### 1. Linux：LiteEth 不显式 iounmap，由设备框架回收

Linux [liteeth_remove](../../../linux/drivers/net/ethernet/litex/litex_liteeth.c#L307)：

```c
struct net_device *netdev = platform_get_drvdata(pdev);
unregister_netdev(netdev);
```

`unregister_netdev` 会关闭仍在运行的网卡，进入 `liteeth_stop`，关闭设备事件并 `free_irq`。**`unregister_netdev` 本身不替 LiteEth 解除这两段 MMIO 映射。**

**`liteeth_remove` 是设备框架调用的回调。它返回后，框架接着执行后面的清理代码。** 不是 LiteEth 自己再调用设备框架。

例如通过 sysfs 解绑设备时，完整主线如下；省略电源管理等与本节无关的操作：

```text
unbind_store                           drivers/base/bus.c
  → device_driver_detach               drivers/base/dd.c
  → device_release_driver_internal
  → __device_release_driver
      ① device_remove(dev)
          → dev->bus->remove(dev)
          → platform_remove(dev)       drivers/base/platform.c
              → drv->remove(pdev)
              → liteeth_remove(pdev)   LiteEth 驱动回调
                  → unregister_netdev(netdev)
                      → 网卡仍打开时调用 liteeth_stop → free_irq
              ← liteeth_remove 返回
          ← platform_remove 返回
        ← device_remove 返回
      ② device_unbind_cleanup(dev)     继续执行框架的下一阶段
          → devres_release_all(dev)    drivers/base/devres.c
          → release_nodes(dev, &todo)
          → dr->node.release(dev, dr->data)
          → devm_ioremap_release       lib/devres.c，先前登记的回收函数
          → iounmap(映射 VA)
          → generic_iounmap → vunmap  mm/ioremap.c，撤销内核映射
```

入口代码见 [unbind_store](../../../linux/drivers/base/bus.c#L235)；卸载驱动等其他解绑入口也会汇入设备解绑流程。

**第一处关键代码：同一个框架函数，先调用驱动，再做资源清理。** [__device_release_driver](../../../linux/drivers/base/dd.c#L1241) 中按顺序执行：

```c
device_remove(dev);          // 经 platform_remove 调用 liteeth_remove，等它返回
/* 中间还有总线 DMA 清理。 */
device_unbind_cleanup(dev);  // 然后回收 devm 资源
```

[device_remove](../../../linux/drivers/base/dd.c#L561) 调用 `dev->bus->remove`；platform 总线把这个函数指针设置为 [platform_remove](../../../linux/drivers/base/platform.c#L1418)，后者调用 `drv->remove(dev)`。LiteEth 设置的是 `.remove_new = liteeth_remove`；在这份 Linux 的 [platform_driver](../../../linux/include/linux/platform_device.h#L236) 中，`remove` 与 `remove_new` 是同一个 union 中的两个名字，使用同一个函数指针。

[device_unbind_cleanup](../../../linux/drivers/base/dd.c#L547) 调用 `devres_release_all(dev)`，后者取出该设备的资源记录，交给 `release_nodes`。

**第二处关键代码：资源记录里保存了具体的回收函数。** 建立映射时，`__devm_ioremap` 已经执行过：

```c
ptr = devres_alloc_node(devm_ioremap_release, sizeof(*ptr), ...);
// 上面保存回收函数；下面保存需要回收的映射 VA，并关联到设备。
*ptr = addr;
devres_add(dev, ptr);
```

现在 [release_nodes](../../../linux/drivers/base/devres.c#L496) 逆序遍历资源记录，对每一项执行：

```c
dr->node.release(dev, dr->data); // 调用当初登记的函数，传入保存的数据
kfree(dr);                     // 回收这条记录
```

遇到 MMIO 映射记录时，`release` 就是 `devm_ioremap_release`，`data` 保存着映射 VA。所以通过函数指针进入 [lib/devres.c](../../../linux/lib/devres.c#L19)：

```c
void devm_ioremap_release(struct device *dev, void *res)
{
    iounmap(*(void __iomem **)res); // 撤销内核 VA 映射
}
```

`res` 指向记录中保存地址的位置，`*(void __iomem **)res` 取出映射 VA。在这里采用的通用 iounmap 路径中，[generic_iounmap](../../../linux/mm/ioremap.c#L60) 调用 `vunmap`。`mac`、`buffer` 两段映射各有一条记录，分别回调解除映射；设备物理范围的占用记录则由另一回调 [devm_region_release](../../../linux/kernel/resource.c#L1638) 释放。设备本身的寄存器和 SRAM 不会被释放。

`probe` 失败后，设备框架也会回收本次 probe 已登记的 devm 资源。

### 2. Rust：remove 返回后，由设备框架回收

[liteeth_remove](../../libos/src/drivers/liteeth.rs) 保留 `pdev` 参数和原调用顺序：

```rust
let netdev = platform_get_drvdata(pdev).cast::<net_device>();
unregister_netdev(netdev);
```

`unregister_netdev` 自动关闭仍在运行的网卡，调用原 `liteeth_stop`，同步结束 IRQ 回调；本身不解除 MMIO。

```text
应用 Driver::remove → module::detach
  → liteeth_remove(pdev)
      → unregister_netdev(netdev)
          → 仍运行时 dev_close → liteeth_stop → free_irq
  ← liteeth_remove 返回
  → devres_release_all(&pdev.dev)                 设备框架继续清理
      → 按登记的逆序解除 buffer、mac 映射
          → exo::unmap → runtime::unmap_mmio
          → SYS_UNMAP_MMIO(11)：x0=VA，x1=映射长度
      → 释放物理范围占用、统计和 netdev 堆对象
```

EL1 核对当前任务映射记录，撤销 PTE 并使 TLB 失效。解除映射不释放设备 SRAM。`probe` 中途失败也由 `module::bind` 执行同一套 devm 回滚。

实现：[module.rs](../../libos/src/linux_api/module.rs)、[platform.rs](../../libos/src/linux_api/platform.rs)、[exo.rs](../../libos/src/linux_api/exo.rs)。
