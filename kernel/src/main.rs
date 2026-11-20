//! UEFI 启动入口。收集固件信息，退出启动服务，将 BootInfo 交给 EL1 的 el1_main。

#![no_std] // 不用 Rust 标准库 (裸机, 没有操作系统)
#![no_main] // 不用 Rust 默认的 main 函数 (用 UEFI 的 #[entry])

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

// 按机制、对象和策略分层；下面的 crate 内别名保持原有路径，
// 让本次目录重构不改变内核行为。
mod arch;
mod boot;
mod config;
mod drivers;
mod interrupt;
mod kmain;
mod memory;
mod object;
mod resource;
mod scheduler;
mod sync;
mod syscall;

pub(crate) use arch::aarch64::vectors;
pub(crate) use boot::{boot_info, elf};
#[cfg(feature = "pi5")]
pub(crate) use boot::platform;
pub(crate) use drivers::uart;
pub(crate) use interrupt::gic;
pub(crate) use memory::{frame, mmu, physical as mem, vspace};
pub(crate) use object::{ipc, task, thread};
pub(crate) use resource::{protect, resources};
pub(crate) use syscall::dispatch as trap;

// ── 导入需要用到的外部库 ──
use boot_info::BootInfo; // 启动信息结构体
use uefi::prelude::*; // UEFI 标准接口
use uefi::proto::console::gop::GraphicsOutput; // 显卡协议 (GOP)

/// DeviceTree 在 UEFI 配置表里的标识 (GUID)
const FDT_GUID: uefi::Guid = uefi::guid!("b1b621d5-f19c-41a5-830b-d9152c69aae0");

const EL1_BOOT_STACK_SIZE: usize = 64 * 1024;
static mut EL1_BOOT_STACK: [u8; EL1_BOOT_STACK_SIZE] = [0; EL1_BOOT_STACK_SIZE];

#[cfg(feature = "pi5")]
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
    unsafe {
        core::arch::asm!("dsb sy", options(nomem, nostack));
    }

    p = start & !63;
    while p < end {
        unsafe {
            core::arch::asm!("ic ivau, {}", in(reg) p, options(nostack));
        }
        p += 64;
    }
    unsafe {
        core::arch::asm!("dsb sy", "isb", options(nomem, nostack));
    }
}

#[cfg(feature = "pi5")]
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
            unsafe {
                core::ptr::write_bytes(pa as *mut u8, 0, 4096);
            }
            return pa;
        }
        i += 1;
    }
    loop {}
}

#[cfg(feature = "pi5")]
fn boot_l1_index(va: u64) -> usize {
    ((va >> 30) & 0x1ff) as usize
}

#[cfg(feature = "pi5")]
fn boot_l2_index(va: u64) -> usize {
    ((va >> 21) & 0x1ff) as usize
}

#[cfg(feature = "pi5")]
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
        core::ptr::write_volatile(l2.add(boot_l2_index(va)), (pa & !0x1f_ffff) | flags | 0b01);
        clean_invalidate_exec_range(l2_pa, 4096);
    }
}

#[cfg(feature = "pi5")]
fn boot_map_range_2m(root: u64, bi: &mut BootInfo, start: u64, end: u64, flags: u64) {
    let mut va = start & !0x1f_ffff;
    let end = (end + 0x1f_ffff) & !0x1f_ffff;
    while va < end {
        boot_map_2m(root, bi, va, va, flags);
        va += 0x20_0000;
    }
}

#[cfg(feature = "pi5")]
fn install_el1_identity_stage1(root: u64) {
    msr!("ttbr0_el1", root);
    let tcr: u64 =
        (0b00 << 14) | (25 << 0) | (0b11 << 8) | (0b11 << 10) | (0b11 << 12) | (0b101 << 32);
    msr!("tcr_el1", tcr);
    let mair: u64 = (0xff << 0) | (0x04 << 8) | (0x44 << 16);
    msr!("mair_el1", mair);
    unsafe {
        core::arch::asm!("dsb sy; tlbi vmalle1; dsb sy; isb", options(nostack));
    }
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
    unsafe {
        core::arch::asm!("isb", options(nomem, nostack));
    }
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
    boot_map_range_2m(root, bi, 0x3000_0000, 0x4200_0000, crate::mmu::MMU_KERNEL);

    // Pi5 debug UART from DTB: 0x107d001000. Map the containing 2MB block as Device.
    let uart = crate::platform::EARLY_DEBUG_UART_BASE & !0x1f_ffff;
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

#[entry]
fn main() -> Status {
    // 1. 固件已提供控制台，第一条启动日志直接使用它输出。
    uefi::helpers::init().unwrap();
    uefi::println!("[exo] UEFI console: EL2 boot, pre-EBS");

    // 2. 建立内核自己的 BootInfo，保存显存信息和设备树副本的位置。
    let mut bi = BootInfo::default();
    if let Err(error) = collect_firmware_info(&mut bi) {
        uefi::println!("[exo] DTB handoff failed: {}", error);
        return Status::LOAD_ERROR;
    }

    // 3. 提前配置UART，退出启动服务后用它输出日志。
    uart::init(crate::resources::PLATFORM.uart_pa);

    // 4. 读取固件交付时的异常级，退出 UEFI 启动服务并取得最终内存图。
    let el_raw = mrs!("CurrentEL");
    let mmap = exit_uefi_boot_services(el_raw);

    // 5. 从内存图提取可用物理页范围，将它和 CPU 寄存器快照写入 BootInfo。
    bi.fill_memmap(&mmap);
    bi.read_cpu_state(el_raw);

    // 6. 通过 x0 把 BootInfo 地址交给 EL1 外核入口 el1_main。
    enter_el1(&mut bi)
}

/// 保存显存和 DTB，留给退出启动服务后的内核使用。
fn collect_firmware_info(bi: &mut BootInfo) -> Result<(), &'static str> {
    // 若固件提供图形输出协议（GOP），保存帧缓冲区的物理地址和长度。
    if let Ok(h) = uefi::boot::get_handle_for_protocol::<GraphicsOutput>() {
        if let Ok(mut gop) = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(h) {
            let mut fb = gop.frame_buffer();
            bi.fb.base = fb.as_mut_ptr() as u64;
            bi.fb.size = fb.size();
        }
    }

    // 从固件配置表查找设备树（DTB）。
    let dtb = uefi::system::with_config_table(|entries| {
        entries
            .iter()
            .rfind(|entry| entry.guid == FDT_GUID)
            .map_or(0, |entry| entry.address as u64)
    });
    uefi::println!("[exo] DTB source: {}", if dtb == 0 { "embedded QEMU" } else { "firmware" });
    // 将 DTB 复制到保留页，供 EL0 解析；QEMU 缺少固件 DTB 时使用内嵌副本。
    boot::handoff::copy_dtb(bi, dtb)?;
    uefi::println!("[exo] DTB handoff: {} bytes, device discovery runs in EL0", bi.dtb_size);
    Ok(())
}

/// 所有固件协议借用均已结束。返回最终内存图，后续日志由内核 UART 接管。
fn exit_uefi_boot_services(el_raw: u64) -> uefi::mem::memory_map::MemoryMapOwned {
    uefi::println!(
        "[exo] CurrentEL raw=0x{:x}, exc_level={}",
        el_raw,
        (el_raw >> 2) & 3
    );
    uefi::println!("[exo] calling ExitBootServices...");
    // 安全前提：GOP 句柄已释放，交接数据已复制，后续代码只访问保留的内存。
    let mmap = unsafe { uefi::boot::exit_boot_services(uefi::boot::MemoryType::LOADER_DATA) };
    uart::puts("[exo] post-EBS direct UART OK\r\n");
    mmap
}

/// 按固件交付的异常级选择入口，将 BootInfo 的地址放入 x0。
fn enter_el1(bi: &mut BootInfo) -> ! {
    let el_raw = bi.el2.current_el;
    uart::puts("[exo] BootInfo ready at CurrentEL=");
    uart::hex((el_raw >> 2) & 3);
    uart::puts("\r\n");

    #[cfg(feature = "pi5")]
    let copied_el1_entry = prepare_pi5_el1_trampoline(bi);

    // EL2 路径使用独立的 EL1 启动栈，栈顶按 AArch64 调用约定对齐到 16 字节。
    let stack_top =
        (core::ptr::addr_of!(EL1_BOOT_STACK) as u64 + EL1_BOOT_STACK_SIZE as u64) & !0xf;

    // 固件已在 EL1 时，沿用当前栈直接进入 el1_main。
    if ((el_raw >> 2) & 3) == 1 {
        uart::puts("[exo] firmware entered at EL1; bypass EL2 boot shim\r\n");
        trap::enter_el1_direct(
            kmain::el1_main as *const () as u64,
            stack_top,
            bi as *const BootInfo as u64,
        );
    }

    // EL2 路径先安装异常向量，随后以 x0 传递 BootInfo 地址，执行 eret 进入 EL1。
    crate::vectors::install_el2();
    uart::puts("[exo] VBAR_EL2 installed\r\n");
    let el1_vbar = crate::vectors::install_el1_from_el2();
    uart::puts("[exo] VBAR_EL1 preinstalled from EL2=");
    uart::hex(el1_vbar);
    uart::puts("\r\n");

    #[cfg(feature = "pi5")]
    {
        let root = install_pi5_el1_boot_mmu(bi);
        uart::puts("[exo] EL1 boot identity MMU root=");
        uart::hex(root);
        uart::puts(" SCTLR_EL1=");
        uart::hex(mrs!("sctlr_el1"));
        uart::puts("\r\n");
    }

    #[cfg(feature = "pi5")]
    let el1_entry = copied_el1_entry;
    #[cfg(not(feature = "pi5"))]
    let el1_entry = kmain::el1_main as *const () as u64;
    trap::enter_el1_kernel(
        el1_entry,
        stack_top,
        bi as *const BootInfo as u64,
        kmain::el1_main as *const () as u64,
    )
}

/// Pi 5 优先从空闲物理页执行 EL1 跳板；没有可用页时沿用镜像中的入口。
#[cfg(feature = "pi5")]
fn prepare_pi5_el1_trampoline(bi: &mut BootInfo) -> u64 {
    let mut entry = el1_entry_trampoline as *const () as u64;
    if bi.range_count != 0 && bi.ranges[0].pages != 0 {
        let dst = bi.ranges[0].base;
        let src = el1_entry_trampoline as *const u8;
        let end = unsafe { &el1_entry_trampoline_end as *const u8 as u64 };
        let size = end - src as u64;
        // 目标是首段空闲内存的第一页，与镜像中的跳板分离；跳板须能放入一页。
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
}
