//! gic.rs —— GICv2 中断控制器驱动
//!
//! 支持 QEMU virt (arm,cortex-a15-gic) 和 Pi5 (arm,gic-400)。
//! GICv2 两个寄存器块:
//!   GICD (Distributor): 中断配置、使能、优先级
//!   GICC (CPU Interface): 确认中断 (IAR)、结束中断 (EOIR)
//!
//! 所有寄存器都是 32-bit, 必须用 volatile 访问 (Device memory)。

use core::sync::atomic::{AtomicU64, Ordering};

use crate::uart;

// GICD 寄存器偏移 (相对 gicd_base)
const GICD_CTLR: usize = 0x000;
const GICD_IGROUPR: usize = 0x080;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_ICPENDR: usize = 0x280;
const GICD_ICACTIVER: usize = 0x380;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;
const GICD_ICFGR: usize = 0xC00;
const GICD_SGIR: usize = 0xF00;

pub const SGI_RESCHEDULE: u32 = 0;
pub const SGI_TLB_SHOOTDOWN: u32 = 1;
pub const SGI_TASK_STOP: u32 = 2;

// GICC 寄存器偏移 (相对 gicc_base)
const GICC_CTLR: usize = 0x00;
const GICC_PMR: usize = 0x04;
const GICC_IAR: usize = 0x0C;
const GICC_EOIR: usize = 0x10;
const GICC_DIR: usize = 0x1000;

// CPU0从DTB发现并映射GIC后以Release发布基地址；辅助核在PSCI入口以
// Acquire读取，保证页表更新和基地址写入都先于其MMIO访问可见。
static GICD_BASE: AtomicU64 = AtomicU64::new(0);
static GICC_BASE: AtomicU64 = AtomicU64::new(0);

/// 是否需要初始化 (DTB 发现 GIC 之前不操作寄存器)。
fn ready() -> bool {
    GICD_BASE.load(Ordering::Acquire) != 0 && GICC_BASE.load(Ordering::Acquire) != 0
}

/// 从 DTB 解析后调用一次。gicd_pa / gicc_pa 必须是已映射为 Device 的物理地址。
pub fn init(gicd_pa: u64, gicc_pa: u64) {
    GICD_BASE.store(gicd_pa, Ordering::Release);
    GICC_BASE.store(gicc_pa, Ordering::Release);

    gicd_write(GICD_CTLR, gicd_read(GICD_CTLR) | 1);
    init_cpu_interface();
    barrier();

    uart::puts("[exo] GICv2 init: GICD=");
    uart::hex(gicd_pa);
    uart::puts(" GICC=");
    uart::hex(gicc_pa);
    uart::puts("\r\n");
}

/// GICC寄存器在GICv2中按CPU banked，每个辅助核都必须单独开启。
pub fn init_cpu_interface() {
    // GICD_ISENABLER0中的SGI/PPI使能位是按CPU banked的，不能依赖UEFI
    // 留给CPU0或辅助核的复位状态。四核都显式开放三个Kernel SGI：
    // 重调度、TLB shootdown和任务停止。
    // SGI/PPI寄存器0是每核banked。PSCI辅助核可能继承固件留下的
    // pending/active状态；若SGI0保持active且priority为0，任何Timer PPI
    // 都会被RPR优先级屏蔽。先清状态，再开放Kernel SGI。
    gicd_write(GICD_ICPENDR, u32::MAX);
    gicd_write(GICD_ICACTIVER, u32::MAX);
    gicd_write(
        GICD_ISENABLER,
        (1u32 << SGI_RESCHEDULE) | (1u32 << SGI_TLB_SHOOTDOWN) | (1u32 << SGI_TASK_STOP),
    );
    gicc_write(GICC_PMR, 0xff);
    // 同时打开Secure/Non-secure两种两阶段EOI模式。固件可能让Kernel
    // 从不同Security state进入；bit9控制Group1/NS，bit10控制Group0/S。
    // IRQ路径统一先EOIR降低优先级，再DIR解除active，防止SGI0的最高
    // 优先级长期留在RPR=0并压住后续Generic Timer。
    let control = gicc_read(GICC_CTLR) | 1 | (1u32 << 9) | (1u32 << 10);
    gicc_write(GICC_CTLR, control);
    barrier();
}

/// 配置一个 SPI 中断: 设置优先级、目标 CPU、边沿/电平触发、初始禁用。
pub fn configure_spi(intid: u32, trigger_level: bool, target_cpu: usize) {
    if !ready() || intid < 32 || target_cpu >= exo_abi::MAX_CPUS {
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

    // GICv2 ITARGETSR的每一位对应一个CPU Interface。QEMU virt与Pi5
    // DTB的CPU顺序均与该目标位一致。
    let target_off = GICD_ITARGETSR + 4 * (idx / 4);
    let target_shift = 8 * (idx % 4);
    let target: u32 = gicd_read(target_off);
    gicd_write(
        target_off,
        (target & !(0xffu32 << target_shift)) | (((1u32 << target_cpu) & 0xff) << target_shift),
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

/// 向指定CPU mask发送Kernel保留SGI。写GICD_SGIR是Device MMIO写，
/// `dsb ishst`确保此前对线程状态的普通内存写先对目标CPU可见。
pub fn send_sgi(intid: u32, target_mask: u8) {
    if !ready() || intid > 15 || target_mask == 0 {
        return;
    }
    unsafe { core::arch::asm!("dsb ishst", options(nostack)) };
    gicd_write(GICD_SGIR, ((target_mask as u32) << 16) | (intid & 0xf));
    barrier();
}

/// 向除当前PE外的所有CPU Interface发送Kernel SGI。
///
/// GICv2的CPUTargetList位编号由实现定义，QEMU/Pi5不应假定辅助核看到的
/// bit0一定代表启动核。`TargetListFilter=0b01`由GIC硬件选择“所有其他
/// PE”，正好符合共享VSpace全核TLB shootdown的目标集合。
pub fn send_sgi_all_others(intid: u32) {
    if !ready() || intid > 15 {
        return;
    }
    unsafe { core::arch::asm!("dsb ishst", options(nostack)) };
    gicd_write(GICD_SGIR, (0b01u32 << 24) | (intid & 0xf));
    barrier();
}

/// 配置当前CPU私有的PPI。Generic Timer使用PPI 30；PPI不需要设置
/// ITARGETSR，触发类型也由体系结构固定，只设置优先级并清pending。
pub fn configure_private(intid: u32, priority: u8) {
    if !ready() || !(16..32).contains(&intid) {
        return;
    }
    disable_private(intid);
    gicd_write(GICD_IGROUPR, gicd_read(GICD_IGROUPR) & !(1u32 << intid));
    gicd_write(GICD_ICPENDR, 1u32 << intid);
    let offset = GICD_IPRIORITYR + 4 * (intid as usize / 4);
    let shift = 8 * (intid as usize % 4);
    let old = gicd_read(offset);
    gicd_write(
        offset,
        (old & !(0xff << shift)) | ((priority as u32) << shift),
    );
    barrier();
}

pub fn enable_private(intid: u32) {
    if !ready() || !(16..32).contains(&intid) {
        return;
    }
    gicd_write(GICD_ISENABLER, 1u32 << intid);
    barrier();
}

pub fn disable_private(intid: u32) {
    if !ready() || !(16..32).contains(&intid) {
        return;
    }
    gicd_write(GICD_ICENABLER, 1u32 << intid);
    barrier();
}

/// 清除当前CPU banked的SGI/PPI pending位。
///
/// Generic Timer先撤销自己的level信号，再调用本函数清掉GIC已经采样的
/// pending状态。这个顺序对HVF尤其重要：只关闭CNTV_CTL并不能保证已注入
/// 的虚拟PPI立即消失，若直接EOI可能马上再次进入同一个IRQ。
pub fn clear_private_pending(intid: u32) {
    if !ready() || !(16..32).contains(&intid) {
        return;
    }
    gicd_write(GICD_ICPENDR, 1u32 << intid);
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
    gicc_write(GICC_DIR, iar_token);
    let finished_intid = intid(iar_token);
    if finished_intid < 32 {
        // SGI/PPI状态是每核banked。部分UEFI/QEMU组合在切换Security view
        // 后即使DIR完成，ISACTIVER0仍保留旧位；显式写ICACTIVER0避免
        // priority stack已经drop但Distributor仍认为SGI0 active。
        gicd_write(GICD_ICACTIVER, 1u32 << finished_intid);
    }
    // EOIR完成priority drop，DIR完成deactivate；两次都使用读取IAR得到
    // 的完整token，尤其不能丢掉SGI source CPU字段。
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
        let addr = (GICD_BASE.load(Ordering::Acquire) + offset as u64) as *const u32;
        core::ptr::read_volatile(addr)
    }
}

fn gicd_write(offset: usize, val: u32) {
    unsafe {
        let addr = (GICD_BASE.load(Ordering::Acquire) + offset as u64) as *mut u32;
        core::ptr::write_volatile(addr, val);
    }
}

fn gicc_read(offset: usize) -> u32 {
    unsafe {
        let addr = (GICC_BASE.load(Ordering::Acquire) + offset as u64) as *const u32;
        core::ptr::read_volatile(addr)
    }
}

fn gicc_write(offset: usize, val: u32) {
    unsafe {
        let addr = (GICC_BASE.load(Ordering::Acquire) + offset as u64) as *mut u32;
        core::ptr::write_volatile(addr, val);
    }
}

fn barrier() {
    unsafe {
        core::arch::asm!("dsb sy", "isb", options(nomem, nostack));
    }
}
