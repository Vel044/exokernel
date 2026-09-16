# 目标侧接口：先 UART，再 DMA

本文根据当前源码梳理；是接口走读，不是所有硬件和并发情形的正确性证明。

## 1. 三层边界

```text
EL0 应用策略           UART 回显 / USB 串口应用 / 摄像头采集
       ↓
EL0 驱动与运行时       协议、寄存器、DMA 描述符、Future / Event Ring
       ↓ SVC（申请资源、绑定中断、等待通知）
EL1 外核               授权检查、页表、物理页、GIC、调度
```

映射成功后，EL0 通过自己的 VA 直接访问设备 MMIO 和 DMA 内存，并非每次读写都进入内核。内核中的 UART 早期日志与 EL0 的 PL011 驱动也要分开理解。

源码定位：共享定义在 [abi](../../abi/src/lib.rs)，EL0 包装在 [runtime](../../libos/src/runtime/mod.rs)，SVC 异常入口在 [vectors](../../kernel/src/arch/aarch64/vectors.rs)，分发与设备 IRQ 入口在 [dispatch](../../kernel/src/syscall/dispatch.rs)。

## 2. 最小接口表

AArch64 约定：`x8` 为系统调用号，`x0..x5` 为参数，`svc #0` 进入 EL1，`x0` 返回结果。

| 接口 | EL0 参数 | EL1 校验及状态变化 | 返回 |
| --- | --- | --- | --- |
| `SYS_MAP_MMIO` | x0=PA、x1=size、x2=VA | 页对齐、范围溢出、平台与任务 grant、VA 冲突；建立 Device-nGnRE 映射并刷新 TLB | 0 / 错误码 |
| `SYS_UNMAP_MMIO` | x0=VA、x1=size | 核对任务记录的映射范围；撤销映射与刷新 TLB | 0 / 错误码 |
| `SYS_DMA_ALLOC` | x0=size、x1=alignment、x2=VA | 非零、对齐、范围及可记账性；分配连续物理页、清零、登记所有权、建立 Normal Non-cacheable 映射 | DMA address；失败 `u64::MAX` |
| `SYS_DMA_FREE` | x0=VA、x1=size | 核对当前任务记录与页数；撤销映射后释放页 | 0 / 错误码 |
| `SYS_IRQ_BIND` | x0=INTID、x1=Notification、x2=badge、x3=CPU | IRQ grant、通知对象归属、badge 和 CPU；配置并启用 GIC SPI | 0 / 错误码 |
| `SYS_IRQ_ACK` | x0=INTID | 校验绑定及待 ACK 状态；重新使能 SPI | 0 / 错误码 |
| `SYS_IRQ_UNBIND` | x0=INTID | 核对绑定并移除，禁用对应 IRQ | 0 / 错误码 |

Notification 的创建、等待与销毁见 [notification.rs](../../libos/src/kernel_api/notification.rs)。当前不存在 `SYS_IRQ_WAIT`；等待使用 `SYS_NOTIFICATION_WAIT`。普通堆内存与文件内容使用 Frame 系列接口，不能默认当作 DMA buffer。

## 3. UART：一次输入字符的完整链路

打开 [uart_echo.rs](../../libos/src/apps/uart_echo.rs) 的 `run`：

1. 从 `UserBootInfo.uart` 取得设备 PA、长度和 GIC INTID；BootInfo 是资源描述，EL1 的任务 grant 才是授权依据。
2. 调用 `map_mmio(pa, size, UART_VA)`；EL1 校验后建立映射，返回 0。
3. 把 `UART_VA` 包装为 PL011 库的寄存器对象。库通过 volatile 读写寄存器；实际落到已授权设备的物理寄存器，不是普通 RAM。
4. 创建 Notification 并绑定 IRQ，启用 UART RX/timeout 中断；EL0 等待通知。
5. 输入到达：GIC 交付 INTID；EL1 暂时禁用该 SPI，写 EOI，记录待 ACK 并投递 badge，调度唤醒的线程。
6. EL0 读取 RX FIFO、写入 TX FIFO 完成回显；清 UART 设备侧中断源，再调用 `SYS_IRQ_ACK` 重新开放 SPI。

这里要强调三个不同动作：清 UART 中断源、GIC EOI、用户态 IRQ ACK。当前实现的 EOI 在内核入口完成，不能画成用户处理完后才 EOI。

`smoke_test` 只覆盖 MMIO 发字节及 IRQ 绑定/解绑；要演示真实 IRQ 等待和回显，需要运行 `uart-echo` 的 `run` 路径。UART 例子没有 DMA。

## 4. DMA：CPU 与设备使用不同地址

主入口：[dma.rs](../../libos/src/drivers/dma.rs)，调用方是 CrabUSB 的 `DmaOp`。另一个适配例子是 [virtio_blk.rs](../../libos/src/drivers/virtio_blk.rs) 的 `ExoVirtioHal`。

```text
CrabUSB 请求 size / align / 地址约束
  → EL0 选择空闲 DMA VA，按页扩大分配长度
  → SYS_DMA_ALLOC(size, align, VA)
  → EL1 分配连续物理页、清零、登记任务所有权、映射 Non-cacheable
  ← DMA address（当前无 IOMMU，等于 PA）
  → EL0 得到 { CPU pointer = VA, device address = DMA address }
```

- CPU 用 VA 填写 buffer/描述符，设备读取描述符中的 DMA address。当前 PA=DMA address 是平台实现条件，不能提升为通用 IR 恒等式。
- USB 适配器检查地址 mask、alignment、boundary、最大 segment 等约束，不满足时回收本次分配。
- coherent/contiguous 分配目前共用 Non-cacheable 页；屏障仍有意义，缓存属性不等于访问顺序保证。
- streaming 映射采用 bounce buffer。ToDevice/Bidirectional 在 `sync_map_for_device` 复制到 bounce；FromDevice/Bidirectional 在 `sync_map_for_cpu` 复制回来；释放在 `unmap_streaming`。`map_streaming` 本身不是完整传输。
- 描述符发布、doorbell、completion 的具体先后要连同第三方 ring 实现审核。这里只陈述适配行为，不据此宣称任意硬件上的屏障位置都已验证。
- 释放之前必须确认设备不再使用 buffer；`SYS_DMA_FREE` 的所有权检查并不代替设备停机或完成同步。

## 5. xHCI 的中断完成链路

[xhci.rs](../../libos/src/drivers/xhci.rs) 取得 BAR/DTB 资源，映射 MMIO、绑定 Notification，再创建 CrabUSB Host；[usb_executor.rs](../../libos/src/runtime/usb_executor.rs) 负责 poll Future。

```text
设备更新 Event Ring 并触发 IRQ
→ EL1 禁用 SPI → EOI → 记录待 ACK → Notification
→ EL0 handle_event() 消费 Event Ring、推进完成状态
→ IRQ_ACK 重新使能 SPI → 再次 poll Future
```

Event Ring 与 USB 协议在 EL0；内核不解析 TRB。USB 串口、UVC 和上层应用不应各自复制一份 DMA 或 IRQ 管理机制。

## 6. 目标侧约束

当前接口不能直接等同于 Linux 的 `struct device`、DMA domain、IOMMU、scatter-gather 或 managed resource 生命周期。IR 应保留源侧的设备身份、方向、上下文约束和资源关系；后端尚未支持的部分应显式报告。Pi5 与 QEMU 的设备发现方式不同，也不能把 QEMU 地址直接用于真机。
