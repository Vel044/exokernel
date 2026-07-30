//! 单线程 USB Future 执行器。
//!
//! CrabUSB 用 Future 表示 xHCI command 和 transfer。Future 返回 Pending 时，
//! 当前 USB 线程通过 Kernel Notification 阻塞；硬件 IRQ 到达后回到 EL0
//! 消费 Event Ring，再 ACK IRQ 并继续 poll。这里不包含任何 USB 类协议。

use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use crab_usb::EventHandler;

const XHCI_IRQ_BADGE: u64 = 1;

/// 运行一个 USB Future，直到返回 Ready。
pub(crate) fn block_on_usb<F: Future>(
    future: F,
    handler: &EventHandler,
    expected_intid: u32,
    notification: &crate::notification::Notification,
) -> F::Output {
    // RawWaker只记录“需要再次poll”；真正的阻塞由Notification完成。
    let waker = unsafe { Waker::from_raw(raw_waker()) };
    let mut context = Context::from_waker(&waker);
    // pin保证Future在执行期间地址不再移动，满足自引用Future的poll契约。
    let mut future = core::pin::pin!(future);

    loop {
        // 每轮poll前清除旧的自唤醒标记，防止把上一次wake误当作新事件。
        WOKEN.store(false, Ordering::Release);
        match Future::poll(Pin::as_mut(&mut future), &mut context) {
            // USB command/transfer已完成，直接把结果交还调用者。
            Poll::Ready(output) => return output,
            Poll::Pending if WOKEN.swap(false, Ordering::AcqRel) => {
                // CrabUSB的有界超时Future可能主动唤醒自己；顺便检查Event Ring。
                handler.handle_event();
                core::hint::spin_loop();
            }
            Poll::Pending => {
                // Kernel阻塞当前USB线程，其他EL0线程仍可由调度器运行。
                let badge = notification
                    .wait()
                    .unwrap_or_else(|_| fail("xHCI Notification wait failed", 0x20c));
                // 同一个Notification可以合并多个badge，只处理属于xHCI的bit。
                if badge & XHCI_IRQ_BADGE != 0 {
                    // USB协议和TRB解析都在EL0；Kernel只负责投递与重新开放IRQ。
                    handler.handle_event();
                    crate::runtime::irq_ack(expected_intid);
                }
            }
        }
    }
}

static WOKEN: AtomicBool = AtomicBool::new(false);

// RawWaker不携带任务指针，因此clone只需返回相同的静态vtable。
unsafe fn clone_waker(_: *const ()) -> RawWaker {
    raw_waker()
}

unsafe fn wake(_: *const ()) {
    // Release与poll侧AcqRel配对，保证Future在wake前写入的状态可见。
    WOKEN.store(true, Ordering::Release);
}

unsafe fn wake_by_ref(_: *const ()) {
    WOKEN.store(true, Ordering::Release);
}

unsafe fn drop_waker(_: *const ()) {}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake, wake_by_ref, drop_waker);

fn raw_waker() -> RawWaker {
    // data为空是安全的，因为vtable中的函数从不解引用该指针。
    RawWaker::new(core::ptr::null(), &WAKER_VTABLE)
}

fn fail(message: &'static str, code: u64) -> ! {
    crate::runtime::puts(b"[libos] ");
    crate::runtime::puts(message.as_bytes());
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(code)
}
