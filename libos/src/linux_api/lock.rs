// SPDX-License-Identifier: GPL-2.0-or-later
//! 内部表锁与设备访问锁；设备锁等待时让出 CPU，IRQ 注销可取消尚未进入回调的等待。
use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};
pub struct Lock<T> {
    held: AtomicBool,
    value: UnsafeCell<T>,
}
// SAFETY: 只有持有 Acquire 锁的 guard 能访问 T；Release 后才允许下一使用者进入。
unsafe impl<T: Send> Sync for Lock<T> {}
impl<T> Lock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            held: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }
    pub fn try_lock(&self) -> Option<Guard<'_, T>> {
        self.held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Guard(self))
    }
    pub fn lock(&self) -> Guard<'_, T> {
        while self
            .held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        Guard(self)
    }
}
pub struct Guard<'a, T>(&'a Lock<T>);
impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.0.value.get() }
    }
}
impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.0.value.get() }
    }
}
impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.0.held.store(false, Ordering::Release);
    }
}
