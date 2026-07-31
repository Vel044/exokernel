//! EL切换、异常入口和系统调用分发。
//!
//! 当前启动链:
//!   EL2 boot shim -> enter_el1_kernel() -> EL1 外核
//!   EL1 外核      -> enter_el0()        -> EL0 libOS
//!
//! xHCI驱动涉及的保护边界：
//!
//! ```text
//! SYS_MAP_MMIO   校验grant并建立Device页表映射
//! SYS_DMA_ALLOC  分配连续物理页并建立Non-cacheable映射
//! SYS_IRQ_BIND   校验INTID并把GIC SPI绑定到Notification
//! NOTIFY_WAIT    只阻塞当前驱动线程
//! SYS_IRQ_ACK    驱动消费Event Ring后重新允许SPI
//! ```
//!
//! Kernel不解析PCI配置寄存器、xHCI TRB、USB descriptor或FTDI数据包。
//! 这些对象都由EL0在获授权映射上直接管理。

use crate::uart;
use exo_abi::{
    SYS_DMA_ALLOC, SYS_DMA_FREE, SYS_ENDPOINT_CALL, SYS_ENDPOINT_CREATE, SYS_ENDPOINT_DESTROY,
    SYS_ENDPOINT_RECV, SYS_ENDPOINT_REPLY, SYS_ENDPOINT_REPLY_RECV, SYS_ENDPOINT_SEND, SYS_EXIT,
    SYS_FRAME_ALLOC, SYS_FRAME_FREE, SYS_FRAME_MAP, SYS_FRAME_UNMAP, SYS_IRQ_ACK, SYS_IRQ_BIND,
    SYS_IRQ_UNBIND, SYS_MAP_MMIO, SYS_NOTIFICATION_CREATE, SYS_NOTIFICATION_DESTROY,
    SYS_NOTIFICATION_POLL, SYS_NOTIFICATION_SIGNAL, SYS_NOTIFICATION_WAIT, SYS_PUTS,
    SYS_THREAD_CREATE, SYS_THREAD_EXIT, SYS_THREAD_RUNTIME, SYS_THREAD_SET_PRIORITY,
    SYS_THREAD_YIELD, SYS_UNMAP_MMIO,
};

#[derive(Clone, Copy)]
#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub sp_el0: u64,
    pub elr_el1: u64,
    pub spsr_el1: u64,
    /// EL0的32个128位FP/SIMD寄存器。Rust/LLVM会用NEON搬运结构体并
    /// 保存普通局部变量；SVC和1ms抢占都必须完整保存，不能只保护GPR。
    pub q: [u128; 32],
}

impl TrapFrame {
    pub const ZERO: Self = Self {
        x: [0; 31],
        sp_el0: 0,
        elr_el1: 0,
        spsr_el1: 0,
        q: [0; 32],
    };
}

// 异常向量汇编硬编码这些偏移；结构布局变化必须在编译期失败，不能等到
// 抢占后才以随机寄存器损坏暴露。
const _: () = assert!(core::mem::offset_of!(TrapFrame, q) == 272);
const _: () = assert!(core::mem::size_of::<TrapFrame>() == 784);

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
    // 允许EL1读取physical counter并使用CNTP_*定时器。时间片抢占由
    // EL1负责，不能让这些访问继续trap回仅用于启动的EL2 shim。
    msr!("cnthctl_el2", mrs!("cnthctl_el2") | 0b11);
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
    // vectors.S已经把EL0的x0..x30、q0..q31、SP、返回PC和PSTATE保存进TrapFrame。
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
    // SVC编号位于保存的x8，参数位于x0..x4。分发完成后返回的TrapFrame
    // 会由异常返回汇编恢复，并通过eret回到EL0的SVC下一条指令。
    handle_svc_frame(frame)
}

#[no_mangle]
pub extern "C" fn el1_irq_handler_frame(frame: *mut TrapFrame) -> *mut TrapFrame {
    let frame = unsafe { &mut *frame };
    if let Some(iar_token) = crate::gic::acknowledge() {
        let intid = crate::gic::intid(iar_token);
        if intid == crate::gic::SGI_RESCHEDULE {
            // SGI0只表示“本核Ready集合发生变化”。先EOI，再由调度器判断：
            // 从EL0被打断时可能抢占低优先级线程；从EL1 idle被打断时可直接
            // 返回新线程保存的TrapFrame，异常返回汇编随后eret进入该EL0线程。
            crate::gic::write_eoir(iar_token);
            if frame.spsr_el1 & 0xf == 0 {
                return crate::scheduler::preempt_if_needed(frame);
            }
            return crate::scheduler::priority::activate_if_idle(frame);
        } else if intid == crate::gic::SGI_TLB_SHOOTDOWN {
            // 当前映射路径使用体系结构广播TLBI；保留该SGI处理入口，便于
            // 后续加入按ASID/VA的软件shootdown mailbox。
            crate::mmu::flush_el1_tlb_local();
            crate::gic::write_eoir(iar_token);
        } else if intid == crate::gic::SGI_TASK_STOP {
            crate::gic::write_eoir(iar_token);
            // v1只有一个共享VSpace和一个任务。目标核一旦确认SGI2便永久
            // 停在EL1，发起退出的调用核随后才可以撤销页表并释放线程栈/DMA。
            crate::arch::aarch64::smp::acknowledge_stop_and_park();
        } else if crate::scheduler::timer::is_timer_irq(intid) {
            crate::scheduler::timer::disarm();
            crate::gic::write_eoir(iar_token);
            // M[3:0]=0表示异常来自EL0t。所有线程Blocked时Kernel会在
            // EL1 WFI并短暂打开IRQ；此时若收到旧pending timer，只完成
            // 中断，绝不能把EL1临时栈帧保存成某个EL0线程上下文。
            if frame.spsr_el1 & 0xf == 0 {
                return crate::scheduler::on_timer(frame);
            }
        } else if crate::task::has_binding(intid) {
            // IRQHandler 只负责接收并暂时屏蔽该 SPI；真正的设备处理仍在
            // EL0线程里完成。Notification模式必须在这里先EOI，否则当前
            // active设备IRQ会阻挡Generic Timer，已唤醒的驱动线程无法获得CPU。
            crate::gic::disable_spi(intid);
            if let Some((notification, badge)) = crate::task::irq_notification(intid) {
                crate::gic::write_eoir(iar_token);
                crate::task::record_irq_pending(intid);
                let _ = crate::ipc::notification_signal(notification, badge);
                // Notification可能唤醒本核更高优先级线程。设备协议仍完全在
                // EL0处理，Kernel这里只决定是否切换TrapFrame。
                if frame.spsr_el1 & 0xf == 0 {
                    return crate::scheduler::preempt_if_needed(frame);
                }
                return crate::scheduler::priority::activate_if_idle(frame);
            } else {
                // 正常绑定必定有Notification。若对象表意外不一致，仍需EOI，
                // 否则GIC会一直保留active状态并阻塞后续中断。
                crate::gic::write_eoir(iar_token);
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
        SYS_THREAD_CREATE => finish(frame, crate::thread::create(arg0, arg1, arg2, arg3, arg4)),
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
        SYS_ENDPOINT_SEND => {
            let result = crate::ipc::endpoint_send(arg0, frame);
            ipc_result(frame, result)
        }
        SYS_ENDPOINT_RECV => {
            let result = crate::ipc::endpoint_recv(arg0, frame);
            ipc_result(frame, result)
        }
        SYS_ENDPOINT_CALL => {
            let result = crate::ipc::endpoint_call(arg0, frame);
            ipc_result(frame, result)
        }
        SYS_ENDPOINT_REPLY => {
            let result = crate::ipc::endpoint_reply(arg0, frame);
            ipc_result(frame, result)
        }
        SYS_ENDPOINT_REPLY_RECV => {
            let result = crate::ipc::endpoint_reply_recv(arg0, arg1, frame);
            ipc_result(frame, result)
        }
        SYS_ENDPOINT_DESTROY => finish(frame, crate::ipc::endpoint_destroy(arg0)),
        SYS_NOTIFICATION_CREATE => finish(frame, crate::ipc::notification_create()),
        SYS_NOTIFICATION_SIGNAL => {
            let result = crate::ipc::notification_signal(arg0, arg1);
            frame.x[0] = result;
            if result == 0 {
                // Signal可能让本核更高优先级线程从Blocked变为Ready；静态
                // 优先级语义要求在返回发送者之前立即检查抢占。
                crate::scheduler::preempt_if_needed(frame)
            } else {
                frame
            }
        }
        SYS_NOTIFICATION_WAIT => {
            let result = crate::ipc::notification_wait(arg0, frame);
            ipc_result(frame, result)
        }
        SYS_NOTIFICATION_POLL => finish(frame, crate::ipc::notification_poll(arg0)),
        SYS_NOTIFICATION_DESTROY => finish(frame, crate::ipc::notification_destroy(arg0)),
        SYS_THREAD_RUNTIME => finish(frame, crate::thread::runtime(arg0)),
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
        // 驱动传入的PA、VA、size和INTID全部不可信，具体函数必须再次
        // 对照Kernel保存的task grant和对象owner进行检查。
        SYS_MAP_MMIO => sys_map_mmio(arg0, arg1, arg2),
        SYS_EXIT => crate::task::exit_current(arg0),
        SYS_IRQ_BIND => sys_irq_bind(arg0, arg1, arg2, arg3),
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
    // 物理地址和目标VA必须页对齐；size随后向上取整到完整页。
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

    // 取得当前单VSpace的stage-1根页表。EL0只给出“想映射到哪里”，
    // 无法提供或修改页表本身。
    let root = crate::mmu::active_table();
    if root == 0 {
        return 3;
    }
    if !crate::task::can_record_mmio() {
        return 4;
    }

    let pages = size_rounded / 4096;
    // 防止覆盖ELF、heap、stack、BootInfo、DMA arena或已有设备映射。
    if !crate::task::range_available(user_va, pages) {
        return 5;
    }

    // MMU_USER_DEV设置EL0可访问、不可执行和Device-nGnRE属性。之后EL0的
    // volatile load/store经过页表翻译，直接到达物理设备寄存器。
    crate::mmu::map(root, user_va, paddr, crate::mmu::MMU_USER_DEV, pages);
    // 页表改变后使旧TLB缓存失效，再把映射登记到task资源表供unmap/exit回收。
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
    // DMA接口比普通Frame多出“连续、设备可见、指定对齐”的要求。
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
    // 物理分配器返回连续PA。EL0不能指定PA，因此不能借DMA接口映射任意内存。
    let Some(pa) = crate::mem::alloc_pages_aligned(pages, align_pages.next_power_of_two()) else {
        return u64::MAX;
    };
    // 清零避免把上一个所有者的数据泄露给当前libOS或设备。
    unsafe { core::ptr::write_bytes(pa as *mut u8, 0, size_rounded as usize) };
    if !crate::task::record_dma(user_va, pa, pages) {
        crate::mem::free_pages(pa, pages);
        return u64::MAX;
    }
    // CPU端映射为Normal Non-cacheable；xHCI端在v1无IOMMU时直接使用PA。
    crate::mmu::map(
        crate::mmu::active_table(),
        user_va,
        pa,
        crate::mmu::MMU_USER_DMA,
        pages,
    );
    crate::mmu::flush_el1_tlb();
    // 返回给EL0的是DMA address，不是允许任意物理访问的FrameHandle。
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

fn sys_irq_bind(intid: u64, notification: u64, badge: u64, target_cpu: u64) -> u64 {
    if intid > u32::MAX as u64 {
        return 1;
    }
    let intid = intid as u32;
    let target_cpu = if target_cpu == exo_abi::IRQ_TARGET_CURRENT {
        crate::arch::aarch64::cpu::id()
    } else {
        target_cpu as usize
    };
    if target_cpu >= crate::arch::aarch64::smp::cpu_count() {
        return exo_abi::SYS_ERR_INVALID;
    }
    if intid < 32 {
        // 只允许 SPI (INTID >= 32)
        uart::puts("\r\n[exo] SYS_IRQ_BIND rejected: INTID must be >= 32 (got ");
        uart::hex(intid as u64);
        uart::puts(")\r\n");
        return 1;
    }
    // INTID必须来自EL1根据DTB/PCI INTx建立的当前任务IRQ grant。
    // UserBootInfo中看见一个数字并不等于可以绑定任意GIC中断。
    let Some(flags) = crate::task::irq_flags(intid) else {
        uart::puts("\r\n[exo] SYS_IRQ_BIND rejected: IRQ not granted to task\r\n");
        return 2;
    };
    // Handle中包含slot+generation；notification_valid同时校验对象存在、
    // generation未过期且owner是当前任务。
    if notification == 0 || !crate::ipc::notification_valid(notification) || badge == 0 {
        return exo_abi::SYS_ERR_INVALID;
    }
    if !crate::task::bind_irq(intid, notification, badge) {
        uart::puts("\r\n[exo] SYS_IRQ_BIND failed: already bound or full\r\n");
        return 3;
    }

    // GIC DT binding: 1/2 表示边沿，4/8 表示电平；缺省按电平处理。
    let trigger_level = flags & 0x3 == 0;
    // GIC MMIO永远留在EL1。驱动只能请求绑定，不能直接改Distributor。
    crate::gic::configure_spi(intid, trigger_level, target_cpu);
    // Notification绑定成功后直接允许该SPI进入EL1 IRQ handler；handler
    // 会在投递badge前暂时禁用它，直到用户完成设备处理并执行IRQ_ACK。
    crate::gic::enable_spi(intid);

    uart::puts("\r\n[exo] SYS_IRQ_BIND intid=");
    uart::hex(intid as u64);
    uart::puts(" target_cpu=");
    uart::hex(target_cpu as u64);
    uart::puts("\r\n");
    0
}

fn sys_irq_ack(intid: u64) -> u64 {
    if intid > u32::MAX as u64 {
        return 1;
    }
    let intid = intid as u32;
    if !crate::task::ack_irq(intid) {
        uart::puts("\r\n[exo] SYS_IRQ_ACK failed: no active IRQ or INTID mismatch (got ");
        uart::hex(intid as u64);
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
    uart::puts("  CPU=");
    uart::hex(crate::arch::aarch64::cpu::id() as u64);
    uart::puts("\r\n");
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
