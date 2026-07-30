# Xok、seL4 调度机制对比

## 1. Xok：可分配的 CPU Quantum Vector（固定长度时间片轮转）

### 1.1 核心模型

Xok 把每个 CPU 的执行时间表示为一个固定长度的 Quantum Vector。Environment
只有获得并绑定 Quantum，才能被 Kernel 调度：

```text
CPU Quantum Vector

0    1    2    3                         127
特殊  A    B    A    ...                  C
```
特殊Quantum一轮结束的边界，触发more_ticks()补充预算；
没有可运行Environment时，Kernel另外运行env0 idle Environment。

一个 Environment 可以拥有多个 Quantum，因此用户态 ExOS 可以通过重新分配
Quantum决定CPU份额。Kernel不理解“控制线程”“推理线程”等策略，只保护
Quantum capability并执行时间回收。

源码位置：

- [`sys/xok/scheduler.h`](../../mit-exokernel/sys/xok/scheduler.h)
- [`sys/kern/sched.c`](../../mit-exokernel/sys/kern/sched.c)

### 1.2 Quantum申请与绑定

Xok的普通系统调用表在
[`sys/conf/syscall.conf`](../../mit-exokernel/sys/conf/syscall.conf)中定义。
调度资源相关接口如下：

| 用户态接口                            | 主要参数                                            | Kernel语义                                                                            |
| ------------------------------------- | --------------------------------------------------- | ------------------------------------------------------------------------------------- |
| `sys_quantum_alloc(k, q, cpu, envid)` | capability槽`k`、指定槽或`-1`、CPU、目标Environment | 分配Quantum，写入capability，补充`BASE_TICKS`并绑定目标Environment；返回Quantum编号。 |
| `sys_quantum_set(k, q, cpu, envid)`   | capability槽、Quantum编号、CPU、目标Environment     | 校验调用者对Quantum的权限，把该Quantum从旧Environment转给新Environment。              |
| `sys_quantum_free(k, q, cpu)`         | capability槽、Quantum编号、CPU                      | 校验权限，解除绑定、清空剩余tick并归还空闲Quantum链表。                               |
| `sys_quantum_get(cpu)`                | CPU编号                                             | 返回该CPU当前正在运行的Quantum编号。                                                  |

这里的`k`不是Quantum编号，而是调用Environment中保存授权capability的槽位；
`q`才是Quantum Vector中的槽号。`envid == 0`表示分配Quantum但暂不绑定普通
Environment。

`sys_quantum_set()`首先检查调用者的capability，再修改Quantum。

因此，Xok把“CPU时间分给谁”的策略交给用户态，但Kernel仍然负责授权、计时、
抢占和回收。

### 1.4 时间片流转

`sched_runnext()`按照Quantum编号循环扫描：

```c
curq = (curq + 1) & QUANTUM_MASK;
```

未分配、没有剩余tick或Sleeping的项会被跳过。扫描完整个Vector后，
`more_ticks()`为已分配Quantum补充tick。

时钟中断进入`sched_intr()`：

```text
Timer IRQ
→ 当前Quantum的q_ticks减1
→ q_ticks耗尽
→ revoke_processor()
→ sched_runnext()
→ 运行下一个可运行Quantum
```

### 1.5 调度性质

Xok的特点是：

- CPU时间是可授权、可转移的低级资源。
- ExOS决定一个Environment获得多少Quantum。
- Kernel按Vector顺序流转并强制回收CPU。
- Sleeping项被跳过，CPU不会因为空Quantum而按比例闲置。
- 多个Quantum主要表达CPU份额，不表达“唤醒后必须立即运行”的静态优先级。

所以Xok很符合外核思想，但原始Quantum Vector本身不是强实时固定优先级调度器。

## 2. seL4：固定优先级抢占与同优先级轮转

### 2.1 核心模型

seL4为每个TCB保存调度属性，其中包括：

```text
tcbPriority   当前静态优先级
tcbMCP        允许该线程控制的最高优先级上限
tcbTimeSlice  同优先级线程的剩余时间片
tcbAffinity   绑定的CPU
```

Kernel按照优先级维护Ready Queue和bitmap。`chooseThread()`先通过bitmap找到
最高的非空优先级，再选择该优先级队列的队首线程：

```text
找到最高非空优先级
→ 取该优先级Ready Queue队首
→ switchToThread()
```

源码位置：

- [`kernel/src/kernel/thread.c`](../../sel4test-qemu-arm-virt/kernel/src/kernel/thread.c)
- [`kernel/include/object/structures.h`](../../sel4test-qemu-arm-virt/kernel/include/object/structures.h)

### 2.2 用户态调度调用

seL4用户看到的不是Linux风格的`SYS_sched_*`编号，而是对capability的
**invocation**。`libsel4`把下面的C函数编码为IPC message，再执行架构相关的
系统调用指令进入Kernel；安全性由目标TCB、SchedControl或SchedContext
capability决定。

以设置优先级为例，调用链是：

```text
seL4_TCB_SetPriority(target_tcb_cap, authority_tcb_cap, priority)
→ libsel4 stub构造TCBSetPriority invocation
→ seL4_Call()执行AArch64 svc
→ Kernel decodeInvocation()
→ decodeTCBInvocation()
→ 检查TCB capability和authority.tcbMCP
→ setPriority()
→ 必要时possibleSwitchTo()/rescheduleRequired()
```

所以，除了`seL4_Yield()`是一个直接的基础系统调用外，设置优先级等操作表现为
对象invocation；它们最终仍会通过系统调用进入Kernel，并不是普通用户态函数。

经典seL4与两种配置共有或常用的TCB调用如下：

| libsel4调用                                      | Kernel语义                                                                      |
| ------------------------------------------------ | ------------------------------------------------------------------------------- |
| `seL4_TCB_SetPriority(tcb, authority, priority)` | 使用`authority` TCB的MCP授权修改目标TCB优先级；新优先级不得超过authority的MCP。 |
| `seL4_TCB_SetMCPriority(tcb, authority, mcp)`    | 修改目标TCB的Maximum Controlled Priority，同样受authority的MCP限制。            |
| `seL4_TCB_Suspend(tcb)`                          | 让目标线程停止参与调度；如果目标正在运行则请求重新调度。                        |
| `seL4_TCB_Resume(tcb)`                           | 让被Suspend的目标线程重新可运行，并在必要时触发抢占。                           |
| `seL4_Yield()`                                   | 当前线程主动放弃剩余时间片；经典配置中把机会让给同优先级线程。                  |
| `seL4_TCB_SetAffinity(tcb, cpu)`                 | SMP经典配置中把线程迁移或绑定到指定CPU。                                        |

- [`kernel/libsel4/include/interfaces/object-api.xml`](../../sel4test-qemu-arm-virt/kernel/libsel4/include/interfaces/object-api.xml)
- [`kernel/libsel4/include/sel4/syscalls_master.h`](../../sel4test-qemu-arm-virt/kernel/libsel4/include/sel4/syscalls_master.h)

这些调用只负责配置和改变调度状态。线程平时运行不需要持续调用Kernel；只有发生
系统调用、IRQ、fault、阻塞、唤醒、主动yield或timer到期时才进入调度路径。

### 2.3 抢占规则

Endpoint、Notification或其他事件使线程重新可运行时，会调用
`possibleSwitchTo()`。在本次Kernel entry结束前，`schedule()`比较候选线程和
当前线程：

```text
高优先级线程被唤醒
→ possibleSwitchTo(高优先级线程)
→ schedule()
→ 当前低优先级线程进入Ready Queue
→ 立即切换到高优先级线程
```

因此，seL4不是等当前低优先级线程主动`yield`后才运行高优先级线程。
IRQ触发Notification时，同样会把等待线程变为Runnable并请求重新调度。

### 2.4 同优先级时间片

经典seL4配置的`timerTick()`递减`tcbTimeSlice`。时间片耗尽后，当前线程被追加
到同优先级Ready Queue尾部：

```text
同优先级A运行
→ 时间片耗尽
→ A放到该优先级队尾
→ 同优先级B运行
```

所以seL4的基本顺序是：

```text
先比较静态优先级
→ 最高优先级Ready线程运行
→ 只有优先级相同时才按时间片轮转
```

用户态通过TCB capability设置Priority和MCP。MCS配置还增加SchedContext，
可以表达budget、period和refill；但固定优先级仍决定可运行线程之间的先后顺序。

### 2.5 调度性质

seL4的特点是：

- 高优先级Ready线程可以抢占低优先级线程。
- 同优先级线程使用时间片轮转。
- MCP限制线程能够设置的最高优先级，避免无权线程提升到系统最高级。
- Notification和IPC唤醒会进入统一的优先级调度路径。
- MCS可进一步限制线程的执行预算和补充周期。

它比单纯按Quantum槽流转更适合实时任务，但硬实时保证仍需要WCET、IRQ延迟、
临界区长度和任务周期等分析。

## 3. 当前系统：四核静态优先级抢占

### 3.1 SMP与线程属性

CPU0从DTB取得最多4个MPIDR，通过PSCI SMC `CPU_ON`拉起CPU1..3。每个线程固定
绑定一个CPU，记录：

```text
affinity_cpu
base_priority
effective_priority
max_control_priority
running_cpu
runtime_ticks
```

源码位置：

- [`kernel/src/arch/aarch64/smp.rs`](../kernel/src/arch/aarch64/smp.rs)
- [`kernel/src/scheduler/priority.rs`](../kernel/src/scheduler/priority.rs)
- [`kernel/src/scheduler/timer.rs`](../kernel/src/scheduler/timer.rs)
- [`kernel/src/object/thread.rs`](../kernel/src/object/thread.rs)

### 3.2 调度规则

每核有64级Ready Queue，优先级取值`0..63`，数值越大越高：

```text
唤醒线程
→ 加入其affinity_cpu的有效优先级FIFO队列
→ 若远程核正在运行更低优先级线程，发送SGI0
→ 目标核立即重新调度最高优先级Ready线程
```

同一优先级有多个Ready线程时，Generic Timer每1ms把当前线程移到FIFO队尾。
如果同级只有一个Ready线程，它继续运行，不会因为空队列浪费CPU。

### 3.3 libOS策略接口

```text
THREAD_CREATE(entry, arg, cpu, priority, max_control_priority)
THREAD_SET_PRIORITY(thread, priority)
THREAD_YIELD()
THREAD_EXIT(code)
THREAD_RUNTIME(thread)
```

libOS决定机器人线程的CPU与基础优先级；Kernel检查CPU范围、`0..63`和MCP，
并负责强制抢占。默认布局为：

```text
CPU0 Main协调线程       priority 32
CPU1 Control线程        priority 56
CPU2 USB/xHCI线程       priority 48
CPU3 Inference线程      priority 16
```

### 3.4 IPC优先级继承

Endpoint Call建立`caller -> server`关系。高优先级Caller等待低优先级Server时，
Kernel把优先级沿最多16个线程的调用链传递给Server；Reply、取消或退出后恢复。
Notification和单向Send只负责唤醒，不建立长期继承。

### 3.5 与Xok和seL4的关系

- 保留外核边界：libOS决定线程布局和设备策略，Kernel只保护并执行机制。
- 不再采用Xok Quantum Vector分配CPU份额，因为它不能表达关键线程的立即响应。
- 调度语义接近seL4的静态优先级抢占、MCP和同级轮转，但对象与ABI更精简。

## 4. 实时性边界

当前实现解决了固定优先级抢占和基本优先级反转，但“高优先级”本身不等于硬实时。
还需要测量或证明WCET、最长Kernel临界区、IRQ响应、跨核SGI/TLB失效和阻塞上界，
才能给出最坏响应时间保证。
