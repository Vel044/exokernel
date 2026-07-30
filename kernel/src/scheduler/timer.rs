//! ARM Generic Timer的EL1 physical timer封装。

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::{gic, uart};

// 所有CPU使用同一种Generic Timer PPI和1ms时间片长度。这两个值只在
// 启动阶段写入，之后由各核IRQ路径并发读取，因此使用原子量避免SMP数据竞争。
static TIMER_INTID: AtomicU32 = AtomicU32::new(0);
static TICKS_PER_SLICE: AtomicU64 = AtomicU64::new(0);

pub fn init(intid: u32) -> bool {
    if !(16..32).contains(&intid) {
        return false;
    }
    let frequency = frequency();
    if frequency == 0 {
        return false;
    }
    TIMER_INTID.store(intid, Ordering::Release);
    TICKS_PER_SLICE.store((frequency / 1_000).max(1), Ordering::Release);
    disarm();
    gic::configure_private(intid, 0x20);
    gic::enable_private(intid);
    // 多核同时写PL011会让日志交错；统一只由启动核输出平台定时器摘要。
    if crate::arch::aarch64::cpu::id() == 0 {
        uart::puts("[exo] Generic Timer PPI=");
        uart::hex(intid as u64);
        uart::puts(" frequency=");
        uart::hex(frequency);
        uart::puts(" slice_ticks=");
        uart::hex(TICKS_PER_SLICE.load(Ordering::Acquire));
        uart::puts("\r\n");
    }
    true
}

pub fn is_timer_irq(intid: u32) -> bool {
    let configured = TIMER_INTID.load(Ordering::Acquire);
    configured != 0 && configured == intid
}

pub fn counter() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) value, options(nomem, nostack));
    }
    value
}

pub fn frequency() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) value, options(nomem, nostack));
    }
    value
}

pub fn arm_full() -> u64 {
    let start = counter();
    let ticks = TICKS_PER_SLICE.load(Ordering::Acquire) as u32;
    unsafe {
        core::arch::asm!(
            // TVAL写入相对当前物理计数器的32位有符号倒计时；1ms远小于
            // i32::MAX，适合每次上下文切换重新装载，避免跨核绝对CVAL基准
            // 在虚拟平台上的实现差异。
            "msr cntp_tval_el0, {}",
            "msr cntp_ctl_el0, {}",
            "isb",
            in(reg) ticks as u64,
            in(reg) 1u64,
            options(nostack)
        );
    }
    start
}

pub fn disarm() {
    unsafe {
        core::arch::asm!("msr cntp_ctl_el0, {}", "isb", in(reg) 0u64, options(nostack));
    }
}
