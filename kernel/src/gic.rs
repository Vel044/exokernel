//! gic.rs —— GICv2 中断控制器驱动
//!
//! 支持 QEMU virt (arm,cortex-a15-gic) 和 Pi5 (arm,gic-400)。
//! GICv2 两个寄存器块:
//!   GICD (Distributor): 中断配置、使能、优先级
//!   GICC (CPU Interface): 确认中断 (IAR)、结束中断 (EOIR)
//!
//! 所有寄存器都是 32-bit, 必须用 volatile 访问 (Device memory)。

use crate::uart;

// GICD 寄存器偏移 (相对 gicd_base)
const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_ICPENDR: usize = 0x280;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;
const GICD_ICFGR: usize = 0xC00;

// GICC 寄存器偏移 (相对 gicc_base)
const GICC_CTLR: usize = 0x00;
const GICC_PMR: usize = 0x04;
const GICC_IAR: usize = 0x0C;
const GICC_EOIR: usize = 0x10;

static mut GICD_BASE: u64 = 0;
static mut GICC_BASE: u64 = 0;
static mut UART_INTID: u32 = 0;

/// 是否需要初始化 (DTB 发现 GIC 之前不操作寄存器)。
fn ready() -> bool {
    unsafe { GICD_BASE != 0 && GICC_BASE != 0 }
}

/// 从 DTB 解析后调用一次。gicd_pa / gicc_pa 必须是已映射为 Device 的物理地址。
pub fn init(gicd_pa: u64, gicc_pa: u64) {
    unsafe {
        GICD_BASE = gicd_pa;
        GICC_BASE = gicc_pa;
    }

    // 保留固件已有的分组设置，只确保 Distributor Group 0 和当前 CPU
    // interface 开启。当前外核和 libOS 处于同一安全状态。
    gicd_write(GICD_CTLR, gicd_read(GICD_CTLR) | 1);
    gicc_write(GICC_PMR, 0xff);
    gicc_write(GICC_CTLR, gicc_read(GICC_CTLR) | 1);
    barrier();

    uart::puts("[exo] GICv2 init: GICD=");
    uart::hex(gicd_pa);
    uart::puts(" GICC=");
    uart::hex(gicc_pa);
    uart::puts("\r\n");
}

/// 登记唯一允许当前 EL0 任务绑定的 UART IRQ。
pub fn authorize_uart_irq(intid: u32) {
    unsafe { UART_INTID = intid };
}

pub fn is_authorized(intid: u32) -> bool {
    unsafe { UART_INTID != 0 && UART_INTID == intid }
}

/// 配置一个 SPI 中断: 设置优先级、目标 CPU、边沿/电平触发、初始禁用。
pub fn configure_spi(intid: u32, trigger_level: bool) {
    if !ready() || intid < 32 {
        return;
    }

    let idx = intid as usize;
    disable_spi(intid);
    gicd_write(GICD_ICPENDR + 4 * (idx / 32), 1u32 << (intid % 32));

    // 优先级: 设为默认中间值 0x80
    let prio_off = GICD_IPRIORITYR + 4 * (idx / 4);
    let prio_shift = 8 * (idx % 4);
    let prio: u32 = gicd_read(prio_off);
    gicd_write(
        prio_off,
        (prio & !(0xffu32 << prio_shift)) | (0x80u32 << prio_shift),
    );

    // 目标 CPU: 发送到 CPU0
    let target_off = GICD_ITARGETSR + 4 * (idx / 4);
    let target_shift = 8 * (idx % 4);
    let target: u32 = gicd_read(target_off);
    gicd_write(
        target_off,
        (target & !(0xffu32 << target_shift)) | (1u32 << target_shift),
    );

    // 触发方式: 0=level, 2=edge
    let cfg_off = GICD_ICFGR + 4 * (idx / 16);
    let cfg_shift = 2 * (idx % 16);
    let cfg: u32 = gicd_read(cfg_off);
    let cfg_val: u32 = if trigger_level { 0 } else { 2 };
    gicd_write(
        cfg_off,
        (cfg & !(0x3u32 << cfg_shift)) | (cfg_val << cfg_shift),
    );
    barrier();
}

/// 在 GICD 中使能一个 SPI。
pub fn enable_spi(intid: u32) {
    if !ready() || intid < 32 {
        return;
    }
    let reg_off = GICD_ISENABLER + 4 * (intid as usize / 32);
    gicd_write(reg_off, 1u32 << (intid % 32));
    barrier();
}

/// 在 GICD 中禁用一个 SPI。
pub fn disable_spi(intid: u32) {
    if !ready() || intid < 32 {
        return;
    }
    let reg_off = GICD_ICENABLER + 4 * (intid as usize / 32);
    gicd_write(reg_off, 1u32 << (intid % 32));
    barrier();
}

/// 读 GICC_IAR, 返回 (INTID, CPUID)。
/// 返回 1023 表示 spurious interrupt。
pub fn read_iar() -> u32 {
    if !ready() {
        return 1023;
    }
    gicc_read(GICC_IAR)
}

/// 写回读取 IAR 时得到的完整 token，完成中断处理。
pub fn write_eoir(iar_token: u32) {
    if !ready() {
        return;
    }
    gicc_write(GICC_EOIR, iar_token);
    barrier();
}

/// 确认 IRQ 并返回完整 IAR token。
pub fn acknowledge() -> Option<u32> {
    let raw = read_iar();
    if raw >= 1020 {
        return None;
    }
    Some(raw)
}

pub fn intid(iar_token: u32) -> u32 {
    iar_token & 0x3ff
}

// ── 内部 MMIO 访问 ──

fn gicd_read(offset: usize) -> u32 {
    unsafe {
        let addr = (GICD_BASE + offset as u64) as *const u32;
        core::ptr::read_volatile(addr)
    }
}

fn gicd_write(offset: usize, val: u32) {
    unsafe {
        let addr = (GICD_BASE + offset as u64) as *mut u32;
        core::ptr::write_volatile(addr, val);
    }
}

fn gicc_read(offset: usize) -> u32 {
    unsafe {
        let addr = (GICC_BASE + offset as u64) as *const u32;
        core::ptr::read_volatile(addr)
    }
}

fn gicc_write(offset: usize, val: u32) {
    unsafe {
        let addr = (GICC_BASE + offset as u64) as *mut u32;
        core::ptr::write_volatile(addr, val);
    }
}

fn barrier() {
    unsafe {
        core::arch::asm!("dsb sy", "isb", options(nomem, nostack));
    }
}
