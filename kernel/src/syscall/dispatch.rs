//! trap.rs —— EL 切换与异常处理
//!
//! 当前启动链:
//!   EL2 boot shim -> enter_el1_kernel() -> EL1 外核
//!   EL1 外核      -> enter_el0()        -> EL0 libOS

use crate::uart;

pub const SYS_PUTS: u64 = 2;
pub const SYS_EXIT: u64 = 5;
pub const SYS_MAP_MMIO: u64 = 6;
pub const SYS_IRQ_BIND: u64 = 7;
pub const SYS_IRQ_WAIT: u64 = 8;
pub const SYS_IRQ_ACK: u64 = 9;
pub const SYS_IRQ_UNBIND: u64 = 10;
pub const SYS_UNMAP_MMIO: u64 = 11;
pub const SYS_DMA_ALLOC: u64 = 12;
pub const SYS_DMA_FREE: u64 = 13;
pub const SYS_FRAME_ALLOC: u64 = 14;
pub const SYS_FRAME_FREE: u64 = 15;
pub const SYS_FRAME_MAP: u64 = 16;
pub const SYS_FRAME_UNMAP: u64 = 17;
pub const SYS_THREAD_CREATE: u64 = 18;
pub const SYS_THREAD_EXIT: u64 = 19;
pub const SYS_THREAD_YIELD: u64 = 20;
pub const SYS_THREAD_SET_PRIORITY: u64 = 21;
pub const SYS_ENDPOINT_CREATE: u64 = 22;
pub const SYS_ENDPOINT_SEND: u64 = 23;
pub const SYS_ENDPOINT_RECV: u64 = 24;
pub const SYS_ENDPOINT_CALL: u64 = 25;
pub const SYS_ENDPOINT_REPLY: u64 = 26;
pub const SYS_ENDPOINT_REPLY_RECV: u64 = 27;
pub const SYS_NOTIFICATION_CREATE: u64 = 28;
pub const SYS_NOTIFICATION_SIGNAL: u64 = 29;
pub const SYS_NOTIFICATION_WAIT: u64 = 30;
pub const SYS_NOTIFICATION_POLL: u64 = 31;
pub const SYS_NOTIFICATION_DESTROY: u64 = 32;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub sp_el0: u64,
    pub elr_el1: u64,
    pub spsr_el1: u64,
}

impl TrapFrame {
    pub const ZERO: Self = Self {
        x: [0; 31],
        sp_el0: 0,
        elr_el1: 0,
        spsr_el1: 0,
    };
}

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
    // IMO/FMO/AMO (bits 4/3/5) = 1: 把物理 IRQ/FIQ/SError 路由到 EL2。
    // EL2 只是 boot shim，所以三个位必须清零，让设备 IRQ 进入 EL1 外核。
    //
    // Pi5 UEFI 给的 HCR_EL2=0x88000000, 也就是 RW=1 且 TGE=1。
    // 如果保留 TGE, eret 到 EL1 会触发 EC=0x0e Illegal Execution state。
    let new_hcr = (old_hcr | (1u64 << 31))
        & !((1u64 << 27) | (1u64 << 20) | (1u64 << 5) | (1u64 << 4) | (1u64 << 3));
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
    msr!(
        "sctlr_el1",
        old_sctlr_el1 & !((1u64 << 1) | (1u64 << 3) | (1u64 << 4))
    );

    // 原设计: EL1 初始先关 MMU, 同时清掉固件可能留下的 alignment/stack alignment 检查。
    //
    // M  = bit0: stage-1 MMU enable
    // A  = bit1: 普通对齐检查
    // SA = bit3: EL1 SP 对齐检查
    // SA0= bit4: EL0 SP 对齐检查
    unsafe {
        core::arch::asm!("isb", options(nomem, nostack));
    }
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
    // 允许 EL0 读取虚拟计数器 CNTVCT_EL0，供无 syscall 的短延时/超时使用。
    msr!("cntkctl_el1", mrs!("cntkctl_el1") | (1u64 << 1));
    // CrabUSB 的 dma-api 会在 EL0 读取 CTR_EL0，并执行 DC IVAC/CIVAC
    // 计算和维护 cache line。UCT=1 的语义是 trap CTR_EL0，因此必须清零；
    // UCI=1 才是允许 EL0 执行 cache maintenance。二者都不增加物理映射权限。
    let sctlr = (mrs!("sctlr_el1") & !(1u64 << 15)) | (1u64 << 26);
    msr!("sctlr_el1", sctlr);
    unsafe { core::arch::asm!("isb", options(nomem, nostack)) };
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
pub extern "C" fn el1_sync_handler_frame(frame: *mut TrapFrame) -> *mut TrapFrame {
    let frame = unsafe { &mut *frame };
    let esr = mrs!("esr_el1");
    let far = mrs!("far_el1");
    let ec = (esr >> 26) & 0x3f;
    if ec != 0x15 {
        if ec == 0x20 {
            uart::puts("\r\n!!! [EL1] EL0 Instruction Abort !!!\r\n");
        } else if ec == 0x24 {
            uart::puts("\r\n!!! [EL1] EL0 Data Abort !!!\r\n");
        } else {
            uart::puts("\r\n!!! [EL1] sync exception !!!\r\n");
        }
        print_el1_exception(esr, frame.elr_el1, far, ec);
        crate::task::exit_current(0x400);
    }
    // AArch64 SVC 异常进入 EL1 时，ELR_EL1 已指向 SVC 后的下一条指令。
    // 返回地址不能再次增加，否则会跳过用户态系统调用封装中的一条指令。
    handle_svc_frame(frame)
}

#[no_mangle]
pub extern "C" fn el1_irq_handler_frame(frame: *mut TrapFrame) -> *mut TrapFrame {
    if let Some(iar_token) = crate::gic::acknowledge() {
        let intid = crate::gic::intid(iar_token);
        if crate::task::has_binding(intid) {
            // IRQHandler 只负责接收并暂时屏蔽该 SPI；真正的设备处理仍在
            // EL0 线程里完成，ACK 时才写回原始 IAR token。
            crate::gic::disable_spi(intid);
            crate::task::record_iar(iar_token);
            if let Some((notification, badge)) = crate::task::irq_notification(intid) {
                let _ = crate::ipc::notification_signal(notification, badge);
            }
        } else {
            crate::gic::write_eoir(iar_token);
        }
    }
    frame
}

fn finish(frame: &mut TrapFrame, result: u64) -> *mut TrapFrame {
    frame.x[0] = result;
    frame
}

fn ipc_result(frame: &mut TrapFrame, result: Result<*mut TrapFrame, u64>) -> *mut TrapFrame {
    match result {
        Ok(next) => next,
        Err(error) => finish(frame, error),
    }
}

fn handle_svc_frame(frame: &mut TrapFrame) -> *mut TrapFrame {
    let sysno = frame.x[8];
    let arg0 = frame.x[0];
    let arg1 = frame.x[1];
    let arg2 = frame.x[2];
    let arg3 = frame.x[3];
    let arg4 = frame.x[4];

    match sysno {
        SYS_THREAD_CREATE => finish(frame, crate::thread::create(arg0, arg1, arg2)),
        SYS_THREAD_EXIT => match crate::thread::exit_current(frame) {
            Some(next) => next,
            None => crate::task::exit_current(arg0),
        },
        SYS_THREAD_YIELD => {
            frame.x[0] = 0;
            crate::thread::yield_current(frame)
        }
        SYS_THREAD_SET_PRIORITY => finish(frame, crate::thread::set_priority(arg0, arg1)),
        SYS_ENDPOINT_CREATE => finish(frame, crate::ipc::endpoint_create()),
        SYS_ENDPOINT_SEND => ipc_result(frame, crate::ipc::endpoint_send(arg0, frame)),
        SYS_ENDPOINT_RECV => ipc_result(frame, crate::ipc::endpoint_recv(arg0, frame)),
        SYS_ENDPOINT_CALL => ipc_result(frame, crate::ipc::endpoint_call(arg0, frame)),
        SYS_ENDPOINT_REPLY => ipc_result(frame, crate::ipc::endpoint_reply(arg0, frame)),
        SYS_ENDPOINT_REPLY_RECV => {
            ipc_result(frame, crate::ipc::endpoint_reply_recv(arg0, arg1, frame))
        }
        SYS_NOTIFICATION_CREATE => finish(frame, crate::ipc::notification_create()),
        SYS_NOTIFICATION_SIGNAL => finish(frame, crate::ipc::notification_signal(arg0, arg1)),
        SYS_NOTIFICATION_WAIT => {
            let result = crate::ipc::notification_wait(arg0, frame);
            ipc_result(frame, result)
        }
        SYS_NOTIFICATION_POLL => finish(frame, crate::ipc::notification_poll(arg0)),
        SYS_NOTIFICATION_DESTROY => finish(frame, crate::ipc::notification_destroy(arg0)),
        _ => {
            let result = handle_svc(frame.elr_el1, sysno, arg0, arg1, arg2, arg3, arg4);
            finish(frame, result)
        }
    }
}

fn handle_svc(elr: u64, sysno: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    msr!("elr_el1", elr);

    match sysno {
        SYS_PUTS => {
            print_user_str(arg0, arg1);
            0
        }
        SYS_MAP_MMIO => sys_map_mmio(arg0, arg1, arg2),
        SYS_EXIT => crate::task::exit_current(arg0),
        SYS_IRQ_BIND => sys_irq_bind(arg0, arg1, arg2),
        SYS_IRQ_WAIT => sys_irq_wait(),
        SYS_IRQ_ACK => sys_irq_ack(arg0),
        SYS_IRQ_UNBIND => sys_irq_unbind(arg0),
        SYS_UNMAP_MMIO => sys_unmap_mmio(arg0, arg1),
        SYS_DMA_ALLOC => sys_dma_alloc(arg0, arg1, arg2),
        SYS_DMA_FREE => sys_dma_free(arg0, arg1),
        SYS_FRAME_ALLOC => crate::frame::allocate(crate::task::current_owner(), arg0, arg1),
        SYS_FRAME_FREE => crate::frame::free(crate::task::current_owner(), arg0),
        SYS_FRAME_MAP => {
            crate::vspace::map_frame(crate::task::current_owner(), arg0, arg1, arg2, arg3, arg4)
        }
        SYS_FRAME_UNMAP => crate::vspace::unmap_handle(crate::task::current_owner(), arg0),
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

    // 授权表在登记设备时已扩展到完整物理页，因此这里可以校验
    // page-rounded 后的完整范围，不能只检查起始物理地址。
    let Some(size_rounded) = size.checked_add(4095).map(|value| value & !4095) else {
        return 1;
    };
    if !crate::protect::contains_range(paddr, size_rounded)
        || !crate::task::is_mmio_granted(paddr, size_rounded)
    {
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
    if !crate::task::can_record_mmio() {
        return 4;
    }

    let pages = size_rounded / 4096;
    if !crate::task::range_available(user_va, pages) {
        return 5;
    }
    crate::mmu::map(root, user_va, paddr, crate::mmu::MMU_USER_DEV, pages);
    crate::mmu::flush_el1_tlb();
    crate::task::record_mmio(user_va, pages);

    uart::puts("\r\n[exo] SYS_MAP_MMIO pa=");
    uart::hex(paddr);
    uart::puts(" va=");
    uart::hex(user_va);
    uart::puts(" size=");
    uart::hex(size_rounded);
    uart::puts("\r\n");
    0
}

fn sys_unmap_mmio(user_va: u64, size: u64) -> u64 {
    if (user_va & 0xfff) != 0 || size == 0 {
        return 1;
    }
    let Some(size_rounded) = size.checked_add(4095).map(|value| value & !4095) else {
        return 1;
    };
    let pages = size_rounded / 4096;
    if !crate::task::remove_mmio(user_va, pages) {
        return 2;
    }
    let root = crate::mmu::active_table();
    if crate::mmu::unmap(root, user_va, pages) != pages {
        return 3;
    }
    crate::mmu::flush_el1_tlb();
    0
}

fn sys_dma_alloc(size: u64, alignment: u64, user_va: u64) -> u64 {
    if size == 0
        || alignment == 0
        || !alignment.is_power_of_two()
        || alignment > 2 * 1024 * 1024
        || (user_va & 0xfff) != 0
    {
        return u64::MAX;
    }
    let Some(size_rounded) = size.checked_add(4095).map(|value| value & !4095) else {
        return u64::MAX;
    };
    let pages = size_rounded / 4096;
    if !crate::task::range_available(user_va, pages) {
        return u64::MAX;
    }
    let align_pages = ((alignment + 4095) / 4096).max(1);
    let Some(pa) = crate::mem::alloc_pages_aligned(pages, align_pages.next_power_of_two()) else {
        return u64::MAX;
    };
    unsafe { core::ptr::write_bytes(pa as *mut u8, 0, size_rounded as usize) };
    if !crate::task::record_dma(user_va, pa, pages) {
        crate::mem::free_pages(pa, pages);
        return u64::MAX;
    }
    crate::mmu::map(
        crate::mmu::active_table(),
        user_va,
        pa,
        crate::mmu::MMU_USER_DMA,
        pages,
    );
    crate::mmu::flush_el1_tlb();
    pa
}

fn sys_dma_free(user_va: u64, size: u64) -> u64 {
    if (user_va & 0xfff) != 0 || size == 0 {
        return 1;
    }
    let Some(size_rounded) = size.checked_add(4095).map(|value| value & !4095) else {
        return 1;
    };
    let pages = size_rounded / 4096;
    let Some(pa) = crate::task::take_dma(user_va, pages) else {
        return 2;
    };
    if crate::mmu::unmap(crate::mmu::active_table(), user_va, pages) != pages {
        return 3;
    }
    crate::mmu::flush_el1_tlb();
    crate::mem::free_pages(pa, pages);
    0
}

// ── IRQ syscalls ──

fn sys_irq_bind(intid: u64, notification: u64, badge: u64) -> u64 {
    if intid > u32::MAX as u64 {
        return 1;
    }
    let intid = intid as u32;
    if intid < 32 {
        // 只允许 SPI (INTID >= 32)
        uart::puts("\r\n[exo] SYS_IRQ_BIND rejected: INTID must be >= 32 (got ");
        uart::hex(intid as u64);
        uart::puts(")\r\n");
        return 1;
    }
    let Some(flags) = crate::task::irq_flags(intid) else {
        uart::puts("\r\n[exo] SYS_IRQ_BIND rejected: IRQ not granted to task\r\n");
        return 2;
    };
    if notification != 0 && (!crate::ipc::notification_valid(notification) || badge == 0) {
        return exo_abi::SYS_ERR_INVALID;
    }
    if !crate::task::bind_irq(intid, notification, badge) {
        uart::puts("\r\n[exo] SYS_IRQ_BIND failed: already bound or full\r\n");
        return 3;
    }

    // GIC DT binding: 1/2 表示边沿，4/8 表示电平；缺省按电平处理。
    let trigger_level = flags & 0x3 == 0;
    crate::gic::configure_spi(intid, trigger_level);
    if notification != 0 {
        // Notification 绑定不经过旧的 SYS_IRQ_WAIT，因此绑定成功后直接
        // 允许该 SPI 进入 EL1 IRQ handler；handler 会在 ACK 前暂时禁用它。
        crate::gic::enable_spi(intid);
    }

    uart::puts("\r\n[exo] SYS_IRQ_BIND intid=");
    uart::hex(intid as u64);
    uart::puts("\r\n");
    0
}

fn sys_irq_wait() -> u64 {
    if crate::task::has_active_iar() {
        uart::puts("\r\n[exo] SYS_IRQ_WAIT rejected: previous IRQ not ACKed\r\n");
        return u64::MAX;
    }

    if crate::task::first_bound_intid().is_none() {
        uart::puts("\r\n[exo] SYS_IRQ_WAIT rejected: no bound IRQ\r\n");
        return u64::MAX;
    }

    unsafe {
        core::arch::asm!("msr daifset, #2", options(nomem, nostack));
    }
    crate::task::enable_bound_irqs();

    // 保持 PSTATE.I=1。ARM 的 WFI 会被已送达的物理 IRQ 唤醒，即使 IRQ
    // 在 PSTATE 中被屏蔽；醒来后由当前同步异常上下文主动读取 GICC_IAR。
    // 这样不存在“解屏蔽后、执行 WFI 前”被抢占导致错过唤醒的窗口。
    let result: u64;
    loop {
        unsafe {
            core::arch::asm!("msr daifset, #2", "dsb sy", "wfi", options(nomem, nostack));
        }

        if let Some(iar_token) = crate::gic::acknowledge() {
            let pending_intid = crate::gic::intid(iar_token);
            if crate::task::has_binding(pending_intid) {
                crate::task::record_iar(iar_token);
                result = pending_intid as u64;
                break;
            }
            crate::gic::write_eoir(iar_token);
        }
    }

    crate::task::disable_bound_irqs();

    result
}

fn sys_irq_ack(intid: u64) -> u64 {
    if intid > u32::MAX as u64 {
        return 1;
    }
    let intid = intid as u32;
    if !crate::task::ack_irq(intid) {
        uart::puts("\r\n[exo] SYS_IRQ_ACK failed: no active IRQ or INTID mismatch (got ");
        uart::hex(intid as u64);
        uart::puts(" active=");
        uart::hex(crate::task::active_iar() as u64);
        uart::puts(")\r\n");
        return 1;
    }

    0
}

fn sys_irq_unbind(intid: u64) -> u64 {
    if intid > u32::MAX as u64 {
        return 1;
    }
    let intid = intid as u32;
    if !crate::task::unbind_irq(intid) {
        uart::puts("\r\n[exo] SYS_IRQ_UNBIND: not bound (intid=");
        uart::hex(intid as u64);
        uart::puts(")\r\n");
        return 1;
    }
    uart::puts("\r\n[exo] SYS_IRQ_UNBIND intid=");
    uart::hex(intid as u64);
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
