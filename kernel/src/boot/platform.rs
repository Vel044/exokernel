//! EL2 启动阶段的平台差异。
//!
//! 正式 EL1/EL0 设备驱动不读取这里的 MMIO 地址，统一通过 DTB 发现设备。

#[cfg(all(feature = "qemu", feature = "pi5"))]
compile_error!("features `qemu` and `pi5` are mutually exclusive");

#[cfg(not(any(feature = "qemu", feature = "pi5")))]
compile_error!("enable exactly one platform feature: `qemu` or `pi5`");

#[cfg(feature = "qemu")]
pub const NAME: &str = "qemu-virt";

#[cfg(feature = "pi5")]
pub const NAME: &str = "raspberry-pi-5";

/// QEMU EDK2 通常不在 UEFI ConfigTable 中提供 FDT，因此允许使用构建时
/// 嵌入的 QEMU virt DTB。Pi5 必须使用固件提供的真实 DTB。
#[cfg(feature = "qemu")]
pub static FALLBACK_DTB: Option<&[u8]> =
    Some(include_bytes!("../../../platform/qemu/qemu-virt.dtb"));

#[cfg(feature = "pi5")]
pub static FALLBACK_DTB: Option<&[u8]> = None;

/// 仅供 Pi5 EL2 -> EL1 bring-up trampoline 使用的诊断地址。
#[cfg(feature = "pi5")]
pub const EARLY_DEBUG_UART_BASE: u64 = 0x107d_001000;
