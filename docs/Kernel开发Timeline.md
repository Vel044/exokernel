# Exokernel Kernel 开发 Timeline

## 目标

在不破坏现有 UART、QEMU xHCI、Pi5 xHCI、CDC ACM 和 SCServo 链路的前提下，逐步补齐一个可用 Kernel 所需的资源、线程、事件和 IPC 机制。

总体原则：

```text
Kernel：分配、保护、隔离、调度、通知、回收
libOS：PCI、xHCI、USB、CDC ACM、SCServo及其管理策略
```

硬件数据面继续保持直接访问：MMIO 和 DMA 建立映射后，libOS 不通过 IPC 或 syscall 逐次读写设备。

## 依赖顺序

```text
现有单任务基线
    ↓
Frame + VSpace
    ↓
Thread + 协作式 Scheduler
    ↓
Notification + IRQ
    ↓
Endpoint + Reply
    ↓
定时器抢占
    ↓
多地址空间与资源域
    ↓
DeviceHandle / IOMMU / 多核
```

## Timeline

| 阶段 | 建议时间 | 实现内容 | 完成标准 |
| --- | --- | --- | --- |
| 0. 固化基线 | 1–2天 | 整理构建feature；保留QEMU和Pi5回归脚本；记录当前ABI | UART、xHCI、CDC ACM、SCServo既有链路能够重复运行 |
| 1. Frame + VSpace（已完成） | 4–6天 | 物理页Handle；分配、清零、释放；部分映射、撤销映射；W^X；所有权检查 | QEMU smoke与unmap后Data Abort测试通过 |
| 2. Thread + 协作式Scheduler（已完成） | 5–7天 | 保存完整EL0上下文；线程栈；Ready/Running/Blocked/Exited；create/exit/yield；优先级就绪队列 | `thread-ipc-smoke`中的线程创建、yield、退出和过期Handle检查通过 |
| 3. Notification + IRQ（已完成） | 4–6天 | Notification对象；signal/wait/poll；IRQ绑定到Notification；阻塞和唤醒线程 | xHCI通过Notification完成启动、USB枚举和endpoint配置；旧`SYS_IRQ_WAIT/ACK`保留 |
| 4. 定时器抢占 | 3–5天 | ARM Generic Timer；时间片；IRQ入口调度；临界区和抢占保护 | 不调用yield的两个线程也能交替运行；USB和UART不发生回归 |
| 5. 多地址空间与资源域 | 5–8天 | 独立VSpace、资源表和线程集合；故障隔离；任务级退出回收 | 一个地址空间崩溃不会破坏另一个；MMIO、IRQ、Frame不能越权使用 |
| 6. Endpoint + Reply（已完成） | 5–7天 | `send/recv/call/reply/reply_recv`；固定IPC Buffer；阻塞队列；超时留到后续 | `thread-ipc-smoke`中的Send/Recv、Call/Reply和过期Handle检查通过 |
| 7. 后续增强 | 按需求 | DeviceHandle、IOMMU/SMMU、capability传递、优先级、多核 | 根据多设备、多libOS和Pi5 DMA隔离需求分别验收 |

时间是单人开发的相对估计；每个阶段完成并通过回归后再进入下一阶段。

## 第一阶段：Frame + VSpace（已完成）

Frame + VSpace、Thread、IPC和Notification均已实现；下一阶段进入
**ARM Generic Timer抢占**。

### Kernel内部模块

```text
kernel/src/memory/frame.rs
    Frame表、Handle generation、物理页所有权、分配与释放

kernel/src/memory/vspace.rs
    用户VA检查、页表映射、权限、属性、unmap与TLBI

kernel/src/object/task.rs
    提供当前资源域owner，并在退出时触发Frame和Mapping批量回收
```

第一版不急着把`task.rs`改成完整多任务系统；它先作为当前地址空间的资源所有权容器。

### 第一批接口

```text
SYS_FRAME_ALLOC(pages, alignment)
    分配、清零物理页，返回不可伪造的FrameHandle

SYS_FRAME_FREE(frame_handle)
    校验当前资源域所有权并释放未映射Frame

SYS_FRAME_MAP(frame_handle, offset_pages, page_count, user_va, rights)
    把Frame的部分或全部页面映射到当前VSpace，返回MappingHandle

SYS_FRAME_UNMAP(mapping_handle)
    按Kernel记录的精确范围撤销映射并执行TLBI
```

Handle采用`slot + generation`，不向EL0返回可用于释放任意内存的裸PA。Frame arena固定为`0x2_0000_0000..0x2_1000_0000`；当前无IOMMU时，DMA接口仍单独返回设备所需PA，普通Frame不承担DMA语义。

### 第一阶段测试

- 分配后页面必须为零。
- 支持4KB页和连续对齐页。
- 拒绝零页、溢出大小和非法alignment。
- 拒绝Frame映射到BootInfo、MMIO、heap、stack及DMA保留窗口。
- 拒绝RWX映射，保持W^X。
- 拒绝其他所有者、过期generation和伪造Handle。
- 已映射Frame不能直接free。
- unmap后访问原VA应触发EL0 Data Abort。
- 任务退出时自动撤销映射并释放遗留Frame。
- QEMU xHCI与Pi5 UART/xHCI回归继续通过。

已验证命令：

```bash
LIBOS_FEATURES=frame-smoke QEMU_USB_MODE=none bash qemu/run.sh
LIBOS_FEATURES=frame-fault-test QEMU_USB_MODE=none bash qemu/run.sh
```

第一条输出`Frame + VSpace smoke passed`；第二条在unmap后访问
`0x2_0000_0000`，EL1报告预期的Data Abort和对应`FAR_EL1`。

## Thread + IPC + Notification（已完成）

当前已经实现单任务内的多线程基础设施：最多16个线程共享一个VSpace和任务级
资源表，采用单核协作式调度。线程阻塞只改变调用线程状态，不冻结其他Ready线程。

```text
SYS_THREAD_CREATE/EXIT/YIELD/SET_PRIORITY
SYS_ENDPOINT_CREATE/SEND/RECV/CALL/REPLY/REPLY_RECV
SYS_NOTIFICATION_CREATE/SIGNAL/WAIT/POLL/DESTROY
```

每个线程拥有独立用户栈和一页固定IPC Buffer。消息由Kernel在两个线程的Buffer
之间复制，ReplyHandle只允许原服务线程回复对应调用者；大数据仍应通过Frame
映射的共享内存传递。Notification只合并badge，不承诺逐事件排队。

已验证命令：

```bash
LIBOS_FEATURES=thread-ipc-smoke QEMU_USB_MODE=none bash qemu/run.sh
LIBOS_FEATURES=qemu-xhci,usb-echo QEMU_USB_MODE=ftdi bash qemu/run.sh
```

验收覆盖线程创建/优先级/退出、yield、Notification阻塞唤醒和pending badge、
Endpoint Send/Recv、Call/Reply、过期Thread/Notification Handle以及任务退出回收。
第二条命令还验证PCI INTx经Notification唤醒USB执行器，并完成xHCI启动、FTDI枚举
和Bulk endpoint配置。

## 后续工作

下一阶段是 Generic Timer 抢占。当前调度只在显式 `yield`、阻塞、唤醒和退出
等安全点切换，因此不会在任意内核临界区打断。完成抢占后再进入多地址空间、
跨任务Endpoint和DeviceHandle/IOMMU。

## DeviceHandle决定

当前阶段不实现DeviceHandle：单任务已有`mmio_grants`和`irq_grants`，增加Handle不会立刻提高隔离效果。

进入“多地址空间与资源域”阶段时再决定：

- 多个libOS需要独占或共享设备时，引入DeviceHandle；
- 需要热插拔、统一revoke或IOMMU domain时，引入DeviceHandle；
- 仍是单libOS固定设备时，继续使用分离的MMIO、IRQ、DMA授权即可。

## 架构定位

加入线程、Notification和Endpoint不会自动把系统变成微内核。需要保持两条边界：

1. IPC不是访问MMIO、DMA和设备Ring的必经路径；
2. Kernel不增加`USB_TRANSFER`、`SERIAL_WRITE`或`MOTOR_MOVE`等设备协议接口。

这样Kernel提供通用保护机制，而每个libOS仍能直接管理获授权资源并实现自己的OS与设备策略。
