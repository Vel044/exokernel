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

Rust [Driver::liteeth_probe](../../libos/src/drivers/liteeth.rs#L240)：

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

Rust [liteeth_probe](../../libos/src/drivers/liteeth.rs#L292) 也是映射两段：

```rust
let base = devm_platform_ioremap_resource_byname(&pdev, "mac")?;
let buf_base = devm_platform_ioremap_resource_byname(&pdev, "buffer")?;
```

`"mac"` 是寄存器区，`"buffer"` 是收发 SRAM。

往下进入 [linux_api/platform.rs](../../libos/src/linux_api/platform.rs#L30)：

```rust
let resource = dev.resources.iter().find(|r| r.name == name).ok_or(ENXIO)?;//找name为mac 和buffer 资源
super::exo::Mapping::map(*resource)//映射
```

先按名字取资源，再调用 `Mapping::map`。这一步对应 Linux 的“按名字找资源，再交给映射函数”。

[Mapping::map](../../libos/src/linux_api/exo.rs#L24) 检查地址和长度对齐后：

```rust
runtime::map_mmio(resource.base, resource.size, resource.user_va)
    .map_err(|_| -5)?;

Ok(Mapping { va: resource.user_va, size: resource.size, /* ... */ })
```

三个映射参数是：设备 PA、字节长度、指定的 EL0 VA。成功后返回 `Mapping`，保存 VA 和长度，供驱动访问及后续自动解除映射。

其中 [runtime::map_mmio](../../libos/src/runtime/mod.rs#L303) 执行：

```rust
svc(exo_abi::SYS_MAP_MMIO, pa, size, va)
```

进入 EL1 的 [sys_map_mmio](../../kernel/src/syscall/dispatch.rs#L469)，校验授权和地址范围，再建立 Device-nGnRE 映射；返回 0 表示成功。

**Linux 的 `__devm_ioremap` 调用 `ioremap` 建立内核 VA 映射；你的 `Mapping::map` 通过 `SYS_MAP_MMIO` 建立 EL0 VA 映射。**

### 4. 建好以后，LiteEth 怎样使用映射

从两边的 `liteeth_rx` 看实际调用，名字保持一致。Linux：

```c
rx_slot = litex_read8(priv->base + LITEETH_WRITER_SLOT);
len = litex_read32(priv->base + LITEETH_WRITER_LENGTH);
memcpy_fromio(data, priv->rx_base + rx_slot * priv->slot_size, len);
```

Rust [liteeth_rx](../../libos/src/drivers/liteeth.rs#L90)：

```rust
let rx_slot = litex_read8(&self.base, LITEETH_WRITER_SLOT) as usize;
let len = litex_read32(&self.base, LITEETH_WRITER_LENGTH) as usize;
memcpy_fromio(&self.base, self.rx_base + rx_slot * self.slot_size, data);
```

源码中的 `Mmio`、`IoRegion` 也统一使用 `litex_read32`、`litex_write32`、`memcpy_fromio`、`memcpy_toio`；`Mmio` 选择寄存器或 SRAM 映射后，调用同名封装完成访问，见 [liteeth.rs](../../libos/src/drivers/liteeth.rs#L366) 和 [io.rs](../../libos/src/linux_api/io.rs#L6)。

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

### 2. Rust：先 stop，再由 Mapping::drop 执行 UNMAP

Rust [Driver::liteeth_remove](../../libos/src/drivers/liteeth.rs#L328)：

```rust
pub fn liteeth_remove(mut self) -> Result<(), i32> {
    self.liteeth_stop()?;                       // 关设备事件、解绑 IRQ
    unregister_netdev(&mut self.r#priv.netdev)   // 变更本地注册状态
} // self 被消费；随后释放字段
```

`self` 拥有 `liteeth<Mmio>`；`Mmio` 拥有两份 `Mapping`。字段释放时，两段映射分别进入 [Mapping::drop](../../libos/src/linux_api/exo.rs#L46)：

```rust
fn drop(&mut self) {
    if runtime::unmap_mmio(self.va, self.size).is_err() {
        fatal();
    }
}
```

`runtime::unmap_mmio` 执行 `SYS_UNMAP_MMIO(11)`：`x0=EL0 VA`、`x1=长度`。EL1 [sys_unmap_mmio](../../kernel/src/syscall/dispatch.rs#L524) 校验对齐和长度，用 VA 与页数匹配当前任务的映射记录，再执行：

```rust
crate::task::remove_mmio(user_va, pages); // 核对并移除任务映射记录
crate::mmu::unmap(root, user_va, pages);  // 撤销页表项
crate::mmu::flush_el1_tlb();             // 使旧地址翻译失效
```

以上是成功路径摘录；实际实现会检查 `remove_mmio` 的返回值，以及 `unmap` 返回的页数是否完整，失败则返回错误。

```text
EL0 liteeth_remove
  → liteeth_stop → free_irq → SYS_IRQ_UNBIND
  → unregister_netdev
  → Driver / Mmio 字段释放
    → Mapping::drop（mac、buffer 各一次）
    → runtime::unmap_mmio → SYS_UNMAP_MMIO(11)
EL1 sys_unmap_mmio → 核对映射记录 → 撤销 PTE → TLB 失效
```

若调用者直接丢弃 `Driver`，它的 `Drop` 也先调用 `liteeth_stop`，再释放字段；`probe` 中途失败时，已创建的 `Mapping` 会因 `?` 返回而自动 Drop。例如第二段 `buffer` 映射失败，第一段 `mac` 会被解除映射。
