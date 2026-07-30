//! 关闭本地 IRQ 的内核自旋锁。

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// 保护可被多个 CPU 同时访问的内核对象。
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// `value`只能在成功持锁后访问；Acquire/Release建立跨CPU可见性，
// 因此只要T可以在线程间发送，SpinLock<T>就可以作为共享静态对象。
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// 关闭本地IRQ并取得锁。
    ///
    /// 锁不在Drop中重新打开IRQ：SVC/IRQ异常返回会从SPSR恢复原来的EL0
    /// PSTATE，idle路径也会在WFE前显式打开IRQ。若在释放锁的一瞬间打开
    /// IRQ，设备中断可能嵌套到仍在使用同一EL1栈帧的系统调用中，增加
    /// 内核栈和对象状态的重入风险。
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        disable_irq();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
        SpinLockGuard { lock: self }
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // 安全前提：guard只能由成功持锁的lock()构造，且锁未释放。
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // 安全前提同Deref；同一时刻只有一个可变guard。
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

#[inline(always)]
fn disable_irq() {
    unsafe {
        core::arch::asm!("msr daifset, #2", options(nomem, nostack));
    }
}
