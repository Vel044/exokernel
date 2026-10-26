// SPDX-License-Identifier: GPL-2.0-or-later
//! MMIO 同名操作及可测试后端。地址是已授权映射的 CPU VA，不是 PA/DMA address。
//! LiteX 的 8/16 位逻辑寄存器仍执行 32 位小端事务；设备 SRAM 使用逐字节 volatile。
use core::{marker::PhantomData, ptr::NonNull};
/// 驱动、映射和测试模型统一使用 Linux 同名方法；具体后端选择寄存器区或 SRAM。
pub trait Io {
    fn litex_read32(&self, offset: usize) -> u32;
    fn litex_write32(&self, offset: usize, value: u32);
    fn memcpy_fromio(&self, offset: usize, out: &mut [u8]);
    fn memcpy_toio(&self, offset: usize, bytes: &[u8]);
}
pub fn litex_read8(io: &impl Io, offset: usize) -> u8 {
    litex_read32(io, offset) as u8
}
pub fn litex_read32(io: &impl Io, offset: usize) -> u32 {
    io.litex_read32(offset)
}
pub fn litex_write8(io: &impl Io, offset: usize, value: u8) {
    litex_write32(io, offset, value as u32)
}
pub fn litex_write16(io: &impl Io, offset: usize, value: u16) {
    litex_write32(io, offset, value as u32)
}
pub fn litex_write32(io: &impl Io, offset: usize, value: u32) {
    io.litex_write32(offset, value)
}
pub fn memcpy_fromio(io: &impl Io, offset: usize, out: &mut [u8]) {
    io.memcpy_fromio(offset, out)
}
pub fn memcpy_toio(io: &impl Io, offset: usize, bytes: &[u8]) {
    io.memcpy_toio(offset, bytes)
}
pub fn barrier() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: EL0 允许 DMB。osh 对设备访问排序；未标 nomem，编译器也不能跨屏障重排内存。
    unsafe {
        core::arch::asm!("dmb osh", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}
/// 对存活映射的借用；不持有物理页所有权，也不创建普通 Rust 内存引用。
pub struct IoRegion<'a> {
    /// CPU VA；NonNull 保留裸指针性质，不允许安全代码随意跨线程共享。
    base: NonNull<u8>,
    /// 已映射字节数，所有事务先检查边界。
    size: usize,
    /// 与拥有者借用绑定，拥有者不能在本视图存活时撤销映射。
    _mapping: PhantomData<&'a ()>,
}
impl<'a> IoRegion<'a> {
    /// SAFETY: 调用者保证整个 VA 范围有设备映射，存活至 'a，且没有不允许的并发访问。
    pub unsafe fn new(base: *mut u8, size: usize) -> Self {
        assert!(size > 0);
        Self {
            base: NonNull::new(base).expect("MMIO 空地址"),
            size,
            _mapping: PhantomData,
        }
    }
    fn check(&self, offset: usize, count: usize) {
        assert!(offset
            .checked_add(count)
            .is_some_and(|end| end <= self.size));
    }
}
impl Io for IoRegion<'_> {
    fn litex_read32(&self, offset: usize) -> u32 {
        self.check(offset, 4);
        assert!((self.base.as_ptr() as usize + offset) % 4 == 0);
        // SAFETY: 已验证范围/对齐；volatile 读取真实设备寄存器，屏障在读取之后。
        let value =
            unsafe { core::ptr::read_volatile(self.base.as_ptr().add(offset).cast::<u32>()) };
        barrier();
        u32::from_le(value)
    }
    fn litex_write32(&self, offset: usize, value: u32) {
        self.check(offset, 4);
        assert!((self.base.as_ptr() as usize + offset) % 4 == 0);
        barrier();
        // SAFETY: 已验证范围/对齐；小端 volatile 事务产生设备寄存器效果。
        unsafe {
            core::ptr::write_volatile(self.base.as_ptr().add(offset).cast::<u32>(), value.to_le());
        }
        barrier();
    }
    fn memcpy_fromio(&self, offset: usize, out: &mut [u8]) {
        self.check(offset, out.len());
        for (i, byte) in out.iter_mut().enumerate() {
            // SAFETY: 映射存活且范围已校验；out 为独占普通内存切片。
            *byte = unsafe { core::ptr::read_volatile(self.base.as_ptr().add(offset + i)) };
        }
        barrier();
    }
    fn memcpy_toio(&self, offset: usize, bytes: &[u8]) {
        self.check(offset, bytes.len());
        for (i, byte) in bytes.iter().enumerate() {
            // SAFETY: 目标位于设备映射内；复制完成的屏障先于后续设备 START。
            unsafe {
                core::ptr::write_volatile(self.base.as_ptr().add(offset + i), *byte);
            }
        }
        barrier();
    }
}
