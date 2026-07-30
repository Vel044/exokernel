//! Kernel/libOS唯一综合验收入口。
//!
//! 各资源接口仍按职责分文件测试，本模块只规定执行顺序。这样构建系统只需
//! 一个`system-smoke` feature，同时读代码时可以独立查看内存、IPC、调度和
//! 机器人并发链路。

mod frame;
mod ipc;
mod priority;
mod robot;

pub fn run(info: &exo_abi::UserBootInfo) -> ! {
    crate::runtime::puts(b"[libos] ===== system smoke start =====\r\n");
    frame::run();
    ipc::run();
    priority::run();
    robot::run(info)
}
