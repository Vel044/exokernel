// SPDX-License-Identifier: GPL-2.0-or-later
//! 可失败的 EL0 堆分配；返回 NULL，保留驱动的分配失败分支。
use alloc::alloc::{alloc_zeroed, dealloc};
use core::{
    alloc::Layout,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};
static LIVE: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static FAIL: AtomicUsize = AtomicUsize::new(usize::MAX);
pub fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}
#[cfg(test)]
pub fn fail_after(n: usize) {
    FAIL.store(n, Ordering::Relaxed);
}
pub unsafe fn allocate(size: usize, align: usize) -> *mut u8 {
    #[cfg(test)]
    {
        let n = FAIL.load(Ordering::Relaxed);
        if n == 0 {
            return ptr::null_mut();
        }
        if n != usize::MAX {
            FAIL.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let layout = match Layout::from_size_align(size.max(1), align) {
        Ok(l) => l,
        Err(_) => return ptr::null_mut(),
    };
    let p = alloc_zeroed(layout);
    if !p.is_null() {
        LIVE.fetch_add(1, Ordering::Relaxed);
    }
    p
}
/// SAFETY: p、size、align 必须来自同一成功分配，且仅释放一次。
pub unsafe fn release(p: *mut u8, size: usize, align: usize) {
    if !p.is_null() {
        dealloc(p, Layout::from_size_align(size.max(1), align).unwrap());
        LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}
pub unsafe fn value<T>(v: T) -> *mut T {
    let p = allocate(core::mem::size_of::<T>(), core::mem::align_of::<T>()).cast::<T>();
    if !p.is_null() {
        ptr::write(p, v);
    }
    p
}
pub unsafe fn free_value<T>(p: *mut T) {
    ptr::drop_in_place(p);
    release(
        p.cast(),
        core::mem::size_of::<T>(),
        core::mem::align_of::<T>(),
    );
}
