//! kmain —— EL1 外核主逻辑
//!
//! EL2 boot shim 只负责 UEFI/EBS 和 eret。这里开始才是真正外核:
//! 初始化物理页分配器、装载平台配置、安装 EL1 异常向量、建立 stage-1 页表、
//! 加载 EL0 libOS, 最后 eret 进 EL0。

use crate::boot_info::BootInfo; // EL2 boot shim 收集后传给 EL1 的启动信息
use crate::config::{
    PAGE_SIZE, USER_BASE, USER_BOOT_INFO_VA, USER_STACK_PAGES, USER_STACK_TOP, USER_WINDOW_END,
};
use crate::uart; // PL011 串口输出, 当前所有 bring-up 日志都靠它

// EL0 libOS 的原始机器码。
//
// build.sh 先把 libos ELF 用 rust-objcopy 转成 libos/libos.bin。
// 编译 BOOTAA64.efi 时, include_bytes! 把 libos.bin 直接塞进 .efi 的 rodata。
// 运行到 EL1 后, 这里的 LIBOS_BIN 就是一段内存里的 &[u8], 不是文件系统路径。
static LIBOS_ELF: &[u8] = include_bytes!("../../libos/libos.elf");

#[no_mangle]
pub extern "C" fn el1_main(boot_info_addr: u64) -> ! {
    // 这里已经不是 EL2 了。
    //
    // trap::enter_el1_kernel() 在 EL2 设置了:
    //   ELR_EL2  = el1_main 的地址
    //   SPSR_EL2 = EL1h
    //   SP_EL1   = EL1 boot stack
    //   x0       = BootInfo 指针
    // 然后执行 eret。AArch64 C ABI 规定第一个参数在 x0, 所以这里的
    // boot_info_addr 就是 EL2 传下来的 BootInfo 物理地址。

    crate::arch::aarch64::cpu::set_id(0);
    // 安装 EL1 异常向量表 — 必须尽早, EL0 的 SVC/Abort/IRQ 都走 VBAR_EL1。
    let vbar = crate::vectors::install_el1();

    // 解引用 BootInfo。MMU 还没开, CPU 把指针值直接当物理地址用。
    let bi = unsafe { &*(boot_info_addr as *const BootInfo) };

    // 用 EL2 收集到的 UEFI conventional memory ranges 初始化 EL1 物理页分配器。
    init_allocator_from_bootinfo(bi);

    // DTB 已由 UEFI 复制到独占 LoaderData；EL1 只移交字节和建立只读映射。
    assert!(bi.dtb != 0 && bi.dtb % PAGE_SIZE == 0);
    assert!(bi.dtb_size >= 40 && bi.dtb_size <= exo_abi::USER_DTB_MAX_SIZE);

    // 先建立 Normal identity 映射，再按平台配置映射 EL1 的 UART/GIC。
    let root = crate::mmu::create_table();
    crate::mmu::map_range_2m(
        root,
        0,
        0x1_8000_0000,
        crate::mmu::MMU_KERNEL,
        USER_BASE,
        USER_WINDOW_END,
    );
    crate::mmu::activate(root);

    // 启动与授权来自显式平台配置；EL0 的设备发现结果不能修改授权。
    let platform = &crate::resources::PLATFORM;
    crate::arch::aarch64::smp::set_cpu_count(platform.cpus.count);
    let uart_pa = platform.uart_pa;

    crate::mmu::map_block_2m(
        root,
        uart_pa & !0x1f_ffff,
        uart_pa & !0x1f_ffff,
        crate::mmu::MMU_DEV,
    );
    crate::mmu::flush_el1_tlb();
    uart::init(uart_pa);

    uart::puts("\r\n*** [exo] EL1 kernel alive ***\r\n");
    uart::puts("[exo] VBAR_EL1=");
    uart::hex(vbar);
    uart::puts("\r\n");

    uart::puts("[exo] EL1 mem ready, free=");
    uart::hex(crate::mem::free_pages_total() * 4);
    uart::puts(" KB\r\n");

    uart::puts("[exo] opaque DTB for EL0: pa=");
    uart::hex(bi.dtb);
    uart::puts(" size=");
    uart::hex(bi.dtb_size);
    uart::puts("\r\n");

    // 初始化 GICv2 中断控制器。
    //
    // 从平台配置取得 GIC 的 Distributor 和 CPU Interface 物理地址，
    // 映射为 Device memory 后初始化 GICD/GICC。后续 SYS_IRQ_BIND
    // 等 syscall 依赖 GIC 就绪。
    {
        let gicd_pa = platform.gicd_pa;
        let gicc_pa = platform.gicc_pa;
        // 映射 GICD 和 GICC 所在的 2MB block 为 Device memory。
        // GICD 和 GICC 通常在同一个或相邻的 2MB block 内。
        crate::mmu::map_block_2m(
            root,
            gicd_pa & !0x1f_ffff,
            gicd_pa & !0x1f_ffff,
            crate::mmu::MMU_DEV,
        );
        crate::mmu::map_block_2m(
            root,
            gicc_pa & !0x1f_ffff,
            gicc_pa & !0x1f_ffff,
            crate::mmu::MMU_DEV,
        );
        crate::mmu::flush_el1_tlb();
        crate::gic::init(gicd_pa, gicc_pa);

        // GIC 只能由 EL1 操作。deny 表优先于平台 MMIO 授权。
        crate::protect::deny(gicd_pa, 0x10000);
        crate::protect::deny(gicc_pa, 0x10000);

        // 启动根页表从这一刻起登记为 VSpaceHandle(1)。后续创建的独立
        // VSpace 会复用同一份 Kernel UART/GIC 设备块，但不会继承当前 EL0
        // 的 ELF、heap、stack、MMIO 或 Frame 映射。
        crate::vspace::initialize(root, uart_pa, gicd_pa, gicc_pa);
    }

    // 注册平台授予的设备窗口；GIC 等 EL1 专用区域始终优先拒绝。
    assert!(platform.mmio.len() <= crate::task::MAX_MMIO_GRANTS);
    assert!(platform.irqs.len() <= crate::task::MAX_IRQ_GRANTS);
    for grant in platform.mmio {
        crate::protect::register(grant.base, grant.size);
    }

    // ELF loader 按 PT_LOAD 的 R/W/X 权限分别分配并映射用户段。
    let loaded = crate::elf::load(root, LIBOS_ELF).expect("failed to load EL0 ELF");
    let stack_paddr = crate::mem::alloc_pages(USER_STACK_PAGES).expect("no mem for EL0 stack");
    let user_boot_info_pa = crate::mem::alloc_page().expect("no mem for UserBootInfo");
    let initial_ipc_pa = crate::mem::alloc_page().expect("no mem for initial IPC buffer");

    unsafe {
        core::ptr::write_bytes(
            stack_paddr as *mut u8,
            0,
            (USER_STACK_PAGES * PAGE_SIZE) as usize,
        );
        core::ptr::write_bytes(user_boot_info_pa as *mut u8, 0, PAGE_SIZE as usize);
        (user_boot_info_pa as *mut exo_abi::UserBootInfo).write(exo_abi::UserBootInfo {
            magic: exo_abi::USER_BOOT_INFO_MAGIC,
            version: exo_abi::USER_BOOT_INFO_VERSION,
            size: core::mem::size_of::<exo_abi::UserBootInfo>() as u16,
            dtb_va: exo_abi::USER_DTB_VA,
            dtb_size: bi.dtb_size,
            cpu_count: platform.cpus.count as u32,
        });
    }

    uart::puts("[exo] EL0 libOS pa=");
    uart::hex(loaded.segments[0].pa);
    uart::puts(" entry=");
    uart::hex(loaded.entry);
    uart::puts(" stack_pa=");
    uart::hex(stack_paddr);
    uart::puts(" stack_va=");
    uart::hex(USER_STACK_TOP);
    uart::puts(" segments=");
    uart::hex(loaded.segment_count as u64);
    uart::puts("\r\n");

    // 建 EL1 stage-1 页表。
    //
    // 这个页表同时服务两个目标:
    //   1. EL1 外核自己继续运行: 需要 identity map 当前代码、栈、DTB、UART 等。
    //   2. EL0 libOS 能启动: 需要把 USER_BASE 映射到 code_paddr,
    //      把 USER_STACK_TOP 下方的栈窗口映射到 stack_paddr。
    complete_el1_stage1(root, bi, stack_paddr, user_boot_info_pa, initial_ipc_pa);
    uart::puts("[exo] EL1 stage-1 root=");
    uart::hex(root);
    uart::puts("\r\n");

    // 登记当前 EL0 任务拥有的 RAM 和用户映射。SYS_EXIT 会根据这份记录撤销
    // PTE，并且只回收真正属于任务的代码页和栈页。
    crate::task::install(
        &loaded.segments[..loaded.segment_count],
        stack_paddr,
        user_boot_info_pa,
        platform.mmio,
        platform.irqs,
    );
    crate::ipc::init();
    crate::thread::install_initial(loaded.entry, USER_STACK_TOP, initial_ipc_pa);

    // CPU0完成共享页表、GIC Distributor和任务对象初始化后，才允许辅助核
    // 进入共享调度器。PSCI context_id携带逻辑CPU编号，辅助核使用独立EL1栈。
    // 任一目标核未在一秒内置online位即停止启动，避免伪装成可用的四核系统。
    // 初始固件在EL1交付时（macOS HVF）PSCI由HVC调用；EL2 boot shim路径
    // 继续使用SMC。此处依据EBS时保存的CurrentEL选择，不依赖平台名称。
    let psci_hvc = ((bi.el2.current_el >> 2) & 3) == 1;
    let timer_intid = if psci_hvc {
        platform.virtual_timer_intid
    } else {
        platform.physical_timer_intid
    };
    if !crate::arch::aarch64::smp::boot_secondaries(&platform.cpus, root, timer_intid, psci_hvc)
    {
        uart::puts("[exo] SMP startup failed: all four PSCI CPUs are required\r\n");
        loop {
            unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
        }
    }
    uart::puts("[exo] SMP online mask=");
    uart::hex(crate::arch::aarch64::smp::online_mask());
    uart::puts(" cpu_count=");
    uart::hex(crate::arch::aarch64::smp::cpu_count() as u64);
    uart::puts("\r\n");

    // 辅助核上线后再启动CPU0的1ms抢占Timer，避免SMP握手期间把尚未
    // 进入EL0的CPU0误当成正在运行的用户线程。
    crate::scheduler::init(timer_intid, psci_hvc);

    uart::puts("[exo] enter EL0 libOS...\r\n");

    // 从 EL1 进入 EL0。
    //
    // USER_BASE 必须是 libOS 链接入口地址 0x4000_0000。
    // USER_STACK_TOP 是 SP_EL0 初值。
    // enter_el0() 内部设置 ELR_EL1/SPSR_EL1/SP_EL0 后执行 eret。
    crate::trap::enter_el0(loaded.entry, USER_STACK_TOP, USER_BOOT_INFO_VA);
}

fn init_allocator_from_bootinfo(bi: &BootInfo) {
    let mut src = 0;
    crate::mem::init_empty();

    // 只复制 BootInfo 里实际有效的 range。
    // bi.range_count 来自 EL2 遍历 UEFI MemoryMap 时统计的 conventional ranges 数量。
    while src < bi.range_count && src < crate::boot_info::MAX_BOOT_RANGES {
        let mut base = bi.ranges[src].base;
        let mut pages = bi.ranges[src].pages;
        let end = base + pages * 4096;

        // v1 先把 EL0 用户窗口对应的物理地址段从 allocator 中剔除。
        //
        // 原因:
        //   build_el1_stage1() 会跳过 USER_BASE..USER_WINDOW_END 的 identity map,
        //   用这段 VA 放 EL0 的 L3 用户映射。
        //   如果页表页/root table 被分配到这段 PA, MMU 打开后 EL1 访问页表页会缺页。
        //
        // 后续更完整的做法是维护 reserved ranges, 当前先保守跳过这 32MB。
        if base < USER_WINDOW_END && end > USER_BASE {
            if end <= USER_WINDOW_END {
                pages = 0;
            } else {
                base = USER_WINDOW_END;
                pages = (end - USER_WINDOW_END) / 4096;
            }
        }

        crate::mem::add_range(base, pages);
        src += 1;
    }
}

fn complete_el1_stage1(
    root: u64,
    bi: &BootInfo,
    stack_paddr: u64,
    user_boot_info_pa: u64,
    initial_ipc_pa: u64,
) {
    // 把 UART MMIO 所在 2MB block 映射成 Device memory。
    //
    // 普通内存和 MMIO 的属性不能混用:
    //   普通内存可以 cache/speculate/reorder。
    //   MMIO 必须使用 Device 属性, 避免 CPU 把设备寄存器访问当普通内存优化。
    let uart_base = crate::uart::base();
    if uart_base != 0 {
        crate::mmu::map_block_2m(
            root,
            uart_base & !0x1f_ffff,
            uart_base & !0x1f_ffff,
            crate::mmu::MMU_DEV,
        );
    }

    // 如果 UEFI 提供 framebuffer, 把 framebuffer 范围也映射成 Device memory。
    //
    // framebuffer 是内存映射设备/显存区域, 写它的效果会被显示设备观察到。
    // 当前只保证 EL1 可访问; EL0 默认不直接拿 framebuffer 权限。
    if bi.fb.base != 0 && bi.fb.size != 0 {
        crate::mmu::map_range_2m(
            root,
            bi.fb.base,
            bi.fb.base + bi.fb.size as u64,
            crate::mmu::MMU_DEV,
            USER_BASE,
            USER_WINDOW_END,
        );
    }

    // 完整 DTB 映射为 Normal、EL0 只读且不可执行；有效长度以 BootInfo 为准。
    // LoaderData 在任务生命周期中保留，EL0 不获得其 Frame 所有权。
    crate::mmu::map(
        root,
        exo_abi::USER_DTB_VA,
        bi.dtb,
        crate::mmu::MMU_USER_RO,
        bi.dtb_size.div_ceil(PAGE_SIZE),
    );

    // BootInfo 仅包含交接地址、长度和实际上线 CPU 数。
    crate::mmu::map(
        root,
        USER_BOOT_INFO_VA,
        user_boot_info_pa,
        crate::mmu::MMU_USER_RO,
        1,
    );

    // 映射 EL0 栈:
    //
    //   EL0 VA [USER_STACK_TOP - USER_STACK_PAGES*4K, USER_STACK_TOP)
    //       -> PA [stack_paddr, stack_paddr + USER_STACK_PAGES*4K)
    //
    // 栈向低地址增长, SP_EL0 初始放在 USER_STACK_TOP。
    // MMU_USER_RW 允许 EL0 读写, 但不允许执行。
    crate::mmu::map(
        root,
        USER_STACK_TOP - USER_STACK_PAGES * 4096,
        stack_paddr,
        crate::mmu::MMU_USER_RW,
        USER_STACK_PAGES,
    );

    // 初始线程的 IPC Buffer 位于固定线程 IPC arena；新线程由
    // SYS_THREAD_CREATE 使用同一布局单独映射自己的页面。
    crate::mmu::map(
        root,
        exo_abi::THREAD_IPC_BUFFER_BASE,
        initial_ipc_pa,
        crate::mmu::MMU_USER_RW,
        1,
    );
    // complete_el1_stage1 在页表已经激活后补充用户映射。确保页表写入对
    // 硬件 page-table walker 可见，并清掉此前可能缓存的无效翻译。
    crate::mmu::flush_el1_tlb();
}
