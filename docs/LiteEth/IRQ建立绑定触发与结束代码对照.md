# 从 LiteEth 看 IRQ：建立、绑定、触发与结束

控制器部分以 Linux 6.12.75 的 ARM GICv2 路径对照当前 Exokernel；其他平台使用对应 irqchip。以下代码省略无关语句。

## ① 建立：从 liteeth_probe 开始

### 1. Linux：取得 IRQ 编号

Linux [liteeth_probe](../../../linux/drivers/net/ethernet/litex/litex_liteeth.c#L264)：

```c
irq = platform_get_irq(pdev, 0); // 取第 0 个中断
if (irq < 0)
    return irq;
netdev->irq = irq;              // 保存 Linux IRQ 编号
```

设备树路径往下调用：

```text
liteeth_probe
  → platform_get_irq → platform_get_irq_optional   drivers/base/platform.c
  → of_irq_get                                    drivers/of/irq.c
  → irq_create_of_mapping → IRQ domain            kernel/irq/irqdomain.c
  → 返回 Linux IRQ 编号
```

[platform_get_irq](../../../linux/drivers/base/platform.c#L260) 可能创建或取得硬件 IRQ 到 Linux IRQ 的映射，**返回的 Linux IRQ 不保证等于 GIC INTID**。此时还没有注册 LiteEth 回调。

### 2. Rust：取得已解析的 GIC INTID

[liteeth_probe](../../libos/src/drivers/liteeth.rs)：

```rust
let irq = platform_get_irq(pdev, 0);
if irq < 0 { return irq; }
(*netdev).irq = irq as u32;
```

`pdev.irqs` 来自 EL0 设备树解析。这里只取编号；Notification 移到 `request_irq` 内创建。Linux 取得虚拟 IRQ；本内核使用 GIC INTID。

## ② 绑定：从 liteeth_open 开始

### 1. Linux：注册 liteeth_interrupt

Linux [liteeth_open](../../../linux/drivers/net/ethernet/litex/litex_liteeth.c#L122)：

```c
litex_write8(priv->base + LITEETH_WRITER_EV_PENDING, 1); // 清旧 RX 事件
litex_write8(priv->base + LITEETH_READER_EV_PENDING, 1); // 清旧 TX 事件

err = request_irq(netdev->irq, liteeth_interrupt, 0, netdev->name, netdev);
if (err)
    return err;

litex_write8(priv->base + LITEETH_WRITER_EV_ENABLE, 1); // 开 RX 事件中断
litex_write8(priv->base + LITEETH_READER_EV_ENABLE, 1); // 开 TX 事件中断
```

`request_irq` 五个参数：IRQ 编号、回调、flags、名称、回调上下文。最后的 `netdev` 会作为 `dev_id` 传给 `liteeth_interrupt`。

```text
liteeth_open
  → request_irq                    include/linux/interrupt.h
  → request_threaded_irq           kernel/irq/manage.c
  → __setup_irq                    登记回调、检查冲突并启动 IRQ
```

[request_threaded_irq](../../../linux/kernel/irq/manage.c#L2148) 分配 `irqaction`，保存回调信息后交给 `__setup_irq`：

```c
action->handler = handler;     // liteeth_interrupt
action->thread_fn = thread_fn; // request_irq 此处传入 NULL
action->flags = irqflags;
action->name = devname;
action->dev_id = dev_id;       // netdev
retval = __setup_irq(irq, desc, action);
```

LiteEth 传入 flags=0，未申请共享 IRQ。注册失败就返回错误，设备事件中断不会被启用。

### 2. Rust：保留五个参数和原回调

[liteeth_open](../../libos/src/drivers/liteeth.rs)：

```rust
let err = request_irq((*netdev).irq, liteeth_interrupt, 0,
                      (*netdev).name.as_ptr(), netdev.cast());
if err != 0 { /* 原日志 */ return err; }
```

[interrupt.rs](../../libos/src/linux_api/interrupt.rs) 保存 `handler/flags/name/dev`。回调仍是 `liteeth_interrupt(irq, dev_id) -> irqreturn_t`，`dev_id` 就是注册时的 `netdev`。

```text
request_irq
  → NOTIFICATION_CREATE(28)                    建硬件事件通知
  → IRQ_BIND(7)：INTID、Notification、badge=1  EL1 校验 grant/owner 并建立路由
  → 建处理完成通知
  → THREAD_CREATE(18)                         在绑定 CPU 创建 EL0 IRQ 服务线程
```

成功后原驱动才打开 RX/TX 设备事件。回调指针留在 EL0；EL1 只保存 IRQ→Notification 路由。LiteEth 传 `flags=0`；其他 flags 当前明确返回不支持。

## ③ 触发：从硬件事件到 liteeth_interrupt

### 1. Linux：IRQ 框架调用驱动回调

`liteeth_open` 启用事件后，LiteEth 收包或完成发送会触发 IRQ：

```text
LiteEth 设置设备 pending → GIC → CPU IRQ 异常
  → gic_handle_irq                          读 IAR，取得硬件 INTID
  → generic_handle_domain_irq               找到 Linux IRQ / irq_desc
  → handle_fasteoi_irq → handle_irq_event
  → handle_irq_event_percpu → __handle_irq_event_percpu
  → action->handler(irq, action->dev_id)
  → liteeth_interrupt(irq, netdev)
```

控制器入口在 [irq-gic.c](../../../linux/drivers/irqchip/irq-gic.c#L334)。最终在 [kernel/irq/handle.c](../../../linux/kernel/irq/handle.c#L139) 调用：

```c
res = action->handler(irq, action->dev_id); // 进入 liteeth_interrupt
```

普通未强制线程化的配置下，回调在内核硬中断上下文执行；`threadirqs` / PREEMPT_RT 等配置可能改为 IRQ 线程执行。应用不需要先调用 read 才触发驱动接收。

### 2. Rust：服务线程自动调用，不等应用 read

```text
设备 pending → GIC → EL1 IRQ 异常
  → 屏蔽该 SPI，执行 GIC EOI/DIR，投递硬件 Notification
  → IRQ 服务线程的 NOTIFICATION_WAIT(30) 返回
  → 取得设备锁，调用 liteeth_interrupt(irq, 原 dev_id)
      → 原顺序：TX pending / wake queue / W1C，再 RX / memcpy_fromio / W1C
  → IRQ_ACK(9)                                EL1 重新开放 SPI
  → 网络核心推进被唤醒的积压发送
  → SIGNAL(29)                               通知应用处理完成
```

[exo.rs::irq_worker](../../libos/src/linux_api/exo.rs) 循环执行 [interrupt::dispatch](../../libos/src/linux_api/interrupt.rs)。应用的 `Driver::wait_irq` 只等待完成通知，不负责调用硬件回调。

Linux 回调运行于内核 IRQ 上下文；这里运行于 EL0 服务线程。设备锁避免回调和应用同时修改普通统计/报文队列；应用阻塞等待通知时不持锁。

## ④ 结束：从 liteeth_interrupt 返回，再到 liteeth_stop 解绑

### 1. 两边都先处理设备事件、清 pending

Linux [liteeth_interrupt](../../../linux/drivers/net/ethernet/litex/litex_liteeth.c#L94) 的 RX 部分：

```c
reg = litex_read8(priv->base + LITEETH_WRITER_EV_PENDING);
if (reg) {
    liteeth_rx(netdev); // 先从设备 SRAM 取帧
    litex_write8(priv->base + LITEETH_WRITER_EV_PENDING, reg); // 写 1 清事件
}
return IRQ_HANDLED;
```

Rust [liteeth_interrupt](../../libos/src/drivers/liteeth.rs) 对应：

```rust
let reg = litex_read8(&self.base, LITEETH_WRITER_EV_PENDING);
if reg != 0 {
    self.liteeth_rx();
    litex_write8(&self.base, LITEETH_WRITER_EV_PENDING, reg);
}
```

两边都在 RX 之前处理 TX pending：需要时 `netif_wake_queue` 恢复发送队列，再写 1 清 TX 事件。`IRQ_HANDLED` 是 Linux 的回调返回值；Rust 回调返回 `()`，没有共享 IRQ 的返回协议。

### 2. Linux：返回 IRQ 框架，由 irqchip 完成控制器处理

```text
liteeth_interrupt 返回 IRQ_HANDLED
  → 返回 handle_irq_event / handle_fasteoi_irq
  → cond_unmask_eoi_irq
  → chip->irq_eoi
  → GIC 驱动完成 EOI/deactivate
```

见 [handle_fasteoi_irq](../../../linux/kernel/irq/chip.c#L687) 和 [GIC 的 irq_eoi 实现](../../../linux/drivers/irqchip/irq-gic.c#L218)。普通模式在此写 EOIR；分离 deactivate 的模式在中断入口先写 EOIR，回调后再写 DIR。具体顺序由 irqchip 和配置决定，LiteEth 不执行用户态 ACK 系统调用。

### 3. Rust：先关设备，再同步解除注册

[liteeth_stop](../../libos/src/drivers/liteeth.rs) 保留原顺序：

```text
netif_stop_queue → netif_carrier_off
  → 关闭 RX / TX EV_ENABLE
  → free_irq(irq, netdev)
```

[free_irq](../../libos/src/linux_api/interrupt.rs) 的返回值与 Linux 一样是注册时的名称指针：

```text
移除相同 irq/dev_id 的 action，禁止新回调
  → IRQ_UNBIND(10)                            EL1 关路由并清状态
  → SIGNAL(29, badge=2)                       取消服务线程尚未结束的等待
  → 等在途 callback 返回                     不把设备提前释放
  → 服务线程 THREAD_EXIT(19) 自动回收；旧 handle 失效后确认退出
  → NOTIFICATION_DESTROY(32)                  销毁硬件通知和完成通知
  → 释放 EL0 注册记录，返回原 name 指针
```

线程退出同步使用 `THREAD_RUNTIME(33)` 查询旧句柄，等待时 `THREAD_YIELD(20)`。具体编号以 [abi/src/lib.rs](../../abi/src/lib.rs) 为准。

Linux 通过 `synchronize_irq` 等待内核在途处理；这里同步服务线程和回调。两边都要求 `free_irq` 返回后才能释放回调使用的设备状态；不能在自身回调里调用它。
