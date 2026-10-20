//! Kernel/libOS唯一综合验收入口。
//!
//! 各资源接口仍按职责分文件测试，本模块只规定执行顺序。这样构建系统只需
//! 一个`system-smoke` feature，同时读代码时可以独立查看内存、IPC、调度和
//! 机器人并发链路。

mod frame;
mod ipc;
mod priority;
mod robot;

pub fn run(info: &crate::drivers::platform::DeviceInfo) -> ! {
    crate::runtime::puts(b"[libos] ===== system smoke start =====\r\n");
    dtb_handoff_smoke(info);
    frame::run();
    ipc::run();
    priority::run();
    robot::run(info)
}

/// 验证完整 DTB 可读，且交接映射不能被资源系统调用覆盖。
fn dtb_handoff_smoke(info: &crate::drivers::platform::DeviceInfo) {
    let check = || -> Result<(), u64> {
        // SAFETY: 启动入口已经校验完整只读 DTB；这里只读取固定 VA 的头部。
        let magic = unsafe { (exo_abi::USER_DTB_VA as *const u32).read_volatile() };
        if u32::from_be(magic) != 0xd00d_feed {
            return Err(0x170);
        }
        // x0=授权 UART PA，x1=页长，x2=DTB VA；SVC 应拒绝覆盖保留窗口。
        if crate::runtime::map_mmio(info.uart.base & !4095, 4096, exo_abi::USER_DTB_VA) != Err(5) {
            return Err(0x171);
        }
        // 以合法 Frame 请求覆盖初始 VSpace 的只读 DTB，页表冲突必须保持原映射。
        let frame = crate::memory::Frame::allocate(1, 1)?;
        let mapping = crate::runtime::frame_map_to(
            frame.raw_handle(),
            // 初始 VSpace 使用 slot 0、generation 1 的完整句柄，不能省略 generation。
            exo_abi::VSpaceHandle(0x1_0000_0001),
            0,
            1,
            exo_abi::USER_DTB_VA,
            exo_abi::FRAME_RIGHT_WRITE | exo_abi::FRAME_RIGHT_READ,
        );
        if mapping != Err(exo_abi::SYS_ERR_CONFLICT) {
            return Err(0x172);
        }
        frame.free()?;
        // 用 Timer PPI 验证即使知道 INTID，也无法绑定内核专用中断。
        let notification = crate::notification::Notification::create()?;
        if notification.bind_irq(30, 1).is_ok() {
            return Err(0x173);
        }
        notification.destroy()?;
        Ok(())
    };
    if let Err(error) = check() {
        crate::runtime::exit(error);
    }
    crate::runtime::puts(b"[libos] DTB handoff + mapping protection passed\r\n");
}
