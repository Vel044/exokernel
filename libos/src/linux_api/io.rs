// SPDX-License-Identifier: GPL-2.0-or-later
//! 参数与 Linux 一致：寄存器传完整 VA，拷贝传目标、源和长度；不额外传 offset/后端。
//! VA 来自已授权的 Device 映射。LiteX 8/16 位逻辑值仍通过 32 位事务访问。
use super::kernel;
use core::ffi::c_void;
pub unsafe fn litex_read8(reg: *mut c_void) -> u8 {
    litex_read32(reg) as u8
}
pub unsafe fn litex_read32(reg: *mut c_void) -> u32 {
    (kernel::ops().read32)(reg)
}
pub unsafe fn litex_write8(reg: *mut c_void, val: u8) {
    litex_write32(reg, val as u32)
}
pub unsafe fn litex_write16(reg: *mut c_void, val: u16) {
    litex_write32(reg, val as u32)
}
pub unsafe fn litex_write32(reg: *mut c_void, val: u32) {
    (kernel::ops().write32)(reg, val)
}
pub unsafe fn memcpy_fromio(to: *mut u8, from: *const c_void, count: usize) {
    (kernel::ops().fromio)(to, from, count)
}
pub unsafe fn memcpy_toio(to: *mut c_void, from: *const u8, count: usize) {
    (kernel::ops().toio)(to, from, count)
}
pub fn barrier() {
    #[cfg(target_arch = "aarch64")] // SAFETY: EL0 可执行 DMB；编译器和 CPU 均保持设备访问顺序。
    unsafe {
        core::arch::asm!("dmb osh", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}
