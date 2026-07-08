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
pub fn enter_el1_kernel(entry_pc: u64, stack_top: u64, boot_info_ptr: u64) -> ! {
    msr!("hcr_el2", 1u64 << 31);
    // EL1 初始先关 MMU, 同时清掉固件可能留下的 alignment/stack alignment 检查。
    //
    // M  = bit0: stage-1 MMU enable
    // A  = bit1: 普通对齐检查
    // SA = bit3: EL1 SP 对齐检查
    // SA0= bit4: EL0 SP 对齐检查
    msr!("sctlr_el1", mrs!("sctlr_el1") & !((1u64 << 0) | (1u64 << 1) | (1u64 << 3) | (1u64 << 4)));
    unsafe { core::arch::asm!("isb", options(nomem, nostack)); }
    msr!("elr_el2", entry_pc);
    msr!("spsr_el2", 0x3c5u64);
    msr!("sp_el1", stack_top);

    uart::puts("[exo] eret to EL1 kernel (PC=");
    uart::hex(entry_pc);
    uart::puts(", SP=");
    uart::hex(stack_top);
    uart::puts(", BootInfo=");
    uart::hex(boot_info_ptr);
    uart::puts(")\r\n");

    unsafe {
        core::arch::asm!(
            "mov x0, {boot_info}",
            "eret",
            boot_info = in(reg) boot_info_ptr,
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
pub extern "C" fn el2_sync_handler() {
    let esr = mrs!("esr_el2");
    let elr = mrs!("elr_el2");
    let far = mrs!("far_el2");
    let ec = (esr >> 26) & 0x3f;

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

    let size_rounded = (size + 4095) & !4095;
    if !crate::protect::contains_range(paddr, size_rounded) {
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
