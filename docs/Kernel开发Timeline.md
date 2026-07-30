# Exokernel Kernel 开发 Timeline

## 当前运行模型

```text
一个EL0任务 / 一个共享VSpace
    + QEMU或Pi5的4个CPU
    + 最多16个固定CPU亲和性的EL0线程
    + 64级静态优先级抢占调度
    + 同优先级线程1ms FIFO轮转
    + Endpoint同步IPC与Notification异步事件
```

Kernel负责资源保护、内存映射、中断投递、线程调度和退出回收；libOS负责
PCI、xHCI、USB、CDC ACM、SCServo及机器人任务策略。MMIO和DMA完成授权与
映射后由libOS直接访问，普通设备操作不进入Kernel。

## 已完成

| 阶段 | Kernel能力 | 验收内容 |
| --- | --- | --- |
| 1. Frame + VSpace | 连续页、Handle、部分映射、W^X、TLBI、回收 | 零初始化、直接读写、冲突与过期Handle通过 |
| 2. Thread | 独立上下文、栈、IPC Buffer、固定CPU亲和性 | 创建、退出、错误CPU与旧Handle检查通过 |
| 3. Endpoint + Reply | Send/Recv、Call/Reply、ReplyRecv | 消息复制及Call优先级继承通过 |
| 4. Notification + IRQ | badge合并、等待、IRQ绑定与唤醒 | 异步通知、跨核唤醒和xHCI IRQ通过 |
| 5. SMP与调度 | PSCI四核、64级优先级、1ms同级轮转 | 四核上线、抢占、轮转和亲和性通过 |
| 6. SMP内存安全 | Kernel自旋锁、远程重调度、TLB shootdown、停止SGI | 并发资源表、跨核失效和退出同步通过 |
| 7. 机器人组合 | Main、Control、USB、Inference固定分核 | IPC、Timer PPI、xHCI INTx并行通过 |

## 统一测试入口

`system-smoke`内部按职责拆分：

```text
libos/src/apps/system_smoke/frame.rs
    Frame + VSpace
libos/src/apps/system_smoke/ipc.rs
    Thread + Endpoint + Notification
libos/src/apps/system_smoke/priority.rs
    CPU亲和性、同级轮转、抢占和优先级继承
libos/src/apps/system_smoke/robot.rs
    Main + Control + USB + Inference组合
```

无USB快速回归：

```bash
LIBOS_FEATURES=system-smoke QEMU_USB_MODE=none bash qemu/run.sh
```

完整xHCI/FTDI并发回归：

```bash
LIBOS_FEATURES=qemu-xhci,usb-echo,system-smoke \
QEMU_USB_MODE=ftdi \
bash qemu/run.sh
```

单独运行模拟FTDI：

```bash
LIBOS_FEATURES=qemu-xhci,usb-echo \
QEMU_USB_MODE=ftdi \
bash qemu/run.sh
```

SCServo串口桥默认只PING和读取，不移动舵机：

```bash
LIBOS_FEATURES=qemu-xhci,scservo \
QEMU_USB_MODE=serial-bridge \
QEMU_SERIAL_PATH=/dev/cu.usbmodem5A7C1191771 \
bash qemu/run.sh
```

## 当前验收状态

2026-07-27已完成：

- QEMU四个不同MPIDR通过PSCI上线，online mask为`0xf`。
- `system-smoke + none`通过Frame、IPC、Notification、同级轮转、高优先级
  抢占、Endpoint优先级继承、CPU亲和性和机器人布局测试。
- `qemu-xhci + usb-echo`完成PCI xHCI初始化及模拟FTDI枚举，USB线程固定CPU2。
- 任务退出使用SGI停止其他核，再回收线程、Frame、DMA、MMIO、IRQ和页表。

Pi5已经具备共用PSCI/GIC/Timer代码和交叉编译路径；四核、RP1 xHCI和组合负载
仍需真机串口验收。

## 下一步

1. 在Pi5验证四个Cortex-A76、每核Timer PPI和RP1 xHCI IRQ。
2. 增加长期压力统计：IRQ次数、线程runtime ticks、IPC次数和剩余物理页。
3. 给全局对象锁建立锁顺序，并测量最长临界区和IRQ关闭时间。
4. 为实时性补充任务周期、WCET、最坏IRQ延迟和优先级反转分析。
5. 出现实际隔离需求后再实现多VSpace、跨任务IPC和DeviceHandle。

## 架构边界

- IPC用于传递命令、状态和所有权，不是访问MMIO、DMA或设备Ring的必经路径。
- Kernel不提供`USB_TRANSFER`、`SERIAL_WRITE`或`MOTOR_MOVE`等设备协议接口。
- libOS选择线程CPU和静态优先级；Kernel验证控制权限并强制执行抢占调度。
- Kernel只在创建、映射、阻塞、唤醒、抢占和回收等保护边界介入。
