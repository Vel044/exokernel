#![no_std]

pub const PAGE_SIZE: u64 = 4096;

pub const USER_BASE: u64 = 0x4000_0000;
pub const USER_BOOT_INFO_VA: u64 = 0x4020_0000;
pub const UART_VA: u64 = 0x4040_0000;
pub const PCI_ECAM_VA: u64 = 0x4050_0000;
pub const XHCI_VA: u64 = 0x4060_0000;
pub const USER_HEAP_BASE: u64 = 0x4080_0000;
pub const USER_HEAP_SIZE: u64 = 4 * 1024 * 1024;
pub const USER_STACK_TOP: u64 = 0x4100_0000;
pub const USER_STACK_PAGES: u64 = 16;
pub const DMA_ARENA_BASE: u64 = 0x4120_0000;
pub const DMA_ARENA_END: u64 = 0x41e0_0000;
pub const USER_WINDOW_END: u64 = 0x4200_0000;
pub const FRAME_ARENA_BASE: u64 = 0x2_0000_0000;
pub const FRAME_ARENA_END: u64 = 0x2_1000_0000;
pub const THREAD_STACK_PAGES: u64 = 16;
pub const THREAD_STACK_ARENA_BASE: u64 = FRAME_ARENA_END;
pub const THREAD_STACK_ARENA_END: u64 =
    THREAD_STACK_ARENA_BASE + 16 * THREAD_STACK_PAGES * PAGE_SIZE;
pub const THREAD_IPC_BUFFER_BASE: u64 = THREAD_STACK_ARENA_END;
pub const THREAD_IPC_BUFFER_END: u64 = THREAD_IPC_BUFFER_BASE + 16 * PAGE_SIZE;
pub const IPC_MESSAGE_WORDS: usize = 4;

pub const SYS_PUTS: u64 = 2;
pub const SYS_EXIT: u64 = 5;
pub const SYS_MAP_MMIO: u64 = 6;
pub const SYS_IRQ_BIND: u64 = 7;
// 编号8曾用于已删除的直接中断等待接口。为保持ABI编号稳定而保留空洞。
pub const SYS_IRQ_ACK: u64 = 9;
pub const SYS_IRQ_UNBIND: u64 = 10;
pub const SYS_UNMAP_MMIO: u64 = 11;
pub const SYS_DMA_ALLOC: u64 = 12;
pub const SYS_DMA_FREE: u64 = 13;
pub const SYS_FRAME_ALLOC: u64 = 14;
pub const SYS_FRAME_FREE: u64 = 15;
pub const SYS_FRAME_MAP: u64 = 16;
pub const SYS_FRAME_UNMAP: u64 = 17;
pub const SYS_THREAD_CREATE: u64 = 18;
pub const SYS_THREAD_EXIT: u64 = 19;
pub const SYS_THREAD_YIELD: u64 = 20;
pub const SYS_THREAD_SET_PRIORITY: u64 = 21;
pub const SYS_ENDPOINT_CREATE: u64 = 22;
pub const SYS_ENDPOINT_SEND: u64 = 23;
pub const SYS_ENDPOINT_RECV: u64 = 24;
pub const SYS_ENDPOINT_CALL: u64 = 25;
pub const SYS_ENDPOINT_REPLY: u64 = 26;
pub const SYS_ENDPOINT_REPLY_RECV: u64 = 27;
pub const SYS_NOTIFICATION_CREATE: u64 = 28;
pub const SYS_NOTIFICATION_SIGNAL: u64 = 29;
pub const SYS_NOTIFICATION_WAIT: u64 = 30;
pub const SYS_NOTIFICATION_POLL: u64 = 31;
pub const SYS_NOTIFICATION_DESTROY: u64 = 32;
pub const SYS_THREAD_RUNTIME: u64 = 33;
pub const SYS_ENDPOINT_DESTROY: u64 = 34;

/// v1固定支持QEMU virt和Pi5的四个AArch64 CPU。
pub const MAX_CPUS: usize = 4;
/// 静态优先级范围为0..=63，数值越大越先运行。
pub const MAX_THREAD_PRIORITY: u8 = 63;
/// 相同优先级线程的强制轮转时间片。
pub const THREAD_TIME_SLICE_NS: u64 = 1_000_000;
/// IRQ绑定到发起系统调用的当前CPU。
pub const IRQ_TARGET_CURRENT: u64 = u64::MAX;

pub const USER_BOOT_INFO_MAGIC: u32 = 0x4558_4f42;
// v7移除旧IRQ_WAIT，并要求每个IRQ_BIND都携带有效Notification和badge。
pub const USER_BOOT_INFO_VERSION: u16 = 7;
pub const XHCI_TRANSPORT_NONE: u32 = 0;
pub const XHCI_TRANSPORT_PCI: u32 = 1;
pub const XHCI_TRANSPORT_DIRECT: u32 = 2;
pub const MAX_PCI_RANGES: usize = 3;
pub const MAX_PCI_INTX_ROUTES: usize = 16;

pub const FRAME_RIGHT_READ: u64 = 1 << 0;
pub const FRAME_RIGHT_WRITE: u64 = 1 << 1;
pub const FRAME_RIGHT_EXECUTE: u64 = 1 << 2;

pub const SYS_ERR_INVALID: u64 = u64::MAX;
pub const SYS_ERR_NO_MEMORY: u64 = u64::MAX - 1;
pub const SYS_ERR_NO_SLOT: u64 = u64::MAX - 2;
pub const SYS_ERR_DENIED: u64 = u64::MAX - 3;
pub const SYS_ERR_BUSY: u64 = u64::MAX - 4;
pub const SYS_ERR_CONFLICT: u64 = u64::MAX - 5;
pub const SYS_ERR_NOT_FOUND: u64 = u64::MAX - 6;
pub const SYS_ERR_WOULD_BLOCK: u64 = u64::MAX - 7;

pub const fn is_sys_error(value: u64) -> bool {
    value >= SYS_ERR_WOULD_BLOCK
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct FrameHandle(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct MappingHandle(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct ThreadHandle(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct EndpointHandle(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct NotificationHandle(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct ReplyHandle(pub u64);

/// 线程固定 IPC buffer 中的消息格式。
///
/// Kernel 只复制这 48 字节，不保存任意 EL0 指针；大数据通过 Frame
/// 映射后的共享内存传递，IPC 消息只携带控制信息和共享区描述。
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct IpcMessage {
    pub label: u64,
    pub words: [u64; IPC_MESSAGE_WORDS],
    pub reply: ReplyHandle,
}

impl Default for ReplyHandle {
    fn default() -> Self {
        Self(0)
    }
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct DeviceResource {
    pub base: u64,
    pub size: u64,
    pub intid: u32,
    pub irq_flags: u32,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct PciRange {
    pub flags: u32,
    pub reserved: u32,
    pub child_base: u64,
    pub parent_base: u64,
    pub size: u64,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct PciIntxRoute {
    pub child_address_hi: u32,
    pub pin: u32,
    pub intid: u32,
    pub flags: u32,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct PciHostInfo {
    pub present: u32,
    pub dma_coherent: u32,
    pub ecam_pa: u64,
    pub ecam_size: u64,
    pub bus_start: u8,
    pub bus_end: u8,
    pub range_count: u8,
    pub intx_route_count: u8,
    pub interrupt_map_mask_hi: u32,
    pub interrupt_map_mask_pin: u32,
    pub ranges: [PciRange; MAX_PCI_RANGES],
    pub intx_routes: [PciIntxRoute; MAX_PCI_INTX_ROUTES],
}

impl Default for PciHostInfo {
    fn default() -> Self {
        Self {
            present: 0,
            dma_coherent: 0,
            ecam_pa: 0,
            ecam_size: 0,
            bus_start: 0,
            bus_end: 0,
            range_count: 0,
            intx_route_count: 0,
            interrupt_map_mask_hi: 0,
            interrupt_map_mask_pin: 0,
            ranges: [PciRange::default(); MAX_PCI_RANGES],
            intx_routes: [PciIntxRoute::default(); MAX_PCI_INTX_ROUTES],
        }
    }
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct UserBootInfo {
    pub magic: u32,
    pub version: u16,
    pub size: u16,
    pub uart: DeviceResource,
    pub xhci: DeviceResource,
    pub xhci_transport: u32,
    /// Kernel已成功拉起并参与调度的CPU数量。
    pub cpu_count: u32,
    pub pci: PciHostInfo,
    pub heap_base: u64,
    pub heap_size: u64,
    pub dma_arena_base: u64,
    pub dma_arena_end: u64,
    pub frame_arena_base: u64,
    pub frame_arena_end: u64,
}
