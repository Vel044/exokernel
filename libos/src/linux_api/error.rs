// SPDX-License-Identifier: GPL-2.0-or-later
//! 保留 Linux 负错误码与错误指针协议；只检查地址值，不解引用错误指针。
use core::ffi::c_void;
pub const EINVAL: i32 = 22;
pub const ENXIO: i32 = 6;
pub const ENOMEM: i32 = 12;
pub const EBUSY: i32 = 16;
pub const ENODEV: i32 = 19;
pub const EOPNOTSUPP: i32 = 95;
pub fn ERR_PTR(error: i64) -> *mut c_void {
    error as isize as *mut c_void
}
pub fn IS_ERR(ptr: *const c_void) -> bool {
    (ptr as usize) >= (-4095isize as usize)
}
pub fn PTR_ERR(ptr: *const c_void) -> i64 {
    ptr as isize as i64
}
pub fn unlikely(x: bool) -> bool {
    x
}
