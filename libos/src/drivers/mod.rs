//! 用户态设备驱动和协议栈。

#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
pub(crate) mod dma;
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
pub(crate) mod pci;
#[cfg(all(any(feature = "qemu-xhci", feature = "pi5-xhci"), feature = "scservo"))]
pub(crate) mod scservo;
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
pub(crate) mod usb;
