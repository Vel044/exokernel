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
// IDE模式会同时加载所有互斥场景，许多入口不会被实际分发；隐藏这些预期的
// dead_code/unreachable提示，避免rust-analyzer诊断淹没真正的类型错误。
#![cfg_attr(feature = "ide", allow(dead_code, unreachable_code))]

// xHCI/CrabUSB 会使用 Vec、Box 等堆类型，因此显式链接 `alloc` crate。
extern crate alloc;

mod apps; // 可执行实验和机器人应用；场景选择统一由apps::run分发。
mod drivers; // PCI、DMA、USB、CDC ACM和SCServo等用户态驱动。
#[cfg(feature = "user-fs")]
mod fs; // virtio-blk之上的用户态只读ext4与大文件Frame缓存。
mod kernel_api; // Thread、Endpoint、Notification、Frame等系统调用安全封装。
mod runtime; // 裸机heap、SVC入口、日志和计时器等基础运行时。

// crate 内兼容别名：驱动与Kernel接口保持独立，应用只通过apps::run启动。
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
#[allow(unused_imports)]
pub(crate) use apps::usb_task;
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
#[allow(unused_imports)]
pub(crate) use drivers::{dma, pci};
#[allow(unused_imports)]
pub(crate) use kernel_api::{
    endpoint as ipc, frame as memory, notification, process, thread, vspace,
};
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
    let info: &exo_abi::UserBootInfo = unsafe { &*(boot_info_va as *const exo_abi::UserBootInfo) };

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

    // SAFETY: 这里通过 SYS_FRAME_ALLOC + SYS_FRAME_MAP 向 EL1 申请普通 Frame
    // 并映射到固定 USER_HEAP_BASE，随后才初始化全局分配器；失败则直接退出。
    let heap_pages = exo_abi::USER_HEAP_SIZE / exo_abi::PAGE_SIZE;
    // 4MiB/4KiB = 1024 pages，足够支撑大部分应用的堆分配需求。
    let heap_frame = memory::Frame::allocate(heap_pages, 1).unwrap_or_else(|_| {
        runtime::puts(b"[libos] heap frame alloc failed\r\n");
        runtime::exit(0x103)
    });
    let heap_mapping = heap_frame
        .map(0, heap_pages, exo_abi::USER_HEAP_BASE, memory::Rights::READ_WRITE)
        .unwrap_or_else(|_| {
            runtime::puts(b"[libos] heap frame map failed\r\n");
            runtime::exit(0x104)
        });
    unsafe { runtime::init_heap(heap_mapping.as_ptr(), heap_mapping.len()) };

    // `_start`不再了解具体场景或设备组合。所有可执行路径都从这个稳定边界
    // 进入，由apps模块校验并分发唯一的编译期场景。
    apps::run(info)
}

// no_std 程序必须提供 panic handler；这里避免展开栈和依赖完整运行时。
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // UART 已切换时直接写 PL011；若尚未切换，则退回 SYS_PUTS。
    runtime::puts(b"[libos] panic\r\n");
    // 通知 EL1 以 panic 错误码清理当前任务的映射、IRQ 和 DMA 资源。
    runtime::exit(0x102)
}
