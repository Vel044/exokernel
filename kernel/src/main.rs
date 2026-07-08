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
    uefi::println!("[exo] calling ExitBootServices...");
    let mmap = unsafe { uefi::boot::exit_boot_services(uefi::boot::MemoryType::LOADER_DATA) };
    // EBS 之后 UEFI console 已经失效。后续日志必须等 EL1 从 DTB 初始化 UART。

    // ── 第四步: 提取空闲物理内存范围, 放进 BootInfo 给 EL1 外核 ──
    // MemoryMap 里记录了整台机器所有物理内存页的用途:
    //   CONVENTIONAL     → 空闲可用 (Type 7)
    //   LOADER_CODE/DATA → 内核自己占的 (Type 12/13)
    //   BOOT_SERVICES_*  → 固件占的, EBS 后也变空闲了 (Type 3/4)
    // 但我们暂时只收 Type 7, 保证不给内核自己的地址
    bi.fill_memmap(&mmap);
    bi.read_el2();

    // ── 第五步: EL2 boot shim 降级到 EL1 外核 ──
    crate::vectors::install_el2();

    let stack_top = core::ptr::addr_of!(EL1_BOOT_STACK) as u64 + EL1_BOOT_STACK_SIZE as u64;
    trap::enter_el1_kernel(kmain::el1_main as *const () as u64, stack_top, &bi as *const BootInfo as u64)
}
