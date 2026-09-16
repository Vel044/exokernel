//! 用户态文件系统层。
//!
//! 当前只提供QEMU virtio-blk上的只读ext4。Kernel不保存inode、路径、文件
//! offset或页缓存；libOS把块设备扇区接口组合成`mount/open/read`语义。

#[cfg(any(feature = "app-uvc-smoke", feature = "app-robot-observation"))]
mod capture;
#[cfg(any(
    feature = "ide",
    feature = "app-act-inference",
    feature = "app-act-benchmark",
    feature = "app-robot-act-once",
    feature = "app-robot-action-replay"
))]
mod ext4;

#[cfg(feature = "app-uvc-smoke")]
pub(crate) use capture::CaptureVolume;
#[cfg(feature = "app-robot-observation")]
pub(crate) use capture::ObservationVolume;
#[cfg(any(
    feature = "ide",
    feature = "app-act-inference",
    feature = "app-act-benchmark",
    feature = "app-robot-act-once",
    feature = "app-robot-action-replay"
))]
pub use ext4::FileSystem;
