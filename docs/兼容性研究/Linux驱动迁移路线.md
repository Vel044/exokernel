# Linux 驱动怎样迁移到 exokernel/libOS

## 首先改变对照对象

Linux 内核驱动通常不通过用户态 `read/write` syscall 使用硬件，而是调用内核内部 API，并向 tty、USB、网络、块设备等子系统注册回调。应用 syscall 经这些子系统才到达驱动。

因此 syscall 清单用于分析应用兼容；驱动移植还需要 **驱动内部依赖清单**。Linux 的设备模型包含总线、设备及 probe/remove 等关系。[Linux 设备模型](https://docs.kernel.org/6.12/driver-api/driver-model/overview.html)

## 驱动 API 与本项目能力对照

这里的 Linux 函数是待分析依赖类别，不表示每个驱动都依赖每一项。实际范围必须由固定源码、Kconfig 和编译结果确定。

| Linux 驱动依赖 | 本项目现有基础 | 适配需要处理的事情 |
| --- | --- | --- |
| `kmalloc/kzalloc/kfree`、页分配 | EL0 heap，Frame 原语 | C 分配器、清零、对齐、GFP 标志及可阻塞上下文；普通 heap 不能当 DMA 内存 |
| `ioremap/iounmap`、`readl/writel` | `MAP_MMIO/UNMAP_MMIO`、用户态 volatile 寄存器访问 | 授权 PA→VA、访问宽度、端序和屏障；普通指针解引用不等价 |
| `dma_alloc_coherent/free` | `DMA_ALLOC/FREE`、UsbKernel DmaOp | 分配 CPU VA 与设备地址、mask、对齐、失败和生命周期；当前非缓存连续页只覆盖特定模型 |
| `dma_map_single/unmap/sync` | 现有 UsbKernel streaming bounce 路径 | DMA 方向、提交前复制、完成后复制、同步、回收；不是把任意 VA 转整数 |
| scatter-gather、IOMMU | 未核查到完整通用支持 | 拒绝或新增明确映射策略；不能从 PA=DMA address 的当前平台假设推广 |
| `request_irq/free_irq` | Notification create/bind/wait + IRQ ACK/unbind | 建立 EL0 dispatcher 回调、共享 IRQ、handler 生命周期、IRQ_HANDLED/NONE、卸载同步 |
| `spin_lock_irqsave` 等 | 原子锁和线程调度机制 | EL0 不能照搬屏蔽 EL1 IRQ 的特权指令；必须定义虚拟中断/线程并发模型，防止持锁自旋死锁 |
| mutex、wait_queue、completion | Notification + Thread | 计数或条件状态、注册等待的原子性、超时、取消；badge 合并不保存事件次数 |
| workqueue、tasklet、timer | 线程、执行器，EL0 counter | 调度队列、不可睡眠/可睡眠上下文、定时触发、flush/cancel 语义 |
| `udelay/msleep` | EL0 `delay_ns` 忙等 | 微小轮询延迟与可调度睡眠分开；长延迟不能一律忙等 |
| PCI/platform/DTB、clock/reset/regulator | UserBootInfo、PCI 枚举和授权资源 | 设备模型、资源索引、时钟/复位的实际控制；不能用空函数吞掉必需操作 |
| `usb_submit_urb/usb_kill_urb` | CrabUSB 请求/Future/Event Ring | URB、完成回调、取消、错误、buffer 所有权和断开流程，不能只改函数名 |
| serial_core/tty、net_device、block layer | 现有 UART/USB transport/块驱动对象 | 决定导入对应 Linux 子系统，还是替换其对外接口；两者移植范围不同 |
| `devm_*`、引用计数、probe/remove | 本地资源对象及释放接口 | 失败回滚、按依赖顺序释放、异步回调结束后再销毁资源 |

DMA 地址与 CPU 地址及 streaming 生命周期需遵循源侧契约；Linux 文档明确区分虚拟地址、物理地址和设备总线地址。[DMA 指南](https://docs.kernel.org/6.12/core-api/dma-api-howto.html)

USB 请求提交与完成之间也有所有权契约，取消不是简单释放 buffer。[Linux USB 主机 API](https://docs.kernel.org/driver-api/usb/usb.html)

本项目依据：[runtime](../../libos/src/runtime/mod.rs)、[DMA 适配](../../libos/src/drivers/dma.rs)、[USB 执行器](../../libos/src/runtime/usb_executor.rs)、[EL1 syscall/IRQ](../../kernel/src/syscall/dispatch.rs)。

## 四种可选路线

| 路线 | 保留/改变什么 | 适用情况 |
| --- | --- | --- |
| 手工迁移设备逻辑 | 保留寄存器协议，重做 OS 接口和运行方式 | 一个设备、目标清晰；现有 Rust 实现可作参考 |
| Linux DDE 源码兼容 | 保留 Linux C 驱动，补内部 API，并导入必要依赖 | 希望重复复用多个 Linux 驱动；Genode 为实证参考 |
| 语义 IR + 目标后端 | 提取资源操作、控制流和约束，再生成适配代码 | 研究自动化；先有受限操作集和可测后端 |
| Linux VM / 驱动域 | 保留 Linux 内核驱动运行环境，通过协议提供服务 | 目标优先是快速保留完整 Linux 能力，接受额外系统层 |

**建议短期先做依赖提取与一个受限适配后端，再让 IR 接到这个后端。** IR 不会消除 Linux 子系统的依赖，也不会自动证明目标系统满足同步、DMA 和错误处理契约。

## UART / xHCI 如何选择起点

### UART：便于看行为，但完整 Linux PL011 并不只有几个寄存器函数

选择 Linux v6.12 `drivers/tty/serial/amba-pl011.c` 作为候选，需要分析 serial_core、AMBA/设备模型、clock、IRQ、可选 DMA 等依赖。先冻结硬件与配置，再确认哪些路径实际参与构建。[源码入口](https://github.com/torvalds/linux/blob/v6.12/drivers/tty/serial/amba-pl011.c)

第一实验建议范围：单个已授权 PL011；固定配置；字节 RX/TX；接收 IRQ；启动/停止与错误回滚。先不覆盖 Linux tty 行规程、完整 termios、DMAengine、热插拔和电源管理。

必须明确产物身份：若只提取这些设备操作并接本地 transport，这是“PL011 功能子集迁移”；若宣称“原 Linux 驱动不修改”，则要满足它实际调用的 serial_core 等依赖，不能暗中裁掉。

### xHCI：现有 CrabUSB 可对照，但不是 Linux USB core

Linux `drivers/usb/host/xhci*` 依赖 Linux HCD/USB core 及内存、DMA、总线、同步等设施。现有 `USB_KERNEL` 是给 CrabUSB 的 EL0 适配对象，不是 Linux 内核环境。[Linux xHCI 源码目录](https://github.com/torvalds/linux/tree/v6.12/drivers/usb/host)

需要在两个目标中选一个：

1. **迁移 Linux xHCI HCD**：补齐或导入它依赖的 USB core/HCD 接口，替换现有控制器实现；不能把 HCD 当普通 USB 设备驱动接在 CrabUSB 上。
2. **迁移 Linux USB 设备驱动**：保留 CrabUSB xHCI 后端，对上提供有限 USB core/URB 兼容接口，再移植 USB serial 等设备类逻辑。需实现请求完成、取消/断开和对象模型。

第二条也不轻：例如 Linux USB 串口仍可能依赖 usb-serial core/tty。建议 xHCI 先作为 DMA 分配、ring 地址编程和错误回收的 IR 局部样例，完整设备栈放在后续。

## 兼容层不是“同名函数返回成功”

以 IRQ 为例，Linux 驱动传给 `request_irq` 的是回调。目标后端要保存这个回调，建立 Notification 与授权 INTID 的绑定，由 EL0 线程收到通知后调用它。设备状态处理结束后才执行本系统 `IRQ_ACK`，释放时还要保证不会再执行已销毁对象的回调。

本系统 GIC 路径会先屏蔽相应 IRQ 并 EOI，再投递通知。Linux 驱动原有的中断上下文、锁和回调时机不能未经分析直接沿用。

类似地，`dma_alloc_coherent` 可以借用现有连续非缓存 DMA 内存，但需要记录 CPU VA、DMA address、长度、方向/所有权及 mask。设备正在使用时不能 free；没有 IOMMU 时，内核检查分配所有权本身也不构成设备 DMA 越界隔离。

## IR 的第一阶段产物

建议 IR 优先是带语义注解的控制流和调用依赖，而不是立即设计覆盖所有 Linux 的全新语言。保留普通算术/控制流时，可使用 Clang/LLVM 信息辅助；必须额外保留以下驱动语义：

- 资源：MMIO、IRQ、DMA buffer、队列、设备对象的来源、类型和所有权。
- 地址：CPU VA / PA / DMA address，长度、对齐、DMA mask、缓存和同步要求。
- 生命周期：allocate→prepare→submit→complete/cancel→free，所有失败和回滚路径。
- 并发：回调、锁、可能睡眠点、屏障、完成与取消之间的先后关系。
- 来源：文件、函数、行号、Linux 版本、Kconfig、宏/内联展开信息。
- 未支持项：未知外部函数、间接调用、内联汇编与未证明的假设，必须显式保留并阻止不安全生成。

例如把一对 DMA 分配/释放标注出来只能证明工具识别了接口，不能证明中间所有路径都不会越界、提前释放或漏同步。

## 建议的推进顺序与交付物

| 阶段 | 交付物 | 验收 |
| --- | --- | --- |
| D0 固定范围 | Linux tag、硬件、驱动文件、Kconfig、正常/错误路径功能表 | 双方确认迁移的是完整驱动还是子集 |
| D1 依赖提取 | 编译命令、头文件/宏、直接与间接调用、全局状态、未解析依赖 | 不只依赖 nm：内联/宏和回调也覆盖；所有未知项显式列出 |
| D2 适配契约 | 每个 API 的参数、所有权、错误、阻塞和并发语义及目标实现 | 活跃路径无“假成功”桩；哪些依赖导入、哪些重做有记录 |
| D3 一个可运行样例 | C/Rust FFI、限定驱动、构建配置与 QEMU 场景 | 正常、资源不足、超时、重复运行、清理可复现 |
| D4 自动化/IR | 用同一契约生成适配调用，保留源位置与诊断 | 生成结果与人工基准在限定测试集上行为一致 |

若近期研究任务仍只做到 IR，则以 D1 和 IR 数据格式为第一交付，不把 D3/D4 的运行保证提前写进承诺。

## 正确性怎样建立证据

1. **接口契约检查**：宽度/端序/边界、DMA 对齐和地址范围、申请失败、释放顺序。
2. **行为对照**：相同 QEMU 设备和输入下，对比 Linux 与目标驱动的可观察结果；可记录 MMIO/DMA/IRQ 事件辅助解释，不能要求所有时间戳或实现细节完全相同。
3. **负面与并发测试**：设备无响应、IRQ 先于等待、批量完成合并通知、资源耗尽、完成和取消竞态、重复启动停止。单次 echo 成功不够。
4. **目标机验证**：QEMU 测试通过后，再确认 Pi5 的设备发现、地址转换、缓存/屏障和中断路径；QEMU 成功不能替代真实硬件证据。
5. **证明范围声明**：若追求形式化保证，需定义源/目标可观察语义和外部设备模型，并分别证明转换规则及兼容后端契约。LLVM IR 或 seL4 的存在均不自动提供这个证明。

建议会议结论表述为：我们会先建立“一个驱动、一个版本、一个硬件、明确功能子集”的可复现迁移方法，兼容层承接 OS 差异，IR 逐步承担识别和生成；POSIX 层单独解决应用使用方式。
