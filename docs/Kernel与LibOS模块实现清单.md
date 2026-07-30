# Kernel 与 libOS 模块实现清单

开发顺序与各阶段验收标准见：[Kernel开发Timeline.md](./Kernel开发Timeline.md)。
**Frame + VSpace、四核Thread、Endpoint/Reply、Notification和静态优先级抢占调度已完成**；
下一阶段是真机SMP验收与实时性分析。

## 共享 ABI

实现位置：`abi/src/lib.rs`

- 定义 syscall 编号和 AArch64 寄存器调用约定。
- 定义 `UserBootInfo`、`DeviceResource`、`PciHostInfo`、`PciRange`、`PciIntxRoute`。
- `UserBootInfo` 包含可选的直接 xHCI 资源和 `XHCI_TRANSPORT_PCI/DIRECT` 传输类型。
- ABI version 6包含`cpu_count`和完整Endpoint生命周期；当前QEMU和Pi5目标要求4个逻辑CPU上线。
- 定义 BootInfo、UART、ECAM、xHCI BAR、heap、stack、DMA arena和Frame arena的固定EL0 VA。
- 使用 `magic + version + size` 校验 EL1 与 EL0 的 ABI。

## EL1 Kernel 功能

### 1. 启动（UEFI）

实现位置：`kernel/src/main.rs`、`kernel/src/kmain.rs`、`kernel/src/boot/boot_info.rs`

- 接收 UEFI/EL2 提供的内存图和 DTB 地址。
- 解析CPU/PSCI信息，通过SMC `PSCI_CPU_ON`拉起CPU1..3，并等待online mask为`0xf`。
- 初始化 EL1 栈、异常向量、物理内存和 stage-1 页表。
- 加载 `libos.elf`。
- 创建只读 `UserBootInfo`，把其 EL0 VA 放入 `x0`。
- 设置 EL0 PC、SP、PSTATE并通过 `eret`进入libOS。

### 2. DTB 与设备发现

实现位置：`kernel/src/boot/dtb.rs`、`kernel/src/resource/pci.rs`、`kernel/src/resource/resources.rs`

- **kernel内校验并解析完整 DTB，完整 DTB不映射给EL0**。
- 解析 `reg`、`ranges`、`interrupts`、`bus-range`、`interrupt-map`和`dma-coherent`。（寄存器地址、中断、PIC总线、能否DMA）
- 发现 PL011 MMIO/IRQ、GICv2 GICD/GICC、PCI ECAM、PCI MMIO windows和INTx routes。
- 从同一份解析结果生成 Kernel 授权表和裁剪后的 **UserBootInfo**，避免两套资源描述不一致。
- `UserBootInfo` 只暴露获准的 UART、直接 xHCI、PCI host 和 PCI INTx 信息；不暴露 GICD/GICC 物理地址。
- QEMU xHCI 通过 PCI host 资源发现；Pi5 RP1 的 `snps,dwc3` 节点直接提供 xHCI MMIO/IRQ。

#### 与 seL4、MIT Exokernel 的对比

- **seL4**：
```
MMIO Frame capability   → 映射 xHCI 寄存器
IRQHandler capability   → 接收并 ACK xHCI 中断
普通 Frame capability   → 驱动自身内存
DMA / IOMMU capability  → 建立设备可访问的 DMA 映射
Notification capability → 等待 IRQ
```

- **MIT Exokernel**：
- 太早了，那个时候没有xHCI
- PCI驱动、网卡驱动、磁盘驱动在Xok内部

- TCP/IP、UDP和Socket\文件系统\控制台和TTY语义\网络管理服务...在外面

#### 可选方案：DeviceHandle（待讨论）

当前实现已经按当前任务记录`mmio_grants`、`irq_grants`和DMA allocation。EL0申请MMIO或IRQ时，Kernel能够知道是哪个任务在申请哪个PA或INTID，因此`DeviceHandle`不是当前单任务资源保护的必要条件。

`DeviceHandle`的用途是在多任务、设备独占、热插拔或IOMMU场景下，把同一设备的MMIO、IRQ和DMA domain组织成一个可统一claim、release和revoke的Kernel对象。Kernel只知道资源归属，不实现xHCI、USB或CDC ACM协议。

可行的Handle形式：

```rust
struct DeviceHandle {
    slot: u32,       // Kernel设备表中的索引
    generation: u32, // 设备释放后递增，使旧Handle失效
}
```

Kernel内部对象：

```text
KernelDevice
    owner_task       // 当前拥有该设备的任务
    generation       // 检测已释放的过期Handle
    mmio_regions[]   // 该设备获授权的MMIO区域
    irq_resources[]  // 该设备获授权的IRQ及触发标志
    dma_domain       // 当前无IOMMU时仅记录所有权，以后可对应IOMMU domain
```

EL1可在创建任务时把已分配的Handle写入`UserBootInfo`；`UserBootInfo`仍负责传递初始信息，Handle负责在后续系统调用中标识资源所有权。

如果采用，接口应该用Handle替换裸PA/INTID，而不是在原有参数上多加一道重复检查：

```text
SYS_MMIO_MAP(device_handle, region_index, user_va)
SYS_IRQ_BIND(device_handle, irq_index)
SYS_DMA_ALLOC(device_handle, size, alignment, user_va)
SYS_DEVICE_RELEASE(device_handle)
```

### 3. 资源授权、所有权与回收

实现位置：`kernel/src/resource/protect.rs`、`kernel/src/object/task.rs`、`kernel/src/interrupt/gic.rs`

- 保存 DTB 生成的平台 MMIO grant 和 IRQ grant；MMIO、IRQ、DMA 仍是分开的资源类型。
- 当前任务只允许映射自己的 grant，不能仅凭 `UserBootInfo` 中的地址伪造资源请求。
- 保存当前任务拥有的 ELF、BootInfo、heap、stack、MMIO、DMA 和 IRQ 状态。
- `SYS_MAP_MMIO` 检查完整页对齐后的 PA 范围和用户 VA 重叠；`SYS_IRQ_BIND` 按 DTB flags 配置 level/edge。
- 任务退出时禁用 IRQ、完成 active IRQ 的 EOI、撤销映射并释放 DMA 与普通页。

### 4. Frame 与 VSpace

实现位置：`kernel/src/memory/frame.rs`、`kernel/src/memory/vspace.rs`、`kernel/src/memory/mmu.rs`

- Frame表最多记录64个普通RAM对象，Mapping表最多记录128条部分映射。
- `FrameHandle`和`MappingHandle`均使用`slot + generation`拒绝伪造和过期Handle。
- 单个Frame最多16MiB，支持最高2MiB物理对齐，分配后始终清零。
- 普通Frame只允许R、RW、RX的Normal Cacheable映射；同一Frame不能同时存在RW与RX别名。
- checked map拒绝覆盖已有PTE；unmap核对预期PA后清PTE并执行DSB、TLBI、ISB。
- 任务退出时先撤销全部Mapping，TLBI完成后再释放所属Frame。

### 5. ELF Loader（拉起libos.elf）

实现位置：`kernel/src/boot/elf.rs`

- 校验ELF64、AArch64、program header和入口地址。
- 加载所有`PT_LOAD` segment。
- 按`PF_R/PF_W/PF_X`建立代码、只读和数据映射。
- 复制`p_filesz`内容并把`p_memsz - p_filesz`的BSS清零。
- 拒绝segment与BootInfo、MMIO、heap、stack和DMA arena重叠。

### 6. 四核线程与静态优先级调度

实现位置：`kernel/src/arch/aarch64/smp.rs`、`kernel/src/object/thread.rs`、
`kernel/src/scheduler/priority.rs`、`kernel/src/scheduler/timer.rs`

- 一个任务表示一个EL0地址空间及其MMIO、DMA、IRQ等资源；一个任务可以包含多个线程。
- CPU0解析DTB CPU节点并通过PSCI `CPU_ON`拉起CPU1..3；每核分别初始化栈、
  `VBAR_EL1`、GIC CPU Interface和Generic Timer。
- 每个线程记录完整整数/SIMD上下文、独立用户栈、CPU亲和性、基础/有效优先级、
  最大可控优先级、运行CPU和累计运行ticks。
- 维护`Ready`、`Running`、`Blocked`、`Exited`线程状态。
- 每核维护64级Ready Queue和非空bitmap；数值较大的优先级先运行。
- 高优先级Ready线程立即抢占低优先级线程；同优先级线程每1ms移到FIFO队尾。
- Notification WAIT只阻塞调用线程；设备IRQ到达后唤醒对应等待线程。
- 跨核唤醒使用SGI0请求远程重调度；页表变化使用SGI1完成TLB shootdown；
  任务退出使用SGI2停止其他核后再统一回收。
- Endpoint Call建立最多16线程的传递优先级继承链，Reply、取消或退出后恢复。
- libOS选择线程亲和性和基础优先级；Kernel校验MCP并强制执行调度。

### 7. Endpoint、Reply 与 Notification

实现位置：`kernel/src/object/ipc.rs`、`libos/src/kernel_api/endpoint.rs`、`libos/src/kernel_api/notification.rs`

- Endpoint 使用固定等待队列实现同步 `SEND/RECV` rendezvous，不缓存无限消息。
- `CALL` 创建一次性 `ReplyHandle`，只有对应服务线程能够 `REPLY`；
  `REPLY_RECV` 在回复后继续接收下一个请求。
- 每个线程有一页固定 IPC Buffer；Kernel 只复制 `IpcMessage`，不保存任意 EL0 指针。
- Notification 保存 pending badge，`SIGNAL` 使用 OR 合并，`WAIT` 阻塞当前线程，
  `POLL` 无事件时返回 `SYS_ERR_WOULD_BLOCK`。
- IRQ 可以绑定 Notification；IRQ 到达时 EL1 禁用 SPI、写 EOI、记录待 ACK 状态并 signal。
  用户处理设备后通过 `SYS_IRQ_ACK` 重新使能 SPI。这样 active 设备 IRQ 不会阻挡
  Generic Timer 抢占和驱动线程唤醒；旧 `SYS_IRQ_WAIT/ACK` 仍使用延迟 EOI。
- xHCI executor 已使用 `IRQ -> Notification -> handle_event -> IRQ_ACK`，
  MMIO、DMA ring和Event Ring仍由libOS直接访问。
- Endpoint Call把Caller的有效优先级传递给Server，避免中优先级线程造成优先级反转；
  Reply后撤销继承。Notification和单向Send只唤醒线程，不建立长期优先级继承。


## EL1 Kernel 系统调用接口

调用约定：

```text
x8 = syscall编号
x0 = arg0 / 返回值
x1 = arg1
x2 = arg2
x3 = arg3
x4 = arg4
svc #0
```

- `kernel/src/arch/aarch64/vectors.rs`接收EL0 SVC，`kernel/src/syscall/dispatch.rs`按`x8`分发接口。
- `x0..x4`中的Handle、VA、PA、size、alignment、rights和INTID全部是不可信输入。
- Kernel必须检查授权范围、任务所有权、地址对齐、整数溢出和状态转换。

### 1. 内存相关接口

#### 1.1 普通内存接口

| syscall | 输入 | 返回 | Kernel处理 |
| --- | --- | --- | --- |
| `SYS_FRAME_ALLOC` | `x0=pages, x1=align_pages` | `FrameHandle` | 分配连续、对齐、清零的普通RAM。 |
| `SYS_FRAME_FREE` | `x0=FrameHandle` | `0`成功 | 校验owner、generation和无活动Mapping后释放。 |
| `SYS_FRAME_MAP` | `x0=FrameHandle, x1=offset_pages, x2=pages, x3=VA, x4=rights` | `MappingHandle` | 在Frame arena建立Normal Cacheable部分映射并保持W^X。 |
| `SYS_FRAME_UNMAP` | `x0=MappingHandle` | `0`成功 | 核对精确VA/PA范围，撤销PTE并TLBI。 |

Frame arena为`0x2_0000_0000..0x2_1000_0000`。映射成功后libOS直接访问VA，
不需要每次读写携带Handle。当前固定4MiB heap不自动扩容；普通`Box/Vec`仍由
libOS allocator在已有heap内管理。

#### 1.2 MMIO接口

| syscall          | 输入                        | 返回    | Kernel处理                                   |
| ---------------- | --------------------------- | ------- | -------------------------------------------- |
| `SYS_MAP_MMIO`   | `x0=PA, x1=size, x2=EL0 VA` | `0`成功 | 校验完整授权范围和VA，建立Device-nGnRE映射。 |
| `SYS_UNMAP_MMIO` | `x0=EL0 VA, x1=size`        | `0`成功 | 校验完全匹配的任务映射，撤销PTE并TLBI。      |

PCI ECAM 和设备 BAR 都属于 MMIO，使用上述接口，不增加独立的 PCI 配置读写 syscall。ECAM 映射成功后，`libos/src/drivers/pci.rs` 直接通过 volatile 访问计算出的 BDF 和寄存器偏移：

```text
EL0: SYS_MAP_MMIO(ECAM)
EL0: 计算 BDF 和寄存器偏移
EL0: volatile read/write(ECAM + offset)
```

当前没有 `SYS_PCI_CFG_READ` 或 `SYS_PCI_CFG_WRITE`。Kernel 只检查 ECAM 是否属于当前任务的 MMIO 授权范围；当前单任务原型授权 bus 0 ECAM，后续多设备或多 libOS 时再增加 PCI device claim 或更细粒度配置空间授权。

#### 1.3 DMA内存接口

| syscall         | 输入                               | 返回                        | Kernel处理                                                   |
| --------------- | ---------------------------------- | --------------------------- | ------------------------------------------------------------ |
| `SYS_DMA_ALLOC` | `x0=size, x1=alignment, x2=EL0 VA` | DMA address；失败`u64::MAX` | 分配连续、对齐、清零的RAM，建立Non-cacheable映射；v1返回PA。 |
| `SYS_DMA_FREE`  | `x0=EL0 VA, x1=size`               | `0`成功                     | 校验所有权，撤销映射并释放物理页。                           |

DMA 页仍然来自普通物理 RAM，但使用独立接口表达“连续、设备可见、由 Kernel 记录所有权”的约束。当前 v1 无 IOMMU，DMA address 等于 PA；CPU 映射使用 Normal Non-cacheable 属性。

### 2. IRQ接口

| syscall          | 输入       | 返回      | Kernel处理                                     |
| ---------------- | ---------- | --------- | ---------------------------------------------- |
| `SYS_IRQ_BIND`   | `x0=INTID, x1=NotificationHandle, x2=badge, x3=target_cpu` | `0`成功 | 校验授权和CPU，配置SPI目标；`x1=0`保留旧WAIT路径。 |
| `SYS_IRQ_WAIT`   | 无         | 实际INTID | enable绑定IRQ，执行WFI，读取IAR并暂时disable。 |
| `SYS_IRQ_ACK`    | `x0=INTID` | `0`成功   | 校验待ACK状态；Notification模式重新使能SPI，旧WAIT模式先写EOIR。 |
| `SYS_IRQ_UNBIND` | `x0=INTID` | `0`成功   | 禁用IRQ并删除binding。                         |

Notification 绑定扩展使用同一个 `SYS_IRQ_BIND`：

```text
SYS_IRQ_BIND(x0=INTID, x1=NotificationHandle, x2=badge, x3=target_cpu)
```

`x1=0` 保持旧的 `SYS_IRQ_WAIT/ACK` 模式；`x1!=0` 时绑定成功即允许该 SPI。
硬件中断由 Kernel 禁用SPI并EOI后投递到Notification；用户处理完设备后调用
`SYS_IRQ_ACK`重新使能SPI。`target_cpu=IRQ_TARGET_CURRENT`表示投递到调用线程
当前所在CPU；USB线程在CPU2绑定xHCI时使用该值。

### 3. Thread接口

| syscall | 输入 | 返回/行为 | Kernel处理 |
| --- | --- | --- | --- |
| `SYS_THREAD_CREATE` | `x0=entry, x1=arg, x2=cpu, x3=priority, x4=max_control_priority` | `ThreadHandle` | 校验入口、CPU和`0..63`优先级，分配栈/IPC Buffer并加入目标核Ready Queue。 |
| `SYS_THREAD_SET_PRIORITY` | `x0=ThreadHandle, x1=priority` | `0/error` | 校验目标所有权及调用线程MCP，更新基础/有效优先级并按需跨核抢占。 |
| `SYS_THREAD_EXIT` | `x0=exit_code` | 不返回 | 回收当前线程栈和IPC Buffer；最后线程退出时清理任务。 |
| `SYS_THREAD_YIELD` | 无 | `0` | 仅放弃当前同优先级时间片，把线程移到该级FIFO队尾。 |
| `SYS_THREAD_RUNTIME` | `x0=ThreadHandle` | Generic Timer ticks | 返回该线程累计执行时间，供libOS监控。 |

线程入口约定为 `extern "C" fn(arg, thread_handle, ipc_buffer_va) -> !`。

### 4. Endpoint与Reply接口

| syscall | 输入 | 返回/行为 | Kernel处理 |
| --- | --- | --- | --- |
| `SYS_ENDPOINT_CREATE` | 无 | `EndpointHandle` | 创建当前任务拥有的同步Endpoint。 |
| `SYS_ENDPOINT_DESTROY` | `x0=EndpointHandle` | `0/error` | 仅销毁无Sender、Receiver和活动Reply的Endpoint，并递增generation。 |
| `SYS_ENDPOINT_SEND` | `x0=EndpointHandle` | 阻塞直到接收者取得消息 | 从当前线程IPC Buffer复制消息。 |
| `SYS_ENDPOINT_RECV` | `x0=EndpointHandle` | 阻塞直到发送者到达 | 把发送者消息复制到当前线程IPC Buffer。 |
| `SYS_ENDPOINT_CALL` | `x0=EndpointHandle` | 阻塞直到Reply | 创建一次性ReplyHandle并等待回复。 |
| `SYS_ENDPOINT_REPLY` | `x0=ReplyHandle` | 阻塞/让出后返回 `0` | 只允许对应服务线程回复调用者。 |
| `SYS_ENDPOINT_REPLY_RECV` | `x0=EndpointHandle, x1=ReplyHandle` | 回复后等待下一请求 | 合并服务线程常用的Reply和Recv流程。 |

消息格式是 `IpcMessage { label, words[4], reply }`；大数据通过Frame映射的共享
内存传输，避免把设备缓冲区或任意用户指针交给Kernel。

### 5. Notification接口

| syscall | 输入 | 返回/行为 | Kernel处理 |
| --- | --- | --- | --- |
| `SYS_NOTIFICATION_CREATE` | 无 | `NotificationHandle` | 创建当前任务拥有的异步事件对象。 |
| `SYS_NOTIFICATION_SIGNAL` | `x0=NotificationHandle, x1=badge` | `0` | OR合并badge并唤醒一个等待线程。 |
| `SYS_NOTIFICATION_WAIT` | `x0=NotificationHandle` | `badge` | 有pending立即消费，否则阻塞当前线程。 |
| `SYS_NOTIFICATION_POLL` | `x0=NotificationHandle` | `badge`或`WOULD_BLOCK` | 非阻塞检查并消费pending badge。 |
| `SYS_NOTIFICATION_DESTROY` | `x0=NotificationHandle` | `0` | 仅允许无等待者且未绑定IRQ的对象销毁，generation递增。 |

### 6. 任务与早期日志接口

| syscall    | 输入                      | 返回   | Kernel处理                         |
| ---------- | ------------------------- | ------ | ---------------------------------- |
| `SYS_PUTS` | `x0=EL0字符串VA, x1=长度` | `0`    | 只用于EL0接管UART前的早期日志。    |
| `SYS_EXIT` | `x0=退出码`               | 不返回 | 清理MMIO、DMA、IRQ和普通任务内存。 |

## EL0 libOS 功能

### 1. 入口与运行时

实现位置：`libos/src/main.rs`、`libos/src/runtime/mod.rs`

- 从`x0`读取并校验`UserBootInfo`。
- 初始化固定用户态heap和global allocator。
- 封装所有AArch64 SVC接口。
- UART接管前`runtime::puts`使用`SYS_PUTS`，接管后直接写PL011 MMIO。
- 使用`CNTVCT_EL0/CNTFRQ_EL0`实现忙等delay。

### 2. 普通Frame封装

实现位置：`libos/src/kernel_api/frame.rs`、`libos/src/apps/system_smoke/frame.rs`

- `Frame::allocate/map/free`和`Mapping::as_ptr/unmap`隐藏原始Handle。
- 应用通过映射后的指针直接访问内存，资源释放时才再次调用Kernel。
- `system-smoke`中的Frame模块覆盖零初始化、部分映射、W^X、错误Handle和状态转换。

### 3. Thread、Endpoint与Notification封装

实现位置：`libos/src/kernel_api/thread.rs`、`libos/src/kernel_api/endpoint.rs`、
`libos/src/kernel_api/notification.rs`、`libos/src/apps/system_smoke/ipc.rs`、
`libos/src/apps/system_smoke/priority.rs`

- `Thread::spawn(entry, arg, ThreadConfig)`明确指定CPU、基础优先级和MCP。
- `set_priority`在MCP范围内调整基础优先级，`runtime_ticks`读取累计执行时间，
  `current_cpu`读取Kernel写入`TPIDRRO_EL0`的逻辑CPU编号。
- `yield_now`只把当前线程移到同优先级FIFO队尾。
- `Endpoint::send/recv/call/reply/reply_recv/destroy`配合显式`IpcBuffer`使用；
  主线程使用固定首地址，新线程从入口参数`x2`取得自己的Buffer VA。
- `Notification::signal/wait/poll/destroy/bind_irq`封装异步事件和IRQ绑定。
- `system-smoke`中的IPC模块验证线程、同步IPC、pending badge、过期Handle和回收。
- `system-smoke`中的Priority模块验证CPU亲和性、同级1ms轮转、高优先级抢占和
  Endpoint Call优先级继承。
- `system-smoke`中的Robot模块同时运行控制、推理和可选USB线程，验证timer PPI、Endpoint、Notification和xHCI IRQ可以共存。

### 4. UART驱动

实现位置：`libos/src/apps/uart_echo.rs`

- 从`UserBootInfo.uart`读取已授权MMIO和INTID。
- 使用`arm-pl011-uart`直接访问EL0 UART VA。
- QEMU smoke test验证TX和IRQ bind/unbind，并切换libOS日志到直接UART。
- Pi5模式开启RXI/RTI，执行WAIT、读取FIFO、回写、清设备中断、ACK。

### 5. PCI驱动

实现位置：`libos/src/drivers/pci.rs`

- 从`UserBootInfo.pci`读取ECAM、bus范围、PCI windows和INTx routes。
- 映射bus 0 ECAM并扫描device/function。
- 查找class code `0x0c0330`的xHCI。
- 识别32-bit/64-bit BAR并探测BAR大小。
- 把PCI bus address翻译为CPU PA。
- 打开Memory Space Enable和Bus Master Enable。
- 根据BDF、Interrupt Pin和INTx route得到GIC INTID。
- 该模块只用于QEMU PCI xHCI；Pi5直连RP1后端不映射ECAM，也不访问PCI配置空间。

### 6. CrabUSB DMA Adapter

实现位置：`libos/src/drivers/dma.rs`

- 实现CrabUSB `KernelOp/DmaOp`。
- 从固定DMA arena选择EL0 VA。
- 通过DMA syscall申请和释放连续内存。
- 记录allocation slot并拒绝重叠或错误释放。
- streaming mapping使用bounce buffer并按方向复制数据。
- 在ring、TRB和doorbell操作前后执行所需barrier；由于 DMA 页是 Non-cacheable，跳过默认 cache flush/invalidate，避免读取 `CTR_EL0` 和执行 `DC IVAC/CIVAC`。

### 7. xHCI与USB

实现位置：`libos/vendor/crab-usb`、`libos/src/drivers/xhci.rs`

- QEMU后端从PCI枚举结果映射xHCI BAR并绑定INTx IRQ。
- Pi5后端直接从`UserBootInfo.xhci`映射RP1 DWC3标准xHCI区域并绑定IRQ。
- 初始化DCBAA、Command Ring、Event Ring、ERST、device context和scratchpad。
- 复位并启动xHCI控制器。
- 配置primary interrupter并reset Root Hub ports。
- 执行Enable Slot、Address Device、descriptor读取和endpoint配置。
- 通过Event Ring完成command和transfer Future。

### 8. USB执行器

当前实现位置：`libos/src/runtime/usb_executor.rs`

- poll CrabUSB Future。
- Future为Pending时等待xHCI IRQ绑定的Notification。
- Notification唤醒USB线程后调用`EventHandler::handle_event()`。
- 消费Event Ring后调用IRQ ACK。
- 当前USB任务使用单Future执行器；system-smoke中由独立USB线程运行，
  Notification阻塞只影响该线程。

### 9. CDC ACM与FTDI USB串口

驱动实现位置：`libos/src/drivers/usb_serial.rs`

应用实现位置：`libos/src/apps/usb_task.rs`、`libos/src/apps/usb_echo.rs`

- 识别CDC控制接口（class `0x02`、subclass `0x02`、protocol `0x01`）和数据接口（class `0x0a`）。
- 从descriptor动态寻找Bulk IN/OUT endpoint，发送`SET_LINE_CODING`和`SET_CONTROL_LINE_STATE`。
- `usb_serial.rs`只提供配置完成的异步字节流，不决定上层业务。
- `usb_echo.rs`负责把CDC ACM Bulk IN数据原样提交到Bulk OUT。
- 保留QEMU FTDI `0403:6001`兼容路径，继续处理FTDI状态字节和vendor request；它只在`usb-echo`诊断feature下启用。

QEMU真实USB透传由`qemu/run.sh`控制：

```bash
QEMU_USB_MODE=host \
QEMU_USB_VENDOR_ID=0x1a86 \
QEMU_USB_PRODUCT_ID=0x55d3 \
QEMU_USB_SERIAL=5A7C119177 \
bash qemu/run.sh
```

macOS上推荐使用已验证的1 Mbps串口桥接路径：

```bash
QEMU_USB_MODE=serial-bridge \
QEMU_SERIAL_PATH=/dev/cu.usbmodem5A7C1191771 \
bash qemu/run.sh
```

该模式由QEMU模拟FTDI并连接Unix socket。宿主Rust桥只负责用
`IOSSIOSPEED`保持真实TTY为1 Mbps及转发原始字节；PCI、xHCI、USB、
FTDI状态头处理、SCServo封包和运动策略仍全部在EL0 libOS中执行。

默认命令只执行PING和读取。显式启用实际运动：

```bash
QEMU_USB_MODE=serial-bridge \
QEMU_SERIAL_PATH=/dev/cu.usbmodem5A7C1191771 \
LIBOS_FEATURES=qemu-xhci,scservo,scservo-move \
bash qemu/run.sh
```

该路径已在QEMU上验证：UART smoke、PCI xHCI、六个STS3215 PING、
位置/状态读取均成功，并由EL0移动到`[0, 0, 0, 0, 0, 50]`后关闭扭矩。

不带环境变量时，QEMU默认构建和启动SCServo应用，并选择上述CDC ACM设备；
FTDI回归需要显式使用`LIBOS_FEATURES=qemu-xhci,usb-echo`和
`QEMU_USB_MODE=ftdi`。

Hub 路径使用 QEMU 模拟 Hub，而不是透传 Mac 的物理 Hub。`usb-host`
只选择并透传一个物理 USB function，物理 Hub 的下游拓扑不会自动随它进入
guest。可用模式：

```text
QEMU_USB_MODE=hub-ftdi  // xHCI -> QEMU Hub -> QEMU FTDI
QEMU_USB_MODE=hub-host  // xHCI -> QEMU Hub -> Mac 上的真实 CDC ACM 设备
```

真实 CDC 设备可以物理连接在 Mac 的 Hub 上；QEMU 仍按 VID/PID/serial 选择
这个下游设备，再把它挂到 guest 内模拟 Hub 的 `1.1` 端口。这样能够稳定测试
CrabUSB 的 Hub descriptor、下游端口、route string 和 Transaction Translator 路径。
`hub-host` 对 `usb-host` 使用 `guest-reset=off`，避免 guest 的 Hub 端口 reset
同时触发 macOS libusb 物理设备 reset，造成设备临时消失和端口 enable 超时。
当多个转接器使用相同 VID/PID 时，通过 `QEMU_USB_SERIAL` 选择实际连接舵机
总线的设备；当前 SO101 转接器为 `5A7C119177`。

`QEMU_USB_SERIAL`用于多个同型号设备的精确选择。Mac上的Apple USB驱动
可能占用设备，QEMU出现`libusb ... ACCESS`时需要改在Linux解绑宿主驱动后
透传，或直接在Pi5上测试。

### 10. SCServo Protocol 0与SO101电机

实现位置：`libos/src/drivers/scservo.rs`

- CDC ACM Bulk IN/OUT 被封装为可复用的异步字节流，配置为 `1_000_000 8N1`。
- 实现 Protocol 0 的 PING、READ、WRITE、REG_WRITE、ACTION、SYNC_READ 和 SYNC_WRITE。
- 校验 `FF FF ID LENGTH ... CHECKSUM`，处理半包、粘包、错误位和 checksum。
- 提供 `FeetechMotorsBus`、`MotorConfig`、`MotorCalibration`、`MotorNormMode` 和 `ControlTable`。
- 固定提供 SO100/SO101 的 1 到 6 号 `sts3215` 配置及夹爪 `0..100` 归一化。
- 默认启动只 PING 和读取位置/状态，不使能扭矩、不写目标位置。

默认构建启用 `qemu-xhci,scservo` 或 `pi5-xhci,scservo`。
USB 回显是显式诊断构建：

```bash
LIBOS_FEATURES=qemu-xhci,usb-echo QEMU_USB_MODE=ftdi bash qemu/run.sh
```

### 11. 日志

实现位置：`libos/src/runtime/logger.rs`

- 实现CrabUSB使用的`log` facade。
- 使用固定栈缓冲区格式化日志。
- 最终调用`runtime::puts`。

## EL0 libOS 使用Kernel接口

| libOS模块      | 使用的Kernel接口                                                                  |
| -------------- | --------------------------------------------------------------------------------- |
| `runtime.rs`   | 封装全部SVC；初始化固定普通heap；切换早期/直接UART日志。                          |
| `memory.rs`    | `FRAME_ALLOC/MAP/UNMAP/FREE`；映射后向调用者提供直接VA访问。                    |
| `thread.rs`    | `THREAD_CREATE/SET_PRIORITY/YIELD/EXIT/RUNTIME`；提供CPU亲和性和优先级配置。 |
| `ipc.rs`       | `ENDPOINT_CREATE/DESTROY/SEND/RECV/CALL/REPLY/REPLY_RECV`；固定IPC Buffer。     |
| `notification.rs` | `NOTIFICATION_CREATE/SIGNAL/WAIT/POLL/DESTROY`和Notification IRQ绑定。       |
| `uart_echo.rs` | `MAP_MMIO`、`IRQ_BIND/WAIT/ACK/UNBIND`。                                          |
| `pci.rs`       | QEMU路径使用`MAP_MMIO`映射ECAM；配置空间读写不再进入Kernel；Pi5路径不使用。       |
| `dma.rs`       | `DMA_ALLOC/FREE`。                                                                |
| `xhci.rs`      | QEMU映射xHCI BAR，Pi5映射直接xHCI资源；使用`MAP_MMIO`和Notification IRQ绑定。    |
| `usb_executor.rs` | `NOTIFICATION_WAIT`、`IRQ_ACK`；在EL0消费CrabUSB Event Ring。                |
| `usb_serial.rs` | 在xHCI之上配置CDC ACM/FTDI并暴露Bulk字节流，不新增Kernel syscall。              |
| `usb_task.rs`  | 组合xHCI、USB串口和具体应用；自身不实现硬件协议。                               |
| `scservo.rs`   | 在USB串口Bulk IN/OUT之上实现Protocol 0和FeetechMotorsBus。                      |
| `scservo_app.rs` | 执行PING、读取和可选中位运动策略，不新增Kernel syscall。                       |
| panic/错误退出 | `SYS_EXIT`。                                                                      |
