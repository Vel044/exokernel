# USB驱动资源与运行流程

## 1. 总体流程

当前外核中的USB驱动遵循以下资源使用流程：

```text
Kernel发现并保护硬件资源（MMIO IRQ、内存）
→ libOS申请MMIO、IRQ、Notification和DMA
→ Kernel验证授权并建立映射或绑定
→ libOS映射后直接驱动硬件
```

Kernel负责资源发现、授权、隔离、映射和回收，不实现PCI、xHCI、USB、CDC ACM
或SCServo协议。libOS取得资源后直接访问xHCI寄存器和DMA内存，不需要为每次
USB操作进入Kernel。

## 2. USB使用的Kernel资源

| 资源           | 首次申请                         | 后续使用方式                                 |
| -------------- | -------------------------------- | -------------------------------------------- |
| MMIO           | `SYS_MAP_MMIO`                   | libOS直接读写xHCI寄存器                      |
| DMA            | `SYS_DMA_ALLOC`                  | CPU填写Ring和Buffer，xHCI通过DMA address读取 |
| Notification   | `SYS_NOTIFICATION_CREATE`        | USB线程没有完成事件时阻塞等待                |
| IRQ            | `SYS_IRQ_BIND`                   | xHCI中断到达后触发Notification并唤醒USB线程  |
| Thread/Priority | 创建固定在CPU2、优先级48的USB线程 | Kernel保存上下文、计时并执行抢占调度       |

其中，Thread和Priority不是驱动xHCI寄存器的必要资源，但在控制线程、推理线程
和USB线程并发运行时需要它们。

## 3. 资源建立

实现位置：`libos/src/drivers/xhci.rs`

```text
发现xHCI资源
→ MAP_MMIO
→ 创建Notification
→ IRQ_BIND
→ 创建CrabUSB Host
```

### 3.1 发现xHCI资源

QEMU和Pi5使用不同的发现路径：

```text
QEMU：
UserBootInfo.pci
→ 映射PCI ECAM
→ libOS的PCI驱动扫描PCI
→ 找到xHCI BAR和INTx

Pi5：
UserBootInfo.xhci
→ 直接取得RP1 xHCI MMIO和INTID
```

发现结果至少包含：

```text
xHCI MMIO物理地址
xHCI MMIO大小
xHCI中断INTID
```

### 3.2 映射xHCI MMIO

libOS调用：

```text
SYS_MAP_MMIO(xhci_pa, xhci_size, XHCI_VA)
```

Kernel检查：

- 物理地址范围是否属于当前任务的MMIO grant；
- 是否试图映射GIC、Kernel内存或其他未授权设备；
- EL0 VA是否对齐、越界或与现有映射重叠。

成功后建立：

```text
EL0 XHCI_VA
→ xHCI BAR物理地址
→ Device-nGnRE内存属性
```

此后CrabUSB通过`XHCI_VA`直接执行`volatile`寄存器访问。每次寄存器读写不再
调用系统调用。

### 3.3 创建Notification并绑定IRQ

libOS先创建Notification：

```text
SYS_NOTIFICATION_CREATE
→ NotificationHandle
```

随后把已经授权的xHCI中断绑定到Notification：

```text
SYS_IRQ_BIND(xhci_intid, notification_handle, badge)
```
Kernel验证INTID授权、Notification所有权和badge，然后配置GIC并记录：

### 3.4 创建CrabUSB Host

## 4. 中断完成一次USB请求

实现位置：`libos/src/runtime/usb_executor.rs`

USB Future的执行流程为：

```text
poll USB Future
→ Future返回Pending
→ Notification WAIT
→ 当前USB线程进入Blocked
→ xHCI产生中断
→ GIC进入Kernel IRQ handler
→ Kernel触发Notification
→ Kernel把USB线程改为Ready
→ Scheduler重新运行USB线程
→ CrabUSB handle_event处理Event Ring
→ IRQ ACK
→ 再次poll USB Future
→ Future返回Ready或继续等待
```


## 6. 哪些操作进入Kernel

USB控制面初始化：
MMIO + DMA + Notification CREATE + IRQ BIND系统调用

USB数据面稳定运行：
直接操作MMIO/DMA
+ Notification WAIT
+ IRQ ACK
