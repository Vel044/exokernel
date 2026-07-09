//! trap.rs —— EL 切换与异常处理
//!
//! 当前启动链:
//!   EL2 boot shim -> enter_el1_kernel() -> EL1 外核
//!   EL1 外核      -> enter_el0()        -> EL0 libOS

use crate::uart;

pub const SYS_PUTC: u64 = 1;
pub const SYS_PUTS: u64 = 2;
pub const SYS_EXIT: u64 = 5;
pub const SYS_MAP_MMIO: u64 = 6;

/// EL2 boot shim 进入 EL1 外核。
pub fn enter_el1_kernel(entry_pc: u64, stack_top: u64, boot_info_ptr: u64, next_pc: u64) -> ! {
    let old_hcr = mrs!("hcr_el2");
    let old_sctlr_el1 = mrs!("sctlr_el1");
    let old_sctlr_el2 = mrs!("sctlr_el2");

    uart::puts("[exo] pre-eret HCR_EL2=");
    uart::hex(old_hcr);
    uart::puts(" SCTLR_EL1=");
    uart::hex(old_sctlr_el1);
    uart::puts(" SCTLR_EL2=");
    uart::hex(old_sctlr_el2);
    uart::puts("\r\n");

    // 保留固件已经设置的 HCR_EL2 位, 但进入普通 EL1 前必须清 TGE。
    //
    // RW  (bit31) = 1: EL1 是 AArch64。
    // TGE (bit27) = 1: traps general exceptions, 不适合这里正常 eret 到 EL1。
    //
    // Pi5 UEFI 给的 HCR_EL2=0x88000000, 也就是 RW=1 且 TGE=1。
    // 如果保留 TGE, eret 到 EL1 会触发 EC=0x0e Illegal Execution state。
    let new_hcr = (old_hcr | (1u64 << 31)) & !(1u64 << 27);
    msr!("hcr_el2", new_hcr);
    uart::puts("[exo] new HCR_EL2=");
    uart::hex(new_hcr);
    uart::puts("\r\n");

    // bring-up 阶段不要强制关闭 EL1 MMU。
    //
    // UEFI 环境下 entry_pc 是当前代码地址; 真机上它不一定能在 EL1 MMU-off
    // 状态下直接取指。先继承固件提供的 EL1 控制状态, 验证能否进入 EL1
    // trampoline。等 EL1 能稳定打印后, 再由 EL1 建自己的 stage-1 页表接管。
    //
    // 只清 alignment/stack alignment 检查, 避免早期 Rust 栈访问被严格对齐规则打断。
    msr!("sctlr_el1", old_sctlr_el1 & !((1u64 << 1) | (1u64 << 3) | (1u64 << 4)));

    // 原设计: EL1 初始先关 MMU, 同时清掉固件可能留下的 alignment/stack alignment 检查。
    //
    // M  = bit0: stage-1 MMU enable
    // A  = bit1: 普通对齐检查
    // SA = bit3: EL1 SP 对齐检查
    // SA0= bit4: EL0 SP 对齐检查
    unsafe { core::arch::asm!("isb", options(nomem, nostack)); }
    msr!("elr_el2", entry_pc);
    let spsr_el2 = 0x3c5u64;
    msr!("spsr_el2", spsr_el2);
    msr!("sp_el1", stack_top);
    msr!("sp_el0", stack_top);

    uart::puts("[exo] eret to EL1 kernel (PC=");
    uart::hex(entry_pc);
    uart::puts(", SP=");
    uart::hex(stack_top);
    uart::puts(", BootInfo=");
    uart::hex(boot_info_ptr);
    uart::puts(", next=");
    uart::hex(next_pc);
    uart::puts(", SPSR_EL2=");
    uart::hex(spsr_el2);
    uart::puts(")\r\n");

    unsafe {
        core::arch::asm!(
            "mov x0, {boot_info}",
            "mov x1, {next}",
            "eret",
            boot_info = in(reg) boot_info_ptr,
            next = in(reg) next_pc,
            options(noreturn)
        );
    }
}

/// EL1 外核进入 EL0 libOS。
pub fn enter_el0(entry_pc: u64, stack_top: u64, arg0: u64) -> ! {
    msr!("elr_el1", entry_pc);
    msr!("spsr_el1", 0u64);
    msr!("sp_el0", stack_top);

    uart::puts("[exo] eret to EL0 libOS (PC=");
    uart::hex(entry_pc);
    uart::puts(", SP=");
    uart::hex(stack_top);
    uart::puts(", x0=");
    uart::hex(arg0);
    uart::puts(")\r\n");

    unsafe {
        core::arch::asm!(
            "mov x0, {arg0}",
            "eret",
            arg0 = in(reg) arg0,
            options(noreturn)
        );
    }
}

#[no_mangle]
pub extern "C" fn el2_sync_handler(arg0: u64) {
    let esr = mrs!("esr_el2");
    let elr = mrs!("elr_el2");
    let far = mrs!("far_el2");
    let ec = (esr >> 26) & 0x3f;

    if ec == 0x16 {
        uart::puts("\r\n*** [EL2] HVC from EL1 reached, ELR_EL2=");
        uart::hex(elr);
        uart::puts(" ESR_EL2=");
        uart::hex(esr);
        uart::puts(" x2=");
        uart::hex(arg0);
        uart::puts(" ***\r\n");
        // AArch64 HVC 进入 EL2 时, ELR_EL2 已经指向 HVC 后的下一条指令。
        // 不能再 +4, 否则会跳过 trampoline 中紧随其后的调试指令。
        msr!("elr_el2", elr);
        return;
    }

    uart::puts("\r\n!!! [EL2] unexpected sync exception !!!\r\n");
    uart::puts("  ESR_EL2=");
    uart::hex(esr);
    uart::puts(" EC=");
    uart::hex(ec);
    uart::puts("\r\n  ELR_EL2=");
    uart::hex(elr);
    uart::puts("\r\n  FAR_EL2=");
    uart::hex(far);
    uart::puts("\r\n");
    loop {}
}

#[no_mangle]
pub extern "C" fn el2_irq_handler() {
    uart::puts("\r\n!!! [EL2] unexpected IRQ !!!\r\n");
    loop {}
}

#[no_mangle]
pub extern "C" fn el1_sync_handler(arg0: u64, arg1: u64, arg2: u64, sysno: u64) -> u64 {
    let esr = mrs!("esr_el1");
    let elr = mrs!("elr_el1");
    let far = mrs!("far_el1");
    let ec = (esr >> 26) & 0x3f;

    match ec {
        0x15 => handle_svc(elr, sysno, arg0, arg1, arg2),
        0x20 => {
            uart::puts("\r\n!!! [EL1] EL0 Instruction Abort !!!\r\n");
            print_el1_exception(esr, elr, far, ec);
            loop {}
        }
        0x24 => {
            uart::puts("\r\n!!! [EL1] EL0 Data Abort !!!\r\n");
            print_el1_exception(esr, elr, far, ec);
            loop {}
        }
        _ => {
            uart::puts("\r\n!!! [EL1] sync exception !!!\r\n");
            print_el1_exception(esr, elr, far, ec);
            loop {}
        }
    }
}

#[no_mangle]
pub extern "C" fn el1_irq_handler() {
    uart::puts("\r\n!!! [EL1] IRQ !!!\r\n");
}

fn handle_svc(elr: u64, sysno: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    msr!("elr_el1", elr + 4);

    match sysno {
        SYS_PUTC => {
            uart::putc(arg0 as u8);
            0
        }
        SYS_PUTS => {
            print_user_str(arg0, arg1);
            0
        }
        SYS_MAP_MMIO => sys_map_mmio(arg0, arg1, arg2),
        SYS_EXIT => {
            uart::puts("\r\n[exo] EL0 exit code=");
            uart::hex(arg0);
            uart::puts("\r\n");
            loop {}
        }
        _ => {
            uart::puts("\r\n[exo] unknown SVC sysno=");
            uart::hex(sysno);
            uart::puts("\r\n");
            u64::MAX
        }
    }
}

fn sys_map_mmio(paddr: u64, size: u64, user_va: u64) -> u64 {
    if (paddr & 0xfff) != 0 || (user_va & 0xfff) != 0 || size == 0 {
        return 1;
    }

    // 用 lookup 而不是 contains_range:
    // MMIO 设备的 reg 通常不到 4KB (比如 UART 只有 0x200),
    // 但页表映射必须按 4KB 对齐。contains_range 要求整个
    // [paddr, paddr+rounded_size) 都在已注册区域内, 会在 UART
    // 等小设备上误杀。lookup 只需要 paddr 落在某个已注册 MMIO
    // 区域内即可——这才是外核的正确语义: "这个物理地址是 MMIO 吗?"
    let size_rounded = (size + 4095) & !4095;
    if crate::protect::lookup(paddr).is_none() {
        uart::puts("\r\n[exo] SYS_MAP_MMIO denied paddr=");
        uart::hex(paddr);
        uart::puts(" size=");
        uart::hex(size);
        uart::puts("\r\n");
        return 2;
    }

    let root = crate::mmu::active_table();
    if root == 0 {
        return 3;
    }

    crate::mmu::map(
        root,
        user_va,
        paddr,
        crate::mmu::MMU_USER_DEV,
        size_rounded / 4096,
    );
    crate::mmu::flush_el1_tlb();

    uart::puts("\r\n[exo] SYS_MAP_MMIO pa=");
    uart::hex(paddr);
    uart::puts(" va=");
    uart::hex(user_va);
    uart::puts(" size=");
    uart::hex(size_rounded);
    uart::puts("\r\n");
    0
}

fn print_user_str(ptr: u64, len: u64) {
    let mut i = 0;
    while i < len {
        let ch = unsafe { core::ptr::read_volatile((ptr + i) as *const u8) };
        uart::putc(ch);
        i += 1;
    }
}

fn print_el1_exception(esr: u64, elr: u64, far: u64, ec: u64) {
    uart::puts("  ESR_EL1=");
    uart::hex(esr);
    uart::puts(" EC=");
    uart::hex(ec);
    uart::puts("\r\n  ELR_EL1=");
    uart::hex(elr);
    uart::puts("\r\n  FAR_EL1=");
    uart::hex(far);
    uart::puts("\r\n");
}
