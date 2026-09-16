//! PSCI四核启动与跨核事件。
//!
//! CPU0在EL1调用`PSCI_CPU_ON`。辅助核不经过UEFI入口，而是从汇编跳板
//! 设置独立栈；随后安装共享页表、异常向量、GICC和本地Generic Timer。

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

const PSCI_CPU_ON_64: u64 = 0xc400_0003;
const STACK_SIZE: usize = 64 * 1024;
const STACK_PAGES: u64 = (STACK_SIZE / exo_abi::PAGE_SIZE as usize) as u64;

// CPU0在调用PSCI前从真实物理内存分配器取得辅助核栈，并以Release发布栈顶。
// 辅助核进入时MMU尚未开启，表内数值必须是identity-mapped物理地址。
#[no_mangle]
static SMP_STACK_TOPS: [AtomicU64; exo_abi::MAX_CPUS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

static CPU_COUNT: AtomicUsize = AtomicUsize::new(1);
static ONLINE_MASK: AtomicU64 = AtomicU64::new(1);
// 单任务v1退出时，发起退出的调用核通过SGI2让其他核永久停在EL1。每个目标核在
// 不再访问EL0地址空间后设置自己的bit，调用核看到完整mask才回收页表。
static STOPPED_MASK: AtomicU64 = AtomicU64::new(0);
static SHARED_ROOT: AtomicU64 = AtomicU64::new(0);
static TIMER_INTID: AtomicUsize = AtomicUsize::new(0);
static USE_VIRTUAL_TIMER: AtomicUsize = AtomicUsize::new(0);
#[no_mangle]
static BOOT_PHASE: [AtomicU32; exo_abi::MAX_CPUS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

core::arch::global_asm!(
    r#"
    .section .text.smp_entry, "ax"
    .global smp_secondary_entry
smp_secondary_entry:
    // PSCI把CPU0传入的context_id放在x0，这里就是逻辑CPU编号。
    mov x20, x0
    adrp x5, BOOT_PHASE
    add  x5, x5, :lo12:BOOT_PHASE
    mov  w6, #10
    str  w6, [x5, x20, lsl #2]
    adrp x1, SMP_STACK_TOPS
    add  x1, x1, :lo12:SMP_STACK_TOPS
    ldr  x1, [x1, x20, lsl #3]
    mov  w6, #11
    str  w6, [x5, x20, lsl #2]

    mrs x3, CurrentEL
    cmp x3, #8
    b.ne 1f

    // 某些固件让PSCI目标核从EL2进入。EL2在本系统中只负责降级，
    // 必须把物理IRQ和Timer访问交给EL1。
    mov sp, x1
    msr sp_el1, x1
    mov  w6, #12
    str  w6, [x5, x20, lsl #2]
    mrs x4, hcr_el2
    orr x4, x4, #(1 << 31)
    bic x4, x4, #(1 << 27)
    bic x4, x4, #(1 << 5)
    bic x4, x4, #(1 << 4)
    bic x4, x4, #(1 << 3)
    msr hcr_el2, x4
    mrs x4, cnthctl_el2
    orr x4, x4, #3
    msr cnthctl_el2, x4
    adr x4, 1f
    msr elr_el2, x4
    mov x4, #0x3c5
    msr spsr_el2, x4
    mov x0, x20
    eret

1:
    mov sp, x1
    // Rust/LLVM可能在普通结构体与数组操作中使用NEON寄存器。辅助核的
    // CPACR_EL1复位值会令此类指令产生FP/SIMD trap，必须在调用Rust前
    // 把FPEN[21:20]设为0b11，允许EL1和后续EL0使用。
    mrs x4, cpacr_el1
    orr x4, x4, #(3 << 20)
    msr cpacr_el1, x4
    isb
    adrp x5, BOOT_PHASE
    add  x5, x5, :lo12:BOOT_PHASE
    mov  w6, #13
    str  w6, [x5, x20, lsl #2]
    mov x0, x20
    bl smp_secondary_rust_entry
2:
    wfe
    b 2b

"#
);

extern "C" {
    fn smp_secondary_entry() -> !;
}

pub fn cpu_count() -> usize {
    CPU_COUNT.load(Ordering::Acquire)
}

pub fn online_mask() -> u64 {
    ONLINE_MASK.load(Ordering::Acquire)
}

pub fn set_cpu_count(count: usize) {
    CPU_COUNT.store(count, Ordering::Release);
}

pub fn boot_secondaries(
    topology: &crate::resources::CpuTopology,
    root: u64,
    timer_intid: u32,
    use_hvc: bool,
) -> bool {
    if topology.count != exo_abi::MAX_CPUS {
        return false;
    }
    SHARED_ROOT.store(root, Ordering::Release);
    TIMER_INTID.store(timer_intid as usize, Ordering::Release);
    USE_VIRTUAL_TIMER.store(use_hvc as usize, Ordering::Release);
    let mut stack_cpu = 1usize;
    while stack_cpu < topology.count {
        let Some(stack_pa) = crate::mem::alloc_pages_aligned(STACK_PAGES, STACK_PAGES) else {
            return false;
        };
        // 清零不是CPU运行所必需，但可避免新栈暴露上一任物理页的残留内容，
        // 并让异常回溯区域在调试时具有确定值。
        unsafe {
            core::ptr::write_bytes(stack_pa as *mut u8, 0, STACK_SIZE);
        }
        SMP_STACK_TOPS[stack_cpu].store(stack_pa + STACK_SIZE as u64, Ordering::Release);
        stack_cpu += 1;
    }
    unsafe { core::arch::asm!("dsb ishst", options(nostack)) };

    let entry = smp_secondary_entry as *const () as u64;
    let mut cpu = 1usize;
    while cpu < topology.count {
        crate::uart::puts("[exo] PSCI CPU_ON cpu=");
        crate::uart::hex(cpu as u64);
        crate::uart::puts(" mpidr=");
        crate::uart::hex(topology.mpidrs[cpu]);
        crate::uart::puts(" entry=");
        crate::uart::hex(entry);
        crate::uart::puts("\r\n");
        let result = psci_cpu_on(topology.mpidrs[cpu], entry, cpu as u64, use_hvc);
        crate::uart::puts("[exo] PSCI CPU_ON result=");
        crate::uart::hex(result as u64);
        crate::uart::puts("\r\n");
        if result != 0 && result != -4 {
            return false;
        }
        // 逐核等待，避免多个尚未验证的辅助核同时访问GIC或输出异常日志。
        // 当前CPU达到online后才启动下一核，也使失败CPU编号完全确定。
        let deadline =
            crate::scheduler::timer::counter().wrapping_add(crate::scheduler::timer::frequency());
        let mut spins = 0u64;
        while online_mask() & (1u64 << cpu) == 0 {
            spins = spins.wrapping_add(1);
            if crate::scheduler::timer::counter().wrapping_sub(deadline) as i64 >= 0
                || spins >= 100_000_000
            {
                crate::uart::puts("[exo] SMP CPU startup timeout cpu=");
                crate::uart::hex(cpu as u64);
                crate::uart::puts(" phase=");
                crate::uart::hex(BOOT_PHASE[cpu].load(Ordering::Acquire) as u64);
                crate::uart::puts("\r\n");
                return false;
            }
            core::hint::spin_loop();
        }
        cpu += 1;
    }

    let expected = (1u64 << topology.count) - 1;
    online_mask() == expected
}

fn psci_cpu_on(mpidr: u64, entry: u64, context: u64, use_hvc: bool) -> i64 {
    let result: i64;
    if use_hvc {
        unsafe {
            core::arch::asm!(
                "hvc #0",
                inlateout("x0") PSCI_CPU_ON_64 => result,
                in("x1") mpidr,
                in("x2") entry,
                in("x3") context,
                lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                lateout("x16") _, lateout("x17") _,
                options(nostack)
            );
        }
    } else {
        unsafe {
            core::arch::asm!(
                "smc #0",
                inlateout("x0") PSCI_CPU_ON_64 => result,
                in("x1") mpidr,
                in("x2") entry,
                in("x3") context,
                lateout("x4") _,
                lateout("x5") _,
                lateout("x6") _,
                lateout("x7") _,
                // SMCCC把x0..x17定义为调用者保存寄存器。若不显式声明，
                // 编译器可能让Rust局部变量跨越smc保存在这些寄存器中，
                // 固件改写后会破坏控制流或SMP等待状态。
                lateout("x8") _,
                lateout("x9") _,
                lateout("x10") _,
                lateout("x11") _,
                lateout("x12") _,
                lateout("x13") _,
                lateout("x14") _,
                lateout("x15") _,
                lateout("x16") _,
                lateout("x17") _,
                options(nostack)
            );
        }
    }
    result
}

#[no_mangle]
extern "C" fn smp_secondary_rust_entry(cpu: u64) -> ! {
    let cpu = cpu as usize;
    BOOT_PHASE[cpu].store(1, Ordering::Release);
    crate::arch::aarch64::cpu::set_id(cpu);
    BOOT_PHASE[cpu].store(2, Ordering::Release);
    crate::mmu::activate(SHARED_ROOT.load(Ordering::Acquire));
    BOOT_PHASE[cpu].store(3, Ordering::Release);
    crate::vectors::install_el1();
    BOOT_PHASE[cpu].store(4, Ordering::Release);
    // CNTKCTL_EL1是每核寄存器。允许EL0读取虚拟计数器和频率，libOS的
    // 超时、USB Future与调度测试才能在CPU1..3使用同一套counter封装。
    unsafe {
        let mut cntkctl: u64;
        core::arch::asm!(
            "mrs {value}, cntkctl_el1",
            value = out(reg) cntkctl,
            options(nomem, nostack)
        );
        cntkctl |= 1u64 << 1;
        core::arch::asm!(
            "msr cntkctl_el1, {value}",
            "isb",
            value = in(reg) cntkctl,
            options(nomem, nostack)
        );
    }
    crate::gic::init_cpu_interface();
    BOOT_PHASE[cpu].store(5, Ordering::Release);
    crate::scheduler::init(
        TIMER_INTID.load(Ordering::Acquire) as u32,
        USE_VIRTUAL_TIMER.load(Ordering::Acquire) != 0,
    );
    BOOT_PHASE[cpu].store(6, Ordering::Release);
    ONLINE_MASK.fetch_or(1u64 << cpu, Ordering::AcqRel);
    BOOT_PHASE[cpu].store(7, Ordering::Release);
    unsafe { core::arch::asm!("sev", options(nomem, nostack)) };
    crate::scheduler::priority::idle_loop()
}

pub fn send_reschedule(cpu: usize) {
    if cpu < cpu_count() && online_mask() & (1u64 << cpu) != 0 {
        if cpu == crate::arch::aarch64::cpu::id() {
            return;
        }
        // 精确目标位来自DTB CPU顺序；只在目标核正运行更低优先级线程时
        // 调用本函数。空闲核由SEV/WFE唤醒，不需要SGI。
        crate::gic::send_sgi(
            crate::gic::SGI_RESCHEDULE,
            1u8.checked_shl(cpu as u32).unwrap_or(0),
        );
    }
}

/// 让所有在线远程CPU失效共享stage-1 TLB，并等待确认。
///
/// `VMALLE1IS`中的IS由硬件广播到Inner Shareable域，随后的`DSB ISH`
/// 等待所有目标PE完成，因此不需要依赖GIC SGI的目标位映射。SGI1仍保留
/// 给以后带ASID/VA请求参数的软件shootdown协议。
pub fn shootdown_tlb() {
    // flush_el1_tlb_local名称表示“不再递归发SGI”，其中的VMALLE1IS仍是
    // 体系结构级广播TLBI，并非只清当前CPU。
    crate::mmu::flush_el1_tlb_local();
}

/// 停止除调用核外的全部在线CPU，并等待它们离开EL0。
pub fn stop_other_cpus() {
    let self_bit = 1u64 << crate::arch::aarch64::cpu::id();
    STOPPED_MASK.store(self_bit, Ordering::Release);
    crate::gic::send_sgi_all_others(crate::gic::SGI_TASK_STOP);
    let expected = online_mask();
    while STOPPED_MASK.load(Ordering::Acquire) != expected {
        core::hint::spin_loop();
    }
    unsafe { core::arch::asm!("dsb ish", options(nostack)) };
}

/// SGI2目标核调用：发布“不会再访问任务VSpace”，随后永久停在EL1。
pub fn acknowledge_stop_and_park() -> ! {
    let cpu = crate::arch::aarch64::cpu::id();
    crate::scheduler::timer::disarm();
    STOPPED_MASK.fetch_or(1u64 << cpu, Ordering::AcqRel);
    unsafe {
        core::arch::asm!("dsb ish", "sev", options(nomem, nostack));
    }
    loop {
        unsafe {
            core::arch::asm!("msr daifset, #0xf", "wfe", options(nomem, nostack));
        }
    }
}
