//! QEMU virt 与 Raspberry Pi 5 共用的内核配置。
//!
//! 这里只放 ABI 和虚拟地址布局。设备物理地址必须来自各平台的 DTB。

pub const PAGE_SIZE: u64 = 4096;

pub const USER_BASE: u64 = 0x4000_0000;
pub const DTB_USER_VA: u64 = 0x4020_0000;
pub const USER_STACK_TOP: u64 = 0x4100_0000;
pub const USER_STACK_PAGES: u64 = 4;
pub const USER_WINDOW_END: u64 = 0x4200_0000;
