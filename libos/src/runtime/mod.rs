//! EL0 libOS运行时和最薄的系统调用封装。
//!
//! 驱动相关调用链统一是：
//!
//! ```text
//! Rust安全封装
//!   → runtime::svc/svc5把编号和参数放入x8、x0..x4
//!   → svc #0同步陷入EL1
//!   → EL1 vectors保存TrapFrame
//!   → syscall::dispatch校验资源并操作页表/GIC/物理内存
//!   → eret返回EL0，结果位于x0
//! ```
//!
//! MMIO/DMA建立映射后，驱动通过返回的EL0 VA直接读写；只有申请、释放、
//! 阻塞等待和ACK等保护边界需要再次执行SVC。

use core::alloc::{GlobalAlloc, Layout};
use core::arch::asm;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use linked_list_allocator::LockedHeap;

#[cfg(any(
    feature = "app-act-benchmark",
    feature = "app-act-inference",
    feature = "app-robot-act-once"
))]
pub(crate) mod act_parallel;

#[global_allocator]
static HEAP: CountingHeap = CountingHeap;
static HEAP_STORAGE: LockedHeap = LockedHeap::empty();
static HEAP_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static DIRECT_UART_READY: AtomicBool = AtomicBool::new(false);

/// 在不改变实际分配器的前提下记录 alloc 调用次数。
///
/// ACT steady-state 只使用调用者提供的 Frame workspace；这个计数器用于把
/// “没有隐含堆分配”变成 LibOS 串口中的可验证证据。计数器本身是原子读写，
/// 不参与分配路径的判定，也不在 EL1 中维护状态。
struct CountingHeap;

unsafe impl GlobalAlloc for CountingHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HEAP_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY:调用者遵守GlobalAlloc契约；LockedHeap负责内部空闲链表锁。
        unsafe { GlobalAlloc::alloc(&HEAP_STORAGE, layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY:指针和布局由对应的alloc调用产生，交回同一个LockedHeap。
        unsafe { GlobalAlloc::dealloc(&HEAP_STORAGE, pointer, layout) }
    }
}

pub unsafe fn init_heap(base: *mut u8, size: usize) {
    // heap backing pages和VA由EL0在启动时通过Frame系统调用建立。此后
    // Vec/Box的小块分配只操作EL0中的LockedHeap，不会每次malloc都陷入Kernel。

    // 使用Rust第三方linked_list_allocator，把已映射的4MiB内存
    // 管理成Heap，供Vec、Box、String等动态对象分配。
    HEAP_STORAGE.lock().init(base, size);
}

/// 返回进程内累计的 GlobalAlloc 调用次数；只读，不分配内存。
pub(crate) fn allocation_count() -> usize {
    HEAP_ALLOCATIONS.load(Ordering::Acquire)
}

/// 为当前CPU上的ACT数值内核启用浮点非正规数flush-to-zero。
///
/// `FPCR.FZ`控制FP32/FP64，`FZ16`控制FP16。ACT当前只计算FP32，但同时设置
/// 两位可避免后续半精度内核落入不同模式。普通数、NaN和Inf语义不变；非常
/// 接近零的subnormal按零处理，既符合常见推理后端行为，也避免QEMU TCG用
/// 极慢的软件路径逐条模拟。FPCR是每个执行CPU的状态，所以每个worker都调用。
#[cfg(any(
    feature = "app-act-benchmark",
    feature = "app-act-inference",
    feature = "app-robot-act-once"
))]
pub(crate) fn configure_act_fpcr() {
    let mut fpcr: u64;
    unsafe {
        core::arch::asm!("mrs {value}, fpcr", value = out(reg) fpcr, options(nomem, nostack));
        fpcr |= (1 << 24) | (1 << 19);
        core::arch::asm!("msr fpcr, {value}", value = in(reg) fpcr, options(nomem, nostack));
        core::arch::asm!("isb", options(nomem, nostack));
    }
}

pub fn svc(sysno: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    // 三参数系统调用复用统一的五参数入口，未使用参数清零。
    svc5(sysno, arg0, arg1, arg2, 0, 0)
}

pub fn svc5(sysno: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    // AArch64外核ABI：x8保存系统调用号，x0..x4保存参数，返回值放回x0。
    let ret: u64;
    unsafe {
        asm!(
            // SVC使CPU从EL0同步陷入EL1，并按VBAR_EL1选择同步异常入口。
            "svc #0",
            inlateout("x8") sysno => _,
            inlateout("x0") arg0 => ret,
            inlateout("x1") arg1 => _,
            inlateout("x2") arg2 => _,
            inlateout("x3") arg3 => _,
            inlateout("x4") arg4 => _,
            lateout("x5") _, lateout("x6") _,
            lateout("x7") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
            lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
            lateout("x16") _, lateout("x17") _,
            // SVC后会执行任意EL1 Rust代码。即使Kernel最终恢复完整线程
            // 上下文，也必须按函数调用边界告诉LLVM：所有AAPCS调用者保存
            // GPR/FP/SIMD寄存器都不可承载跨SVC局部值。
            clobber_abi("C"),
            options(nostack)
        );
    }
    ret
}

/// 六参数 SVC 入口。AArch64 的 x0..x5 都是普通参数寄存器，x8仍保存
/// 系统调用号；这里只给需要第六个参数的低级 ABI 使用，例如 FRAME_MAP_TO
/// 的 rights。绝大多数调用继续使用 svc/svc5，保持调用面简单。
pub fn svc6(sysno: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x8") sysno => _,
            inlateout("x0") arg0 => ret,
            inlateout("x1") arg1 => _,
            inlateout("x2") arg2 => _,
            inlateout("x3") arg3 => _,
            inlateout("x4") arg4 => _,
            inlateout("x5") arg5 => _,
            lateout("x6") _, lateout("x7") _, lateout("x9") _, lateout("x10") _,
            lateout("x11") _, lateout("x12") _, lateout("x13") _, lateout("x14") _,
            lateout("x15") _, lateout("x16") _, lateout("x17") _,
            clobber_abi("C"),
            options(nostack)
        );
    }
    ret
}

pub fn frame_alloc(pages: u64, align_pages: u64) -> Result<exo_abi::FrameHandle, u64> {
    // 请求Kernel分配普通Frame并返回不透明句柄。
    let result = svc(exo_abi::SYS_FRAME_ALLOC, pages, align_pages, 0);
    if exo_abi::is_sys_error(result) {
        Err(result)
    } else {
        Ok(exo_abi::FrameHandle(result))
    }
}

pub fn frame_free(handle: exo_abi::FrameHandle) -> Result<(), u64> {
    // 请求Kernel释放当前任务拥有的Frame。
    match svc(exo_abi::SYS_FRAME_FREE, handle.0, 0, 0) {
        0 => Ok(()),
        error => Err(error),
    }
}

pub fn frame_map(
    frame: exo_abi::FrameHandle,
    offset_pages: u64,
    pages: u64,
    va: u64,
    rights: u64,
) -> Result<exo_abi::MappingHandle, u64> {
    // 请求Kernel按权限把Frame映射到当前EL0 VA。
    let result = svc5(
        exo_abi::SYS_FRAME_MAP,
        frame.0,
        offset_pages,
        pages,
        va,
        rights,
    );
    if exo_abi::is_sys_error(result) {
        Err(result)
    } else {
        Ok(exo_abi::MappingHandle(result))
    }
}

/// 把当前 owner 的 Frame 映射到一个由它创建的目标 VSpace。
pub fn frame_map_to(
    frame: exo_abi::FrameHandle,
    target: exo_abi::VSpaceHandle,
    offset_pages: u64,
    pages: u64,
    va: u64,
    rights: u64,
) -> Result<exo_abi::MappingHandle, u64> {
    // 请求Kernel把当前Frame映射到另一个用户VSpace。
    let result = svc6(
        exo_abi::SYS_FRAME_MAP_TO,
        frame.0,
        target.0,
        offset_pages,
        pages,
        va,
        rights,
    );
    if exo_abi::is_sys_error(result) {
        Err(result)
    } else {
        Ok(exo_abi::MappingHandle(result))
    }
}

pub fn vspace_create() -> Result<exo_abi::VSpaceHandle, u64> {
    let result = svc(exo_abi::SYS_VSPACE_CREATE, 0, 0, 0);
    if exo_abi::is_sys_error(result) {
        Err(result)
    } else {
        Ok(exo_abi::VSpaceHandle(result))
    }
}

pub fn vspace_destroy(handle: exo_abi::VSpaceHandle) -> Result<(), u64> {
    match svc(exo_abi::SYS_VSPACE_DESTROY, handle.0, 0, 0) {
        0 => Ok(()),
        error => Err(error),
    }
}

pub fn frame_unmap(handle: exo_abi::MappingHandle) -> Result<(), u64> {
    // 请求Kernel撤销当前任务拥有的Frame映射。
    match svc(exo_abi::SYS_FRAME_UNMAP, handle.0, 0, 0) {
        0 => Ok(()),
        error => Err(error),
    }
}

pub fn puts(bytes: &[u8]) {
    if DIRECT_UART_READY.load(Ordering::Acquire) {
        direct_uart_puts(bytes);
        return;
    }
    svc(
        exo_abi::SYS_PUTS,
        bytes.as_ptr() as u64,
        bytes.len() as u64,
        0,
    );
}

/// UART MMIO 映射和 smoke test 成功后，将 libOS 日志切换为 EL0 直写。
pub fn enable_direct_uart() {
    DIRECT_UART_READY.store(true, Ordering::Release);
}

fn direct_uart_puts(bytes: &[u8]) {
    let pointer = NonNull::new(exo_abi::UART_VA as *mut arm_pl011_uart::PL011Registers)
        .expect("mapped UART VA must be non-null");
    let mmio = unsafe { arm_pl011_uart::UniqueMmioPointer::new(pointer) };
    let mut uart = arm_pl011_uart::Uart::new(mmio);
    for &byte in bytes {
        while uart.is_tx_fifo_full() {
            core::hint::spin_loop();
        }
        uart.write_word(byte);
    }
}

pub fn hex(value: u64) {
    let mut buffer = [b'0'; 18];
    buffer[0] = b'0';
    buffer[1] = b'x';
    let mut index = 0usize;
    while index < 16 {
        let nibble = ((value >> (60 - index * 4)) & 0xf) as u8;
        buffer[index + 2] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + nibble - 10
        };
        index += 1;
    }
    puts(&buffer);
}

pub fn exit(code: u64) -> ! {
    svc(exo_abi::SYS_EXIT, code, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

pub fn map_mmio(pa: u64, size: u64, va: u64) -> Result<(), u64> {
    // SYS_MAP_MMIO寄存器约定：
    // x8=SYS_MAP_MMIO，x0=设备PA，x1=字节数，x2=期望的EL0 VA。
    // EL1会验证PA grant、页对齐、VA冲突，再用Device-nGnRE属性建立页表。
    // 成功以后驱动直接使用va访问设备，不需要为每个寄存器读写调用Kernel。
    match svc(exo_abi::SYS_MAP_MMIO, pa, size, va) {
        0 => Ok(()),
        error => Err(error),
    }
}

pub fn unmap_mmio(va: u64, size: u64) -> Result<(), u64> {
    // Kernel只接受与任务记录完全匹配的映射，撤销PTE后执行TLBI。
    match svc(exo_abi::SYS_UNMAP_MMIO, va, size, 0) {
        0 => Ok(()),
        error => Err(error),
    }
}

pub fn dma_alloc(size: usize, alignment: usize, va: u64) -> Result<u64, u64> {
    // x0=size、x1=alignment、x2=EL0 VA。Kernel分配连续且清零的物理页，
    // 以Normal Non-cacheable映射到va，并在无IOMMU的v1中返回PA作为
    // device-visible DMA address。CPU指针仍然是调用者指定的va。
    let result = svc(exo_abi::SYS_DMA_ALLOC, size as u64, alignment as u64, va);
    if result == u64::MAX {
        Err(result)
    } else {
        Ok(result)
    }
}

pub fn dma_free(va: u64, size: usize) -> Result<(), u64> {
    // 释放时不接受裸PA，而是用Kernel记录的VA+size核对当前任务所有权。
    match svc(exo_abi::SYS_DMA_FREE, va, size as u64, 0) {
        0 => Ok(()),
        error => Err(error),
    }
}

/// 将硬件 IRQ 绑定到 Notification。绑定后 IRQ 到达会由 Kernel 合并
/// badge 并唤醒等待线程；用户处理完设备状态后仍需调用 irq_ack。
pub fn irq_bind_notification(
    intid: u32,
    notification: exo_abi::NotificationHandle,
    badge: u64,
) -> Result<(), u64> {
    // x0=GIC INTID、x1=NotificationHandle、x2=badge。
    // Kernel检查IRQ grant和Notification owner后配置GIC SPI。
    match svc5(
        exo_abi::SYS_IRQ_BIND,
        intid as u64,
        notification.0,
        badge,
        exo_abi::IRQ_TARGET_CURRENT,
        0,
    ) {
        0 => Ok(()),
        error => Err(error),
    }
}

pub fn irq_ack(intid: u32) {
    // Notification模式的IRQ handler已经EOI并暂时disable SPI；ACK表示
    // EL0已经消费完设备Event Ring，Kernel现在可以重新enable该SPI。
    let _ = svc(exo_abi::SYS_IRQ_ACK, intid as u64, 0, 0);
}

pub fn irq_unbind(intid: u32) -> Result<(), u64> {
    match svc(exo_abi::SYS_IRQ_UNBIND, intid as u64, 0, 0) {
        0 => Ok(()),
        error => Err(error),
    }
}

pub fn counter() -> u64 {
    let value: u64;
    unsafe { asm!("mrs {}, cntvct_el0", out(reg) value, options(nomem, nostack)) };
    value
}

pub fn counter_frequency() -> u64 {
    let value: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) value, options(nomem, nostack)) };
    value
}

pub fn delay_ns(nanoseconds: u64) {
    let ticks = (counter_frequency() as u128 * nanoseconds as u128 / 1_000_000_000) as u64;
    let deadline = counter().wrapping_add(ticks);
    while (counter().wrapping_sub(deadline) as i64) < 0 {
        core::hint::spin_loop();
    }
}

#[alloc_error_handler]
fn alloc_error(_: Layout) -> ! {
    puts(b"[libos] heap exhausted\r\n");
    exit(0x100)
}
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
pub(crate) mod logger;
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
pub(crate) mod usb_executor;
