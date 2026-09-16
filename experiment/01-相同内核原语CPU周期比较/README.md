# 实验 01：机器人热路径内核原语 CPU 周期比较

## 1. 实验目标

只比较本 Exokernel 与 seL4，不加入 Linux。原因是 Linux 没有与 Frame、
Notification、Endpoint、IRQHandler 等对象一一对应的用户态原语；用 futex、
socket 或 mmap 强行代替，会把 Linux 的高层服务成本混入 Kernel 原语比较。

本实验从机器人的一次控制周期出发，把热路径分为三部分：

```text
收：USB Bulk IN完成，USB线程被IRQ唤醒并取得相机帧或舵机状态
发：USB Bulk OUT完成，USB线程确认控制命令已经发送
推理：推理线程被输入事件唤醒、获得CPU并完成ACT前向
```

实验回答：两套 Kernel 为这些路径提供的相同原语分别需要多少 CPU 周期，以及
同核、跨核和高优先级唤醒的尾延迟有多大。

## 2. 一个关键边界

初始化阶段不计入本实验。以下操作在计时开始前完成：

```text
MMIO映射、DMA分配、Notification/IRQ绑定、Thread创建、模型加载、
推理workspace分配、USB枚举和CDC ACM配置
```

映射完成后，USB数据不经过 Kernel：

```text
Bulk OUT：EL0写DMA buffer/TRB/doorbell
Bulk IN ：xHCI把数据DMA到EL0 buffer
```

因此收和发在 Kernel 侧都只使用“等待完成事件”和“确认IRQ处理完成”。方向差异
位于EL0的TRB和DMA操作中，不应伪造成两个不同的Kernel接口。

ACT的数值前向也是纯EL0 CPU计算；当前实现从进入`predict`到返回没有系统调用。
推理部分要比较的是输入到达后的唤醒、调度和抢占成本，而不是神经网络运算本身。
神经网络运算放在实验02中比较。

## 3. 两个系统的原语对应

| 机器人阶段 | 本 Exokernel | seL4 | 被测含义 |
| --- | --- | --- | --- |
| 收：等待Bulk IN | `NOTIFICATION_WAIT` | `seL4_Wait` | pending快路径或阻塞当前USB线程 |
| 收：IRQ唤醒 | IRQ → Notification | IRQHandler → Notification | 硬件完成到USB线程恢复运行 |
| 收：完成IRQ | `IRQ_ACK` | `seL4_IRQHandler_Ack` | 消费Event Ring后重新开放IRQ |
| 发：等待Bulk OUT | `NOTIFICATION_WAIT` | `seL4_Wait` | 等待发送完成事件 |
| 发：IRQ唤醒 | IRQ → Notification | IRQHandler → Notification | xHCI发送完成到USB线程恢复运行 |
| 发：完成IRQ | `IRQ_ACK` | `seL4_IRQHandler_Ack` | 处理完成后重新开放IRQ |
| 推理：输入就绪 | `NOTIFICATION_SIGNAL/WAIT` | `seL4_Signal/Wait` | USB线程通知推理线程 |
| 推理：线程切换 | 静态优先级调度 | seL4 TCB调度 | 推理线程从Ready变为Running |
| 推理：高优先级抢占 | Notification唤醒触发抢占 | Notification唤醒触发抢占 | 高优先级任务获得CPU的延迟 |
| 可选跨进程输入 | `ENDPOINT_CALL/REPLY` | `seL4_Call/ReplyRecv` | 独立VSpace时传递控制消息 |

`Endpoint`不是当前单一libOS内运行ACT所必需的原语。只有后续把USB与推理拆成
独立VSpace时才纳入核心结果；当前先实现为扩展项。

## 4. 测试项目

### 4.1 收：Bulk IN Kernel路径

#### R1：Notification pending快路径

计时前先产生Notification，随后测量一次不阻塞的wait：

```text
Exokernel：NOTIFICATION_SIGNAL → 开始计时 → NOTIFICATION_WAIT → 停止计时
seL4：     seL4_Signal        → 开始计时 → seL4_Wait        → 停止计时
```

R1不包含线程切换，用来观察对象查询、状态清除和Kernel往返成本。

#### R2：同核阻塞唤醒

USB线程与触发线程固定在同一个CPU：

```text
USB线程WAIT并Blocked
→ 触发线程SIGNAL
→ Kernel把USB线程置为Ready并调度
→ USB线程记录恢复运行时刻
```

报告`SIGNAL开始 → WAIT返回`的完整延迟。

#### R3：跨核阻塞唤醒

触发线程固定CPU0，USB线程固定CPU2。该结果包含远程重调度SGI/IPI成本，不能
与R2合并。

#### R4：真实IRQ投递与ACK

使用相同xHCI控制器和同类USB传输，测量：

```text
xHCI完成事件
→ GIC IRQ
→ Notification唤醒USB线程
→ 空的用户态事件处理钩子
→ IRQ ACK返回
```

如果无法从设备侧取得精确IRQ产生时刻，则分别记录Kernel IRQ入口时间、用户态
WAIT返回时间和ACK返回时间，并把“设备完成到Kernel入口”标记为不可观测。

### 4.2 发：Bulk OUT Kernel路径

#### S1：提交发送

只验证提交过程没有系统调用：

```text
写DMA buffer → 填OUT TRB → 写doorbell
```

该项属于EL0驱动路径，不纳入Kernel周期几何平均。

#### S2：发送完成唤醒

执行真实Bulk OUT，并测量与R4相同的IRQ → Notification → ACK路径。R4与S2应
非常接近；若差异明显，应检查xHCI事件数量、传输大小和用户态Event Ring处理，
不能直接归因于Kernel。

#### S3：IRQ ACK

单独批量测量已经处于可ACK状态的IRQ确认操作：

```text
Exokernel：IRQ_ACK
seL4：     seL4_IRQHandler_Ack
```

不能在没有合法active/bound IRQ的情况下重复伪造ACK；每个样本必须由一次合法
IRQ事件配对产生。

### 4.3 推理：输入到CPU执行

#### I1：输入通知pending快路径

与R1相同，但Notification语义标记为`INFERENCE_INPUT_READY`。该数据可以复用
R1的原始测量，不重复计算为第二个独立样本。

#### I2：同核推理线程唤醒

USB线程和推理线程同核，推理线程优先级更高：

```text
推理线程WAIT
→ USB线程SIGNAL
→ 高优先级推理线程立即抢占
→ 推理线程记录开始执行时刻
```

#### I3：跨核推理线程唤醒

USB线程固定CPU2，推理线程固定CPU3。测量SIGNAL到CPU3开始执行的时间，包括
远程唤醒和缓存一致性开销。

#### I4：上下文切换基线

两个预先创建的同优先级线程在同核进行Notification ping-pong。测完整RTT并除以
2估算单次交接，同时保留未经除法处理的RTT原始数据。

#### I5：同步Endpoint往返（扩展）

只有进程化推理架构启用：

```text
USB进程 CALL(输入元数据)
→ 推理进程 RECV
→ REPLY_RECV
→ USB进程返回
```

分别测试0和4个机器字、同核和跨核。

## 5. 正式比较项

核心表只保留真正可对齐的原语，不把同一测量因“收、发、推理”重复计权：

```text
notification_wait_pending
notification_wake_same_core
notification_wake_cross_core
thread_handoff_same_core
high_priority_preemption_same_core
high_priority_preemption_cross_core
irq_to_user
irq_ack
```

Endpoint RTT作为当前架构的扩展结果。MMIO、DMA、Frame和VSpace属于初始化或
资源管理，不纳入本实验。

## 6. 测量方法

### 6.1 平台

正式结果必须在同一块物理硬件上产生，目标为Raspberry Pi 5：

```text
同一CPU频率
同一CPU亲和性
同一GIC路由
相同优化等级
相同消息大小
相同USB设备和传输大小（真实IRQ项目）
```

QEMU只验证状态机和日志格式，不作为性能结论。

### 6.2 周期计数器

主指标使用`PMCCNTR_EL0`真实CPU cycle：

```text
compiler fence → ISB → read PMCCNTR_EL0
→ 被测路径
→ ISB → read PMCCNTR_EL0 → compiler fence
```

- seL4使用`libsel4bench::sel4bench_get_cycle_count()`。
- Exokernel增加benchmark-only PMU开放和EL0读取函数。
- 当前`runtime::counter()`读取`CNTVCT_EL0`，只能作为时间指标，不能标成CPU周期。

### 6.3 样本

- 无阻塞短原语：warm-up 10,000次，批量1,000,000次为一轮，共30轮。
- 阻塞、切换和IRQ：至少100,000个单次样本。
- 计时区间内禁止UART日志、分配和文件访问。
- 预先分配样本数组，完成后统一输出。
- 保留全部离群值，同时标注温度、抢占和异常中断情况。

每项报告：

```text
min / median / mean / p95 / p99 / max / standard deviation
```

平均性能使用median/mean，机器人实时性重点使用p99和max。

## 7. 输出格式

```text
system,commit,phase,primitive,variant,cpu_from,cpu_to,
iterations,sample,total_cycles,cycles_per_op,timer_ticks,temperature_c,valid
```

其中`phase`取：

```text
receive / send / inference
```

同一个底层测量可关联多个phase，但汇总几何平均只能计入一次。

## 8. 实施顺序

1. 为Exokernel增加`app-kernel-bench`、PMU读取和无日志结果缓冲区。
2. 实现Notification pending、同核唤醒、跨核唤醒和线程交接。
3. 在seL4工程中增加对应app并复用`libsel4bench`。
4. 实现高优先级抢占测试。
5. 接入真实xHCI IRQ并区分Bulk IN、Bulk OUT完成路径。
6. 统一CSV和统计脚本，在QEMU验证后转到共同真机。
7. 进程化推理完成后增加Endpoint扩展项。

## 9. 验收条件

- 核心比较只有Exokernel与seL4，没有Linux近似原语。
- 收、发、推理三部分都能追溯到实际机器人运行路径。
- 明确证明Bulk IN/OUT提交不进入Kernel，完成路径才使用Notification和IRQ ACK。
- ACT前向不被错误描述为系统调用密集型路径。
- 同核、跨核、平均延迟和尾延迟分别报告。
- 每个结果可追溯到原始CSV、Git commit、构建参数和硬件状态。
