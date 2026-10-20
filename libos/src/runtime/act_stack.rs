//! ACT应用的EL0加载栈。仅使用已有Frame接口，栈VA从ACT arena中显式保留。
//! Kernel只提供普通页映射和退出回收；模型、线程和栈容量策略均由LibOS决定。

pub(crate) fn enter(
    info: &crate::drivers::platform::DeviceInfo,
    entry: extern "C" fn(&crate::drivers::platform::DeviceInfo) -> !,
) -> ! {
    let frame =
        crate::memory::Frame::allocate(256, 1).unwrap_or_else(|_| crate::runtime::exit(0x53e));
    let mapping = frame
        .map(
            0,
            256,
            exo_abi::ACT_INPUT_ARENA_BASE - 1024 * 1024,
            crate::memory::Rights::READ_WRITE,
        )
        .unwrap_or_else(|_| crate::runtime::exit(0x53e));
    // Frame/Mapping句柄没有自动Drop；资源实际由当前task持有，SYS_EXIT统一回收。
    // 切换后不再访问旧栈上的句柄值，也不需要为普通整数句柄调用mem::forget。
    let _mapping = mapping;
    let _frame = frame;
    // SAFETY: 新SP位于独占RW映射末端且16字节对齐，DeviceInfo只读借用保持有效。
    // entry使用C ABI且永不返回；tail branch后不存在旧栈引用或析构路径。
    unsafe {
        core::arch::asm!("mov sp, {stack}", "mov x0, {info}", "br {entry}",
        stack=in(reg) exo_abi::ACT_INPUT_ARENA_BASE, info=in(reg) info,
        entry=in(reg) entry as *const (), options(noreturn));
    }
}
