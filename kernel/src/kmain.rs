//! EL1 外核启动。使用 BootInfo 初始化内存和平台，准备初始任务，再进入 EL0。

use crate::boot_info::BootInfo;
use crate::config::{
    PAGE_SIZE, USER_BASE, USER_BOOT_INFO_VA, USER_STACK_PAGES, USER_STACK_TOP, USER_WINDOW_END,
};
use crate::resources::PlatformResources;
use crate::uart;

// 构建时将 libOS ELF 嵌入 EFI 镜像，EL1 从内存中加载。
static LIBOS_ELF: &[u8] = include_bytes!("../../libos/libos.elf");

/// QEMU 的 EL2 路径通过 eret 进入 EL1，并把 BootInfo 地址放在 x0。
#[no_mangle]
pub extern "C" fn el1_main(boot_info_addr: u64) -> ! {
    crate::arch::aarch64::cpu::set_id(0);
    // 先安装 EL1 异常向量，供 SVC、Abort 和 IRQ 进入。
    let vbar = crate::vectors::install_el1();

    // 安全前提：启动入口保留了对齐的 BootInfo，交接地址在 EL1 启动期间可读。
    let bi = unsafe { &*(boot_info_addr as *const BootInfo) };
    let platform = &crate::resources::PLATFORM;

    let root = init_el1_memory(bi);
    init_el1_platform(root, bi, vbar, platform);
    let libos_entry = install_initial_libos(root, bi, platform);
    start_secondary_cpus_and_scheduler(root, bi.el2.current_el, platform);

    uart::puts("[exo] enter EL0 libOS...\r\n");
    // 设置 EL0 入口和栈顶，通过 x0 传递 UserBootInfo 的虚拟地址后执行 eret。
    crate::trap::enter_el0(libos_entry, USER_STACK_TOP, USER_BOOT_INFO_VA)
}

/// 根据 BootInfo 初始化物理页分配器，返回已启用的根页表物理地址。
fn init_el1_memory(bi: &BootInfo) -> u64 {
    init_allocator_from_bootinfo(bi);

    // 启动代码已将 DTB 复制到独占的 LoaderData 页，EL1 校验后再建立只读映射。
    assert!(bi.dtb != 0 && bi.dtb % PAGE_SIZE == 0);
    assert!(bi.dtb_size >= 40 && bi.dtb_size <= exo_abi::USER_DTB_MAX_SIZE);

    // 先建立普通内存的恒等映射，留出固定的 EL0 虚拟地址窗口。
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
    root
}

/// 初始化 EL1 的 UART、GIC 和初始 VSpace，并登记平台设备的授权范围。
fn init_el1_platform(root: u64, bi: &BootInfo, vbar: u64, platform: &PlatformResources) {
    // 平台配置规定设备访问授权，EL0 根据 DTB 发现设备时仍受这些授权约束。
    crate::arch::aarch64::smp::set_cpu_count(platform.cpus.count);
    let uart_pa = platform.uart_pa;

    // UART 的物理地址来自平台配置，按 VA = PA 建立 Device 映射。
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

    init_gic_and_vspace(root, platform);

    // 注册平台授予的设备窗口。GIC 等 EL1 专用区域始终优先拒绝。
    assert!(platform.mmio.len() <= crate::task::MAX_MMIO_GRANTS);
    assert!(platform.irqs.len() <= crate::task::MAX_IRQ_GRANTS);
    for grant in platform.mmio {
        crate::protect::register(grant.base, grant.size);
    }
}

/// 初始化 GICv2 后，将启动根页表登记为初始 VSpace。
fn init_gic_and_vspace(root: u64, platform: &PlatformResources) {
    let gicd_pa = platform.gicd_pa;
    let gicc_pa = platform.gicc_pa;
    // GICD/GICC 的物理地址来自平台配置，按 VA = PA 建立 Device 映射。
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

    // 登记初始 VSpace，后续独立 VSpace 据此建立内核 UART/GIC 的设备映射。
    crate::vspace::initialize(root, platform.uart_pa, gicd_pa, gicc_pa);
}

/// 加载初始 libOS，准备用户映射并登记任务、IPC 和线程，返回 ELF 入口虚拟地址。
fn install_initial_libos(root: u64, bi: &BootInfo, platform: &PlatformResources) -> u64 {
    // ELF loader 按 PT_LOAD 的 R/W/X 权限分别分配并映射用户段。
    let loaded = crate::elf::load(root, LIBOS_ELF).expect("failed to load EL0 ELF");
    let stack_paddr = crate::mem::alloc_pages(USER_STACK_PAGES).expect("no mem for EL0 stack");
    let user_boot_info_pa = crate::mem::alloc_page().expect("no mem for UserBootInfo");
    let initial_ipc_pa = crate::mem::alloc_page().expect("no mem for initial IPC buffer");

    // 安全前提：物理分配器返回独占页，EL1 可通过恒等映射访问。
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

    // 在已启用的根页表中补上初始任务的栈和交接数据映射。
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

    loaded.entry
}

/// 共享页表、GIC 和任务对象就绪后，启动辅助核与 CPU0 的调度定时器。
fn start_secondary_cpus_and_scheduler(
    root: u64,
    firmware_current_el: u64,
    platform: &PlatformResources,
) {
    // 固件在 EL1 交付时用 HVC 和虚拟定时器。在 EL2 交付时用 SMC 和物理定时器。
    let psci_hvc = ((firmware_current_el >> 2) & 3) == 1;
    let timer_intid = if psci_hvc {
        platform.virtual_timer_intid
    } else {
        platform.physical_timer_intid
    };
    if !crate::arch::aarch64::smp::boot_secondaries(&platform.cpus, root, timer_intid, psci_hvc) {
        uart::puts("[exo] SMP startup failed: all four PSCI CPUs are required\r\n");
        loop {
            // 四核启动失败后停在 EL1，用 WFE 等待事件以避免空转。
            unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
        }
    }
    uart::puts("[exo] SMP online mask=");
    uart::hex(crate::arch::aarch64::smp::online_mask());
    uart::puts(" cpu_count=");
    uart::hex(crate::arch::aarch64::smp::cpu_count() as u64);
    uart::puts("\r\n");

    // 等辅助核上线后再启动 CPU0 的 1 ms 抢占定时器，避免干扰启动握手。
    crate::scheduler::init(timer_intid, psci_hvc);
}

fn init_allocator_from_bootinfo(bi: &BootInfo) {
    let mut src = 0;
    crate::mem::init_empty();

    // 只登记 BootInfo 从 UEFI 内存图提取的有效空闲范围。
    while src < bi.range_count && src < crate::boot_info::MAX_BOOT_RANGES {
        let mut base = bi.ranges[src].base;
        let mut pages = bi.ranges[src].pages;
        let end = base + pages * 4096;

        // EL1 恒等映射留出了 EL0 用户窗口。与窗口相交的物理范围保守地只保留
        // 窗口上方的部分，确保页表页等分配结果可由 EL1 通过恒等映射访问。
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

    // 将 UserBootInfo 映射为 EL0 只读页，供 libOS 校验格式并读取设备树信息。
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
