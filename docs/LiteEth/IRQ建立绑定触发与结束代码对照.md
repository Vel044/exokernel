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

### 2. Rust：取得 INTID，创建 Notification

Rust [liteeth_probe](../../libos/src/drivers/liteeth.rs#L240)：

```rust
let irqs = [info.mac.intid]; // 来自 EL0 解析的 DTB，保存到 pdev.irqs
let irq = platform_get_irq(&pdev, 0)?;
let irq_registration = IrqRegistration::new(ExoIrq::new()?);
```

[platform_get_irq](../../libos/src/linux_api/platform.rs#L37) 从已经解析好的数组取值：

```rust
dev.irqs.get(index).copied().ok_or(ENXIO)
```

[ExoIrq::new](../../libos/src/linux_api/exo.rs#L59) → [Notification::create](../../libos/src/kernel_api/notification.rs#L25)：

```rust
crate::runtime::svc(exo_abi::SYS_NOTIFICATION_CREATE, 0, 0, 0)
```

`SYS_NOTIFICATION_CREATE(28)` 进入 EL1 的 [notification_create](../../kernel/src/object/ipc.rs#L487)，创建属于当前任务的通知对象，返回 handle。`IrqRegistration` 保存它，初始 `binding=None`；此时还没有绑定或启用 GIC IRQ。

**Linux 使用 IRQ domain 管理编号；当前 Rust 直接保存 GIC INTID，并额外创建接收中断通知的 Notification。**

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

### 2. Rust：EL0 保存回调，EL1 绑定 Notification

Rust [liteeth_open](../../libos/src/drivers/liteeth.rs#L141)：

```rust
self.r#priv.prepare_open()?; // 清 RX/TX 旧 pending
request_irq(
    &mut self.irq_registration, // 注册对象，内部持有 Notification
    self.irq,                  // GIC INTID
    liteeth::liteeth_interrupt, // EL0 回调
    0,                         // flags
    DRV_NAME,
)?;
self.r#priv.enable();           // 成功后写 RX/TX EV_ENABLE=1
```

[linux_api::request_irq](../../libos/src/linux_api/interrupt.rs#L41) 检查尚未绑定、flags=0，再执行：

```rust
registration.backend.bind(irq)?;                     // 绑定失败立即返回
registration.binding = Some((irq, handler, name));  // EL0 保存函数指针
```

`backend.bind` 实际进入 [ExoIrq::bind](../../libos/src/linux_api/exo.rs#L66)：

```rust
self.notification.bind_irq(irq, 1).map_err(|_| -5) // badge=1 表示这个 IRQ 事件
```

往下调用：

```text
liteeth_open → request_irq
  → ExoIrq::bind → Notification::bind_irq
  → runtime::irq_bind_notification
  → SYS_IRQ_BIND(7)：INTID、Notification handle、badge=1、目标当前 CPU
  → EL1 sys_irq_bind
    → task::bind_irq：登记 INTID / handle / badge
    → gic::configure_spi：设置触发类型及目标 CPU
    → gic::enable_spi：开放这根 IRQ
```

[runtime::irq_bind_notification](../../libos/src/runtime/mod.rs#L344) 执行系统调用；[sys_irq_bind](../../kernel/src/syscall/dispatch.rs#L605) 校验 SPI 编号、IRQ grant、通知对象所有者、badge、目标 CPU 和重复绑定。触发类型取自内核的 grant flags，成功返回 0；其他 IRQ flags 当前在 EL0 返回不支持。

**函数指针留在 EL0；EL1 只保存 INTID 与 Notification 的关系。** Linux 注册时保存 `dev_id=netdev`；Rust 在派发时才通过 `&mut self.r#priv` 传入驱动状态。

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

### 2. Rust：EL1 通知，EL0 wait_irq 派发

应用在 [liteeth_smoke.rs](../../libos/src/apps/liteeth_smoke.rs#L92) 中调用：

```rust
driver.wait_irq().unwrap_or_else(|e| fail(b"IRQ", e)); // 等通知并执行驱动回调
let n = driver.receive(&mut frame);                  // 从已收好的本地队列取帧
```

从 [Driver::wait_irq](../../libos/src/drivers/liteeth.rs#L412) 进入 [wait_and_dispatch](../../libos/src/linux_api/interrupt.rs#L30)：

```rust
let (irq, handler, _) = self.binding.ok_or(EINVAL)?;
if self.backend.wait()? { // ExoIrq → Notification::wait → SYS_NOTIFICATION_WAIT(30)
    handler(context);    // liteeth_interrupt(&mut self.r#priv)
    self.backend.ack(irq)?;
}
```

[Notification::wait](../../libos/src/kernel_api/notification.rs#L57) 把 handle 传给内核。有 pending badge 就取走并返回；没有则阻塞当前线程。设备中断到达时，[EL1 IRQ 入口](../../kernel/src/syscall/dispatch.rs#L270) 按顺序执行：

```rust
crate::gic::disable_spi(intid);                       // 暂时屏蔽这根 IRQ
crate::gic::write_eoir(iar_token);                    // 写 GIC EOIR / DIR
crate::task::record_irq_pending(intid);               // 等待用户态 ACK
let _ = crate::ipc::notification_signal(notification, badge); // 投递通知、唤醒等待线程
```

```text
LiteEth → GIC → EL1 IRQ 入口
  → 屏蔽 SPI → EOI/deactivate → 投递 Notification
  → EL0 的 WAIT 返回 badge
  → wait_and_dispatch 调用 liteeth_interrupt
```

中断先到时，badge 会保留到 WAIT 取走；多个 badge 按位合并，不是事件次数计数。这里的 `notification_signal` 是 EL1 内部调用，不经过 `SYS_NOTIFICATION_SIGNAL`。

**Rust 必须由所属 EL0 线程调用 `wait_irq` 才执行回调。** 内核只处理 GIC 和通知，不处理 LiteEth 寄存器或数据包。

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

Rust [liteeth_interrupt](../../libos/src/drivers/liteeth.rs#L121) 对应：

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

### 3. Rust：回调返回后，通过 ACK 重新开放 SPI

[wait_and_dispatch](../../libos/src/linux_api/interrupt.rs#L30) 在 `handler(context)` 返回后调用 `ExoIrq::ack`。后者在 [exo.rs](../../libos/src/linux_api/exo.rs#L74) 执行并检查：

```rust
match runtime::svc(exo_abi::SYS_IRQ_ACK, irq as u64, 0, 0) {
    0 => Ok(()),
    _ => Err(-5),
}
```

`SYS_IRQ_ACK(9)` → [sys_irq_ack](../../kernel/src/syscall/dispatch.rs#L657) → [task::ack_irq](../../kernel/src/object/task.rs#L490)：找到该 INTID 的绑定，确认 `awaiting_ack=true`，清除该状态并 `gic::enable_spi(intid)`；没有绑定或不在等待 ACK 则失败。

| 动作 | 当前 Rust 中的位置 | 作用 |
| --- | --- | --- |
| 清 `EV_PENDING` | EL0 `liteeth_interrupt` | 清 LiteEth 设备事件 |
| 写 EOIR / DIR | EL1 硬件 IRQ 入口 | GIC priority drop / deactivate |
| `SYS_IRQ_ACK` | EL0 回调返回后请求 EL1 | 校验处理状态，重新开放 SPI |

**ACK 不再写 EOI；EOIR/DIR 在投递通知前已经写过。**

### 4. 最终解绑：从 liteeth_stop 到 free_irq

Linux [liteeth_stop](../../../linux/drivers/net/ethernet/litex/litex_liteeth.c#L150)：

```c
netif_stop_queue(netdev);
netif_carrier_off(netdev);
litex_write8(priv->base + LITEETH_WRITER_EV_ENABLE, 0);
litex_write8(priv->base + LITEETH_READER_EV_ENABLE, 0);
free_irq(netdev->irq, netdev);
```

往下进入 [free_irq / __free_irq](../../../linux/kernel/irq/manage.c#L1879)：按 `dev_id` 移除 action；最后一个 handler 移除时关闭 IRQ，等待其他 CPU 上正在执行的处理结束，再释放 action。

Rust [liteeth_stop](../../libos/src/drivers/liteeth.rs#L162)：

```rust
if self.irq_registration.is_bound() {
    self.r#priv.stop_device_events();       // 停队列、断链路、写 EV_ENABLE=0
    free_irq(&mut self.irq_registration)?;
}
```

[linux_api::free_irq](../../libos/src/linux_api/interrupt.rs#L59) 先解绑，成功后才删除 EL0 回调：

```rust
let (irq, _, _) = registration.binding.ok_or(EINVAL)?;
registration.backend.unbind(irq)?;
registration.binding = None;
```

```text
liteeth_stop → free_irq → ExoIrq::unbind → runtime::irq_unbind
  → SYS_IRQ_UNBIND(10) → sys_irq_unbind → task::unbind_irq
  → 禁用 SPI，删除 EL1 绑定
  → 返回 EL0，清除 binding 中的函数指针
```

[task::unbind_irq](../../kernel/src/object/task.rs#L404) 负责禁用和删除绑定；失败则保留 EL0 binding。当前驱动与注册对象由单个 EL0 线程独占、通过 `&mut` 串行派发，因此没有照搬 Linux 的跨 CPU 回调等待机制。

`stop` 保留 Notification，供重新 `open` 使用。最终 `liteeth_remove` 消费 Driver，或直接 Drop Driver 时，都会先 stop，再由 [ExoIrq::drop](../../libos/src/linux_api/exo.rs#L85) → `Notification::destroy` → `SYS_NOTIFICATION_DESTROY(32)` 销毁通知。内核拒绝销毁仍绑定 IRQ 或仍有等待线程的对象；清理失败时终止执行，禁止继续回收仍被使用的资源。
