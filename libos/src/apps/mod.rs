//! 可执行的 libOS 实验和机器人应用。

#[cfg(all(any(feature = "qemu-xhci", feature = "pi5-xhci"), feature = "scservo"))]
pub(crate) mod scservo_app;
#[cfg(feature = "system-smoke")]
pub(crate) mod system_smoke;
pub(crate) mod uart_echo;
#[cfg(all(any(feature = "qemu-xhci", feature = "pi5-xhci"), feature = "usb-echo"))]
pub(crate) mod usb_echo;
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
pub(crate) mod usb_task;
