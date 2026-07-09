//! kmain —— EL1 外核主逻辑
//!
//! EL2 boot shim 只负责 UEFI/EBS 和 eret。这里开始才是真正外核:
//! 初始化物理页分配器、解析 DTB、安装 EL1 异常向量、建立 stage-1 页表、
//! 加载 EL0 libOS, 最后 eret 进 EL0。

use crate::boot_info::BootInfo;        // EL2 boot shim 收集后传给 EL1 的启动信息
use crate::uart;                       // PL011 串口输出, 当前所有 bring-up 日志都靠它
use core::ptr::copy_nonoverlapping;    // 裸机 memcpy: 把 DTB/libOS 字节拷到分配出的物理页

// QEMU virt 的 DTB 备份。
//
// QEMU 的 EDK2 默认经常只给 ACPI, 不一定在 UEFI config table 里放 FDT_GUID。
// 如果 BootInfo.dtb == 0, EL1 就把这个编译期嵌入的 DTB 拷到普通物理页,
// 再把那块物理地址交给 dtb::parse() 解析。
static EMBEDDED_DTB: &[u8] = include_bytes!("../../qemu/qemu-virt.dtb");

// EL0 libOS 的原始机器码。
//
// build.sh 先把 libos ELF 用 rust-objcopy 转成 libos/libos.bin。
// 编译 BOOTAA64.efi 时, include_bytes! 把 libos.bin 直接塞进 .efi 的 rodata。
// 运行到 EL1 后, 这里的 LIBOS_BIN 就是一段内存里的 &[u8], 不是文件系统路径。
static LIBOS_BIN: &[u8] = include_bytes!("../../libos/libos.bin");

// EL0 用户虚拟地址布局。
//
// libos/link.ld 把 _start 链接到 0x4000_0000, 所以 EL1 必须把 libOS 代码
// 映射到这个 VA。否则 eret 到 EL0 后, PC=0x4000_0000 会取不到正确指令。
const USER_BASE: u64 = 0x4000_0000;
const DTB_USER_VA: u64 = 0x4020_0000;

// EL0 栈顶。AArch64 栈向低地址增长, 所以实际映射的栈页是:
//   [USER_STACK_TOP - USER_STACK_PAGES * 4096, USER_STACK_TOP)
const USER_STACK_TOP: u64 = 0x4100_0000;
const USER_STACK_PAGES: u64 = 4;

// EL0 用户窗口的上界。
//
// build_el1_stage1() 做 EL1 identity map 时会跳过 USER_BASE..USER_WINDOW_END,
// 避免先用 2MB block 把 0x4000_0000 附近映射成 EL1-only, 之后又想在同一范围
// 放 EL0 L3 页表映射。页表同一个 L2 entry 不能同时是 block 又是 table。
const USER_WINDOW_END: u64 = 0x4200_0000;

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

    // 安装 EL1 异常向量表 — 必须尽早, EL0 的 SVC/Abort/IRQ 都走 VBAR_EL1。
    let vbar = crate::vectors::install_el1();

    // 解引用 BootInfo。MMU 还没开, CPU 把指针值直接当物理地址用。
    let bi = unsafe { &*(boot_info_addr as *const BootInfo) };

    // 用 EL2 收集到的 UEFI conventional memory ranges 初始化 EL1 物理页分配器。
    init_allocator_from_bootinfo(bi);

    // 选择 DTB 来源。
    //
    // 情况 A: UEFI config table 里提供了 FDT_GUID, bi.dtb != 0, 直接解析那块物理地址。
    // 情况 B: QEMU EDK2 没给 DTB, bi.dtb == 0, 使用编译进 .efi 的 EMBEDDED_DTB。
    let dtb_addr = if bi.dtb == 0 {
        #[cfg(all(feature = "qemu", not(feature = "pi5")))]
        {
            let pages = ((EMBEDDED_DTB.len() + 4095) / 4096) as u64;
            let pa = crate::mem::alloc_pages(pages).expect("no mem for DTB");
            unsafe { copy_nonoverlapping(EMBEDDED_DTB.as_ptr(), pa as *mut u8, EMBEDDED_DTB.len()); }
            pa
        }
        #[cfg(not(all(feature = "qemu", not(feature = "pi5"))))]
        {
            // Pi5 没有 DTB 无法继续 — 此时 UART 还没初始化, 无法打印, 只能死循环。
            loop {}
        }
    } else {
        bi.dtb
    };

    let dtb_total_size = crate::dtb::total_size(dtb_addr).expect("bad DTB");
    let dtb_page_pa = dtb_addr & !0xfff;
    let dtb_page_off = dtb_addr & 0xfff;
    let dtb_map_pages = ((dtb_page_off + dtb_total_size + 4095) / 4096) as u64;
    let dtb_user_va = DTB_USER_VA + dtb_page_off;

    // 打开 EL1 identity stage-1。
    //
    // EL1 MMU 关闭时 ARM 对部分栈上的宽访问会按严格对齐处理, Rust 的 DTB
    // parser 容易触发对齐异常。先把代码/数据/栈/DTB 按 Normal memory 做
    // identity map。UART 的 MMIO 区域暂不映射 — 等 DTB 发现 UART 地址后再
    // 精确映射为 Device memory。
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

    // 从 DTB 发现 UART 物理地址 (只读 DTB 内存, 不碰 UART MMIO)。
    let uart_reg = crate::dtb::find_pl011_reg(dtb_addr).expect("no PL011 UART in DTB");

    // 把 DTB 发现的 UART 地址所在的 2MB block 映射为 Device memory,
    // 然后初始化 UART。从这行开始, 所有日志都走 DTB 发现的 UART, 不再有硬编码地址。
    crate::mmu::map_block_2m(
        root,
        uart_reg.base & !0x1f_ffff,
        uart_reg.base & !0x1f_ffff,
        crate::mmu::MMU_DEV,
    );
    crate::mmu::flush_el1_tlb();
    crate::protect::register(uart_reg.base, uart_reg.size);
    uart::init(uart_reg.base);

    uart::puts("\r\n*** [exo] EL1 kernel alive ***\r\n");
    uart::puts("[exo] VBAR_EL1=");
    uart::hex(vbar);
    uart::puts("\r\n");

    uart::puts("[exo] EL1 mem ready, free=");
    uart::hex(crate::mem::free_pages_total() * 4);
    uart::puts(" KB\r\n");

    if bi.dtb == 0 {
        uart::puts("[exo] using embedded QEMU DTB\r\n");
    }
    uart::puts("[exo] DTB pa=");
    uart::hex(dtb_addr);
    uart::puts(" user_va=");
    uart::hex(dtb_user_va);
    uart::puts(" size=");
    uart::hex(dtb_total_size);
    uart::puts("\r\n");
    uart::puts("[exo] DTB UART base=");
    uart::hex(uart_reg.base);
    uart::puts(" size=");
    uart::hex(uart_reg.size);
    uart::puts("\r\n");

    // 解析 DTB。
    //
    // 当前 dtb::parse() 主要打印节点和 reg 地址, 后续应该在这里把 MMIO reg 范围
    // 登记进 protect.rs, 形成 "哪些物理地址是设备寄存器" 的保护表。
    crate::dtb::parse(dtb_addr);

    // 准备 EL0 libOS 的代码页。
    //
    // LIBOS_BIN.len() 是二进制字节数; 物理页分配器按 4KB 页工作,
    // 所以这里向上取整得到 code_pages。
    let code_bytes = LIBOS_BIN.len();
    let code_pages = ((code_bytes + 4095) / 4096) as u64;

    // 为 EL0 分配代码物理页和栈物理页。
    //
    // code_paddr 是 libOS 机器码实际放置的物理地址。
    // stack_paddr 是 EL0 栈物理页实际放置的起始物理地址。
    //
    // 注意: 这两个地址都不是 EL0 看到的 VA。EL0 看到的是 USER_BASE 和 USER_STACK_TOP。
    let code_paddr = crate::mem::alloc_pages(code_pages).expect("no mem for EL0 code");
    let stack_paddr = crate::mem::alloc_pages(USER_STACK_PAGES).expect("no mem for EL0 stack");

    // 把 .efi rodata 里的 libos.bin 拷贝到新分配的代码物理页。
    //
    // copy_nonoverlapping(src, dst, len) 等价于 memcpy, 但要求源和目标不重叠。
    // 这里源是 .efi 内部 rodata, 目标是刚 alloc_pages() 得到的空闲物理页, 不会重叠。
    unsafe { copy_nonoverlapping(LIBOS_BIN.as_ptr(), code_paddr as *mut u8, code_bytes); }

    uart::puts("[exo] EL0 libOS pa=");
    uart::hex(code_paddr);
    uart::puts(" va=");
    uart::hex(USER_BASE);
    uart::puts(" stack_pa=");
    uart::hex(stack_paddr);
    uart::puts(" stack_va=");
    uart::hex(USER_STACK_TOP);
    uart::puts(" bytes=");
    uart::hex(code_bytes as u64);
    uart::puts("\r\n");

    // 建 EL1 stage-1 页表。
    //
    // 这个页表同时服务两个目标:
    //   1. EL1 外核自己继续运行: 需要 identity map 当前代码、栈、DTB、UART 等。
    //   2. EL0 libOS 能启动: 需要把 USER_BASE 映射到 code_paddr,
    //      把 USER_STACK_TOP 下方的栈窗口映射到 stack_paddr。
    complete_el1_stage1(
        root,
        bi,
        code_paddr,
        code_pages,
        stack_paddr,
        dtb_page_pa,
        dtb_map_pages,
    );
    uart::puts("[exo] EL1 stage-1 root=");
    uart::hex(root);
    uart::puts("\r\n");

    uart::puts("[exo] enter EL0 libOS...\r\n");

    // 从 EL1 进入 EL0。
    //
    // USER_BASE 必须是 libOS 链接入口地址 0x4000_0000。
    // USER_STACK_TOP 是 SP_EL0 初值。
    // enter_el0() 内部设置 ELR_EL1/SPSR_EL1/SP_EL0 后执行 eret。
    crate::trap::enter_el0(USER_BASE, USER_STACK_TOP, dtb_user_va);
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
    code_paddr: u64,
    code_pages: u64,
    stack_paddr: u64,
    dtb_page_pa: u64,
    dtb_map_pages: u64,
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

    // 映射 EL0 代码:
    //
    //   EL0 VA USER_BASE -> PA code_paddr
    //
    // MMU_USER_RX 允许 EL0 取指执行, 但不允许写。
    // 这里 code_pages 可能大于 1, 所以连续映射多个 4KB 页。
    crate::mmu::map(root, USER_BASE, code_paddr, crate::mmu::MMU_USER_RX, code_pages);

    // 把 DTB 只读映射给 EL0。
    //
    // DTB 是硬件发现入口: EL0 libOS 可以读 compatible/reg, 自己决定要请求映射哪个设备。
    // 权限用 MMU_USER_RO: EL0 可读不可写, 不能篡改 EL1 作为授权依据的硬件描述。
    crate::mmu::map(
        root,
        DTB_USER_VA,
        dtb_page_pa,
        crate::mmu::MMU_USER_RO,
        dtb_map_pages,
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

}
