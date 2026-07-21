//! 可执行的 libOS 实验和机器人应用。

#[cfg(feature = "frame-smoke")]
pub(crate) mod frame_smoke;
#[cfg(feature = "thread-ipc-smoke")]
pub(crate) mod thread_ipc_smoke;
pub(crate) mod uart_echo;
