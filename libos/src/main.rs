//! EL0 libOS 入口。
//!
//! EL1 加载 ELF、建立用户地址空间后，把只读 `UserBootInfo` 的 EL0
//! 虚拟地址放入 `x0`，再通过 `eret` 跳到 `_start`。本文件只负责公共
//! 启动流程；设备访问、syscall 封装和具体驱动分别放在其他模块中。

// 裸机目标没有操作系统标准库，只使用 `core` 和显式引入的 `alloc`。
#![no_std]
// 不使用 Rust 运行时提供的 `main`，EL1 会直接进入下面的 `_start`。
#![no_main]
// 为裸机堆分配失败提供自定义处理函数；实现位于 runtime 模块。
#![feature(alloc_error_handler)]

// xHCI/CrabUSB 会使用 Vec、Box 等堆类型，因此显式链接 `alloc` crate。
extern crate alloc;

mod apps;
mod drivers;
mod kernel_api;
mod runtime;

// crate 内兼容别名：先完成职责分层，不在同一次改动里重写驱动逻辑。
#[cfg(feature = "frame-smoke")]
#[allow(unused_imports)]
pub(crate) use apps::frame_smoke as frame_test;
#[cfg(feature = "thread-ipc-smoke")]
#[allow(unused_imports)]
pub(crate) use apps::thread_ipc_smoke as thread_test;
#[allow(unused_imports)]
pub(crate) use apps::uart_echo;
#[cfg(all(any(feature = "qemu-xhci", feature = "pi5-xhci"), feature = "scservo"))]
#[allow(unused_imports)]
pub(crate) use drivers::scservo;
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
#[allow(unused_imports)]
pub(crate) use drivers::{dma, pci, usb as usb_app};
#[allow(unused_imports)]
pub(crate) use kernel_api::{endpoint as ipc, frame as memory, notification, thread};
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
#[allow(unused_imports)]
pub(crate) use runtime::logger;

// 保留固定符号名，确保 ELF 入口和 EL1 查找的 `_start` 名称完全一致。
#[no_mangle]
// 把入口单独放进 `.text.entry`，由链接脚本置于用户代码起始位置。
#[link_section = ".text.entry"]
// AArch64 调用约定下，第一个参数位于 x0；这里收到 UserBootInfo 的 EL0 VA。
// 返回类型 `!` 表示 libOS 不会返回 EL1，只会运行或通过 SYS_EXIT 退出。
pub extern "C" fn _start(boot_info_va: u64) -> ! {
    // SAFETY: EL1 已把该只读页面映射到当前 EL0 地址空间，并保证其布局与
    // exo_abi::UserBootInfo 一致；这里把数值虚拟地址转换成只读 Rust 引用。
    let info = unsafe { &*(boot_info_va as *const exo_abi::UserBootInfo) };

    // magic 防止把任意页面误认成 BootInfo，version 防止 EL1/EL0 ABI 不匹配。
    if info.magic != exo_abi::USER_BOOT_INFO_MAGIC
        || info.version != exo_abi::USER_BOOT_INFO_VERSION
        || info.size as usize != core::mem::size_of::<exo_abi::UserBootInfo>()
    {
        // 此时 UART MMIO 尚未初始化，puts 会通过 SYS_PUTS 请求 EL1 输出。
        runtime::puts(b"[libos] invalid UserBootInfo\r\n");
        // 把启动 ABI 错误码交给 EL1；该 syscall 不返回。
        runtime::exit(0x101);
    }

    // SAFETY: heap 的 VA、大小和 backing pages 均由 EL1 创建并写入 BootInfo；
    // 初始化后 alloc crate 才能安全使用该固定用户态堆。
    unsafe { runtime::init_heap(info) };

    #[cfg(feature = "thread-ipc-smoke")]
    thread_test::run();

    #[cfg(feature = "frame-smoke")]
    frame_test::run();

    // QEMU 综合测试路径：先验证 EL0 UART，再启动 PCI/xHCI/FTDI USB 栈。
    #[cfg(all(
        not(feature = "frame-smoke"),
        not(feature = "thread-ipc-smoke"),
        any(feature = "qemu-xhci", feature = "pi5-xhci")
    ))]
    {
        // smoke_test 会申请 UART MMIO、直接发送测试字符串并验证 IRQ
        // bind/unbind。成功后保留 UART 映射，并让 runtime::puts 从
        // SYS_PUTS 切换为 EL0 直接访问 PL011 寄存器。
        if let Err(error) = uart_echo::smoke_test(info) {
            runtime::puts(b"[libos] UART smoke failed=");
            runtime::hex(error);
            runtime::puts(b"\r\n");
            runtime::exit(0x103);
        }

        // 从这里开始，libOS 与 CrabUSB 日志都由已初始化的 EL0 UART 输出；
        // xHCI 仍通过 MMIO、IRQ、DMA syscall 向 EL1申请受保护资源。
        runtime::puts(b"[libos] 2. xHCI/CDC ACM application\r\n");
        usb_app::run(info)
    }

    // Pi5/纯 UART 构建路径：不编译 PCI/xHCI，直接进入 IRQ 驱动的回显循环。
    #[cfg(all(
        not(feature = "frame-smoke"),
        not(feature = "thread-ipc-smoke"),
        not(any(feature = "qemu-xhci", feature = "pi5-xhci"))
    ))]
    {
        uart_echo::run(info)
    }
}

// no_std 程序必须提供 panic handler；这里避免展开栈和依赖完整运行时。
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // UART 已切换时直接写 PL011；若尚未切换，则退回 SYS_PUTS。
    runtime::puts(b"[libos] panic\r\n");
    // 通知 EL1 以 panic 错误码清理当前任务的映射、IRQ 和 DMA 资源。
    runtime::exit(0x102)
}
