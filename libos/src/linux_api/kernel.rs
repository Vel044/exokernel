// SPDX-License-Identifier: GPL-2.0-or-later
//! libOS 平台运行时注入的底层操作；Linux 驱动接口不增加后端参数。
//! 只在设备绑定前安装；运行中的设备不得切换平台服务。
use core::{
    ffi::c_void,
    sync::atomic::{AtomicPtr, Ordering},
};
#[derive(Clone, Copy)]
pub struct KernelOps {
    pub map: unsafe fn(u64, u64, u64) -> Result<*mut c_void, i32>,
    pub unmap: unsafe fn(*mut c_void, u64) -> Result<(), i32>,
    pub bind: fn(u32) -> Result<u64, i32>,
    pub wait: fn(u64) -> Result<u64, i32>,
    pub ack: fn(u32) -> Result<(), i32>,
    pub unbind: fn(u32) -> Result<(), i32>,
    pub destroy: fn(u64) -> Result<(), i32>,
    pub cancel_wait: fn(u64) -> Result<(), i32>,
    pub yield_now: fn(),
    pub start_worker: fn(u32) -> Result<u64, i32>,
    pub wait_worker: fn(u64) -> Result<(), i32>,
    pub stop_worker: fn(u64),
    pub read32: unsafe fn(*const c_void) -> u32,
    pub write32: unsafe fn(*mut c_void, u32),
    pub fromio: unsafe fn(*mut u8, *const c_void, usize),
    pub toio: unsafe fn(*mut c_void, *const u8, usize),
    pub log: fn(&str),
    pub ticks: fn() -> u64,
    pub frequency: fn() -> u64,
    pub cpu: fn() -> usize,
    pub cpus: fn() -> usize,
}
static OPS: AtomicPtr<KernelOps> = AtomicPtr::new(core::ptr::null_mut());
/// SAFETY: ops 必须为静态对象；所有旧设备和回调结束后才可更换。
pub unsafe fn install(ops: &'static KernelOps) {
    OPS.store(ops as *const _ as *mut _, Ordering::Release);
}
pub fn ops() -> &'static KernelOps {
    let p = OPS.load(Ordering::Acquire);
    assert!(!p.is_null(), "尚未安装驱动平台服务");
    // SAFETY: install 的静态生命周期约定保证指针有效。
    unsafe { &*p }
}
