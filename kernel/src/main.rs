//! ============================================================================
//! 外核 (Exokernel) —— EL2 boot shim + EL1 外核
//! ============================================================================
//! 启动顺序: UEFI 固件 → BOOTAA64.EFI(EL2) → EBS → eret 进 EL1 外核 → eret 进 EL0 libOS
//!
//! main.rs       UEFI/EL2 入口: 收 GOP/DTB/MemoryMap → EBS → eret 到 EL1
//! kmain.rs      EL1 外核主逻辑: 初始化内存/DTB/MMU → 加载 EL0 libOS → eret
//! boot_info.rs  BootInfo 结构体 (显存地址 / 空闲内存数量 / CPU状态)
//! uart.rs       EBS 后由 DTB 初始化的 PL011 串口驱动

#![no_std]      // 不用 Rust 标准库 (裸机, 没有操作系统)
#![no_main]     // 不用 Rust 默认的 main 函数 (用 UEFI 的 #[entry])

// ═══════════════════════════════════════════════════════════════════
// mrs!/msr! 宏 —— 读写 ARM 系统寄存器
//   例: let v = mrs!("sctlr_el2");    // 读 SCTLR_EL2
//       msr!("hcr_el2", 1 << 31);     // 写 HCR_EL2
// ═══════════════════════════════════════════════════════════════════

/// 读系统寄存器: mrs 指令 (Move System Register)
#[macro_export]
macro_rules! mrs {
    ($reg:literal) => {{
        let v: u64;
        unsafe {
            core::arch::asm!(concat!("mrs {}, ", $reg), out(reg) v, options(nomem, nostack));
        }
        v
    }};
}

/// 写系统寄存器: msr 指令 (Move System Register)
#[macro_export]
macro_rules! msr {
    ($reg:literal, $val:expr) => {{
        let v: u64 = $val;
        unsafe {
            core::arch::asm!(concat!("msr ", $reg, ", {}"), in(reg) v, options(nomem, nostack));
        }
    }};
}

// ── 加载其他模块 ──
mod boot_info;  // BootInfo 结构体定义
mod dtb;        // 设备树解析器
mod kmain;      // 内核主逻辑
mod mem;        // 物理页分配器
mod mmu;        // EL1 stage-1 页表管理
mod protect;    // MMIO 保护表 (谁拥有哪个物理地址)
mod trap;       // EL2/EL1 陷入处理 + eret 函数
mod uart;       // 串口输出
mod vectors;    // EL2/EL1 异常向量表

// ── 导入需要用到的外部库 ──
use boot_info::BootInfo;                       // 启动信息结构体
use uefi::prelude::*;                          // UEFI 标准接口
use uefi::proto::console::gop::GraphicsOutput; // 显卡协议 (GOP)

/// DeviceTree 在 UEFI 配置表里的标识 (GUID)
const FDT_GUID: uefi::Guid = uefi::guid!("b1b621d5-f19c-41a5-830b-d9152c69aae0");

const EL1_BOOT_STACK_SIZE: usize = 64 * 1024;
static mut EL1_BOOT_STACK: [u8; EL1_BOOT_STACK_SIZE] = [0; EL1_BOOT_STACK_SIZE];

#[inline(always)]
fn clean_invalidate_exec_range(start: u64, size: u64) {
    let mut p = start & !63;
    let end = (start + size + 63) & !63;
    while p < end {
        unsafe {
            // Clean data cache to PoC, then invalidate matching instruction cache line.
            // 这里用 cvac 而不是 cvau, 对真机固件/缓存层级更保守。
            core::arch::asm!("dc cvac, {}", in(reg) p, options(nostack));
        }
        p += 64;
    }
    unsafe { core::arch::asm!("dsb sy", options(nomem, nostack)); }

    p = start & !63;
    while p < end {
        unsafe {
            core::arch::asm!("ic ivau, {}", in(reg) p, options(nostack));
        }
        p += 64;
    }
    unsafe { core::arch::asm!("dsb sy", "isb", options(nomem, nostack)); }
}

fn boot_alloc_page(bi: &mut BootInfo) -> u64 {
    let mut i = 0usize;
    while i < bi.range_count {
        if bi.ranges[i].pages != 0 {
            let pa = bi.ranges[i].base;
            bi.ranges[i].base += 4096;
            bi.ranges[i].pages -= 1;
            if bi.mem.free_pages != 0 {
                bi.mem.free_pages -= 1;
            }
            unsafe { core::ptr::write_bytes(pa as *mut u8, 0, 4096); }
            return pa;
        }
        i += 1;
    }
    loop {}
}

fn boot_l1_index(va: u64) -> usize {
    ((va >> 30) & 0x1ff) as usize
}

fn boot_l2_index(va: u64) -> usize {
    ((va >> 21) & 0x1ff) as usize
}

fn boot_map_2m(root: u64, bi: &mut BootInfo, va: u64, pa: u64, flags: u64) {
    unsafe {
        let l1 = root as *mut u64;
        let l1e = l1.add(boot_l1_index(va));
        if core::ptr::read_volatile(l1e) == 0 {
            let l2_pa = boot_alloc_page(bi);
            core::ptr::write_volatile(l1e, l2_pa | 0b11);
            clean_invalidate_exec_range(l2_pa, 4096);
        }
        let l2_pa = core::ptr::read_volatile(l1e) & !0xfff;
        let l2 = l2_pa as *mut u64;
        core::ptr::write_volatile(
            l2.add(boot_l2_index(va)),
            (pa & !0x1f_ffff) | flags | 0b01,
        );
        clean_invalidate_exec_range(l2_pa, 4096);
    }
}

fn boot_map_range_2m(root: u64, bi: &mut BootInfo, start: u64, end: u64, flags: u64) {
    let mut va = start & !0x1f_ffff;
    let end = (end + 0x1f_ffff) & !0x1f_ffff;
    while va < end {
        boot_map_2m(root, bi, va, va, flags);
        va += 0x20_0000;
    }
}

fn install_el1_identity_stage1(root: u64) {
    msr!("ttbr0_el1", root);
    let tcr: u64 = (0b00 << 14)
        | (25 << 0)
        | (0b11 << 8)
        | (0b11 << 10)
        | (0b11 << 12)
        | (0b101 << 32);
    msr!("tcr_el1", tcr);
    let mair: u64 = (0xff << 0) | (0x04 << 8);
    msr!("mair_el1", mair);
    unsafe { core::arch::asm!("dsb sy; tlbi vmalle1; dsb sy; isb", options(nostack)); }
    let sctlr: u64 = (1 << 0)
        | (1 << 2)
        | (1 << 11)
        | (1 << 12)
        | (1 << 20)
        | (1 << 22)
        | (1 << 23)
        | (1 << 28)
        | (1 << 29);
    msr!("sctlr_el1", sctlr);
    unsafe { core::arch::asm!("isb", options(nomem, nostack)); }
}

#[cfg(feature = "pi5")]
fn install_pi5_el1_boot_mmu(bi: &mut BootInfo) -> u64 {
    let root = boot_alloc_page(bi);

    // EL1 trampoline/page tables live in low conventional memory.
    boot_map_range_2m(root, bi, 0, 0x0040_0000, crate::mmu::MMU_KERNEL);

    // UEFI-loaded .efi image, EL1 stack, BootInfo, and FDT currently sit in this band
    // on Pi5 EDK2 logs:
    //   el1_main/VBAR/stack: 0x37xx_xxxx
    //   FDT:                0x3a10_b000
    //   BootInfo:           0x3f3f_fxxx
    // Keep it deliberately broad for bring-up, then EL1 replaces it with its own table.
    boot_map_range_2m(
        root,
        bi,
        0x3000_0000,
        0x4200_0000,
        crate::mmu::MMU_KERNEL,
    );

    // Pi5 debug UART from DTB: 0x107d001000. Map the containing 2MB block as Device.
    let uart = crate::uart::PI5_DEBUG_UART_BASE & !0x1f_ffff;
    boot_map_2m(root, bi, uart, uart, crate::mmu::MMU_DEV);

    clean_invalidate_exec_range(root, 4096);
    install_el1_identity_stage1(root);
    root
}

#[cfg(feature = "pi5")]
core::arch::global_asm!(
    r#"
    .section .text.el1_entry_trampoline, "ax"
    .global el1_entry_trampoline
    .global el1_entry_trampoline_end
el1_entry_trampoline:
    mov x20, x0
    mov x21, x1

    // 第一条可观测动作: 从 EL1 主动 HVC 回 EL2。
    //
    // 如果 EL2 handler 能看到 EC=HVC64, 说明 eret 已经成功进入 EL1 并取到指令。
    // 如果看不到, 问题就在 eret/EL1 取指状态本身, 而不是 UART 写或 Rust 入口。
    hvc #0x51

    // 第二个 HVC 验证: EL2 handler 修改 ELR_EL2 后, 能否正常返回 EL1 继续执行。
    // 如果只看到第一个 HVC, 问题在 EL2 handler 返回路径/ELR_EL2 推进。
    hvc #0x52

    // x9 = Pi5 UART10 CPU physical address 0x107d001000.
    movz x9, #0x1000
    movk x9, #0x7d00, lsl #16
    movk x9, #0x0010, lsl #32

    // 先让 EL2 的 HVC 日志有时间从 UART FIFO 排空。
    movz x11, #0xffff
    movk x11, #0x0003, lsl #16
1:
    subs x11, x11, #1
    b.ne 1b

    // 现在再读 PL011 FR。若 TXFF(bit5) 仍然是 1, 回 EL2 报告。
    ldr w2, [x9, #0x18]
    tbz w2, #5, 2f
    hvc #0x62
2:
    mov w10, #'E'
    str w10, [x9]

    hvc #0x53

    mov x0, x20
    movz x2, #0xe1
    br x21
el1_entry_trampoline_end:
    "#
);

#[cfg(feature = "pi5")]
extern "C" {
    fn el1_entry_trampoline() -> !;
    static el1_entry_trampoline_end: u8;
}

// ═══════════════════════════════════════════════════════════════════
// UEFI 入口函数 —— 固件加载 BOOTAA64.EFI 后跳到这里
// ═══════════════════════════════════════════════════════════════════
// #[entry] 是 uefi crate 提供的宏, 它在编译期自动生成 UEFI 标准入口点
// (_start / efi_main), 然后调我们这个 main()。
// 返回 Status: 成功=0, 失败=错误码。但我们永远不会返回——EBS 后直接接管机器。
// ═══════════════════════════════════════════════════════════════════

#[entry]
fn main() -> Status {
    // ── 初始化 UEFI 库 (让 print! 等宏能工作) ──
    uefi::helpers::init().unwrap();

    // ── EBS 前日志走 UEFI console ──
    // 此时不能假设 UART MMIO 地址固定。QEMU/Pi5 的 UART 都应该从 DTB 发现。
    uefi::println!("[exo] UEFI console: EL2 boot, pre-EBS");

    // ── 建一个 BootInfo 结构体, 用来收集所有硬件资源 ──
    let mut bi = BootInfo::default();

    // ── 第一步: 拿 framebuffer 物理地址 (趁 UEFI Boot Services 还在) ──
    // GOP = Graphics Output Protocol, UEFI 的显卡驱动提供的接口
    // EBS 之后 GOP 协议就死了, 但 framebuffer 那块物理内存一直有效
    // 我们要在 EBS 之前把物理地址抄出来
    if let Ok(h) = uefi::boot::get_handle_for_protocol::<GraphicsOutput>() {
        if let Ok(mut gop) = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(h) {
            let mut fb = gop.frame_buffer();
            bi.fb.base = fb.as_mut_ptr() as u64;  // framebuffer 物理基址
            bi.fb.size = fb.size();                // 总共多少字节
        }
    }

    // ── 第二步: 扫 UEFI 配置表拿 DTB (DeviceTree 物理地址) ──
    // ConfigTable 是 UEFI 提供的一个全局表, 里面放了 ACPI 和 DTB 等系统描述数据
    // EBS 后这些东西还在(它们是物理内存), 但趁 EBS 前抄地址更保险
    // QEMU virt 默认走 ACPI (没有 FDT_GUID), 所以 bi.dtb 通常是 0
    // Pi5 上需要设 SystemTableMode=0x02 才有 DTB
    let (rsdp, dtb) = uefi::system::with_config_table(|entries| {
        let mut rsdp = 0u64;
        let mut dtb = 0u64;
        for e in entries {
            if e.guid == uefi::table::cfg::ACPI2_GUID {
                rsdp = e.address as u64;   // ACPI 表地址 (高级配置与电源接口)
            } else if e.guid == FDT_GUID {
                dtb = e.address as u64;     // DeviceTree 地址 (扁平设备树)
            }
        }
        (rsdp, dtb)  // 返回 (ACPI地址, DTB地址)
    });
    bi.rsdp = rsdp;
    bi.dtb = dtb;  // 存进 BootInfo, 后面 kmain 会拿它解析硬件信息

    // EBS 前仍可用 UEFI console, 所以这里先用同一套 DTB parser 做一次只读预检。
    //
    // 目的:
    //   1. 证明 UEFI config table 里到底有没有 FDT_GUID。
    //   2. 证明 FDT header 是否有效。
    //   3. 证明 stdout-path/aliases/reg/ranges 是否能解析出 UART CPU 物理地址。
    //
    // EBS 后 console 失效; 如果 EL1 UART 日志不出现, 这组日志能把问题切开:
    //   - EBS 前就找不到 UART: DTB/解析逻辑问题。
    //   - EBS 前能找到 UART, EBS 后没日志: EL2->EL1 或 UART MMU/访问问题。
    uefi::println!("[exo] UEFI config: ACPI2=0x{:x}, FDT=0x{:x}", rsdp, dtb);
    let mut pre_ebs_uart = None;
    if dtb != 0 {
        match crate::dtb::total_size(dtb) {
            Some(size) => uefi::println!("[exo] pre-EBS FDT valid, size=0x{:x}", size),
            None => uefi::println!("[exo] pre-EBS FDT bad header"),
        }
        match crate::dtb::find_pl011_reg(dtb) {
            Some(reg) => {
                uefi::println!(
                    "[exo] pre-EBS DTB UART pa=0x{:x}, size=0x{:x}",
                    reg.base,
                    reg.size
                );
                pre_ebs_uart = Some(reg);
            }
            None => uefi::println!("[exo] pre-EBS DTB UART not found"),
        }
    }
    if let Some(reg) = pre_ebs_uart {
        uart::init(reg.base);
        uefi::println!("[exo] pre-EBS direct UART write test follows");
        uart::puts("[exo] pre-EBS direct UART OK\r\n");
    }

    if dtb == 0 {
        #[cfg(all(feature = "qemu", not(feature = "pi5")))]
        uefi::println!("[exo] UEFI FDT not found; QEMU build will use embedded qemu-virt.dtb after EBS");
        #[cfg(not(all(feature = "qemu", not(feature = "pi5"))))]
        uefi::println!("[exo] UEFI FDT not found; no reliable UART after EBS");
    }

    // ── 第三步: ExitBootServices —— 从此机器归内核管 ──
    // EBS 之前, UEFI 固件还在后台处理事件、管理协议、提供服务
    // EBS 之后, UEFI 的 Boot Services 全部失效, 但 Runtime Services 和
    // ConfigTable 还在。外核彻底接管硬件, 只能用自驱的 UART/framebuffer。
    // exit_boot_services() 内部: GetMemoryMap → ExitBootServices,
    // 失败则重试一次(仿 Linux 的做法), 返回 MemoryMap 的所有权。
    // 诊断: 确认 EL 级别 (2=EL2 正常, 1=EL1 不正常)
    let el_raw = mrs!("CurrentEL");
    uefi::println!("[exo] CurrentEL raw=0x{:x}, exc_level={}", el_raw, (el_raw >> 2) & 3);
    uefi::println!("[exo] calling ExitBootServices...");
    let mmap = unsafe { uefi::boot::exit_boot_services(uefi::boot::MemoryType::LOADER_DATA) };
    // EBS 之后 UEFI console 已经失效。后续日志必须等 EL1 从 DTB 初始化 UART。
    uart::puts("[exo] post-EBS still in EL2, direct UART OK\r\n");

    // ── 第四步: 提取空闲物理内存范围, 放进 BootInfo 给 EL1 外核 ──
    // MemoryMap 里记录了整台机器所有物理内存页的用途:
    //   CONVENTIONAL     → 空闲可用 (Type 7)
    //   LOADER_CODE/DATA → 内核自己占的 (Type 12/13)
    //   BOOT_SERVICES_*  → 固件占的, EBS 后也变空闲了 (Type 3/4)
    // 但我们暂时只收 Type 7, 保证不给内核自己的地址
    bi.fill_memmap(&mmap);
    bi.read_el2();
    uart::puts("[exo] BootInfo ready in EL2\r\n");

    #[cfg(feature = "pi5")]
    let copied_el1_entry = {
        let mut entry = el1_entry_trampoline as *const () as u64;
        if bi.range_count != 0 && bi.ranges[0].pages != 0 {
            let dst = bi.ranges[0].base;
            let src = el1_entry_trampoline as *const u8;
            let end = unsafe { &el1_entry_trampoline_end as *const u8 as u64 };
            let size = end - src as u64;
            unsafe {
                core::ptr::copy_nonoverlapping(src, dst as *mut u8, size as usize);
            }
            clean_invalidate_exec_range(dst, size);
            bi.ranges[0].base += 4096;
            bi.ranges[0].pages -= 1;
            if bi.mem.free_pages != 0 {
                bi.mem.free_pages -= 1;
            }
            entry = dst;
            uart::puts("[exo] copied EL1 trampoline pa=");
            uart::hex(dst);
            uart::puts(" size=");
            uart::hex(size);
            uart::puts("\r\n");
            uart::puts("[exo] trampoline words=");
            let w0 = unsafe { core::ptr::read_volatile(dst as *const u32) };
            let w1 = unsafe { core::ptr::read_volatile((dst + 4) as *const u32) };
            let w2 = unsafe { core::ptr::read_volatile((dst + 8) as *const u32) };
            let w3 = unsafe { core::ptr::read_volatile((dst + 12) as *const u32) };
            uart::hex(w0 as u64);
            uart::puts(" ");
            uart::hex(w1 as u64);
            uart::puts(" ");
            uart::hex(w2 as u64);
            uart::puts(" ");
            uart::hex(w3 as u64);
            uart::puts("\r\n");
        }
        entry
    };

    // ── 第五步: EL2 boot shim 降级到 EL1 外核 ──
    crate::vectors::install_el2();
    uart::puts("[exo] VBAR_EL2 installed\r\n");
    let el1_vbar = crate::vectors::install_el1_from_el2();
    uart::puts("[exo] VBAR_EL1 preinstalled from EL2=");
    uart::hex(el1_vbar);
    uart::puts("\r\n");

    #[cfg(feature = "pi5")]
    {
        let root = install_pi5_el1_boot_mmu(&mut bi);
        uart::puts("[exo] EL1 boot identity MMU root=");
        uart::hex(root);
        uart::puts(" SCTLR_EL1=");
        uart::hex(mrs!("sctlr_el1"));
        uart::puts("\r\n");
    }

    let stack_top = core::ptr::addr_of!(EL1_BOOT_STACK) as u64 + EL1_BOOT_STACK_SIZE as u64;
    #[cfg(feature = "pi5")]
    let el1_entry = copied_el1_entry;
    #[cfg(not(feature = "pi5"))]
    let el1_entry = kmain::el1_main as *const () as u64;
    trap::enter_el1_kernel(
        el1_entry,
        stack_top,
        &bi as *const BootInfo as u64,
        kmain::el1_main as *const () as u64,
    )
}
