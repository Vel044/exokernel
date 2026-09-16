//! 用户态设备驱动和协议栈。

#[cfg(any(
    feature = "ide",
    feature = "app-act-inference",
    feature = "app-act-benchmark",
    feature = "app-robot-act-once"
))]
pub(crate) mod act;

#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
pub(crate) mod dma;
#[cfg(any(feature = "ide", feature = "app-robot-act-once"))]
pub(crate) mod jpeg;
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
pub(crate) mod pci;
#[cfg(any(
    feature = "ide",
    feature = "app-scservo",
    feature = "app-robot-act-once",
    feature = "app-robot-observation",
    feature = "app-robot-action-replay"
))]
pub(crate) mod scservo;
#[cfg(any(
    feature = "ide",
    feature = "app-usb-echo",
    feature = "app-scservo",
    feature = "app-robot-act-once",
    feature = "app-robot-observation",
    feature = "app-robot-action-replay",
    all(
        feature = "app-system-smoke",
        any(feature = "qemu-xhci", feature = "pi5-xhci")
    )
))]
pub(crate) mod usb_serial;
#[cfg(any(
    feature = "ide",
    feature = "app-uvc-smoke",
    feature = "app-robot-act-once",
    feature = "app-robot-observation"
))]
pub(crate) mod uvc;
#[cfg(feature = "user-fs")]
pub(crate) mod virtio_blk;
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
pub(crate) mod xhci;
