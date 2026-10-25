//! EL0 设备发现的结果；不是内核 ABI，也不授予 MMIO/IRQ 权限。
//!
//! libOS 在堆初始化后解析完整 DTB，一次生成这些描述，应用及驱动只读共享。
//! 地址均为 CPU PA；驱动仍须调用 MAP_MMIO / IRQ_BIND，由 EL1 校验平台授权。

pub const XHCI_TRANSPORT_NONE: u32 = 0;
pub const XHCI_TRANSPORT_PCI: u32 = 1;
pub const XHCI_TRANSPORT_DIRECT: u32 = 2;
pub const MAX_PCI_RANGES: usize = 3;
pub const MAX_PCI_INTX_ROUTES: usize = 16;

#[derive(Clone, Copy, Default)]
pub struct DeviceResource {
    /// 寄存器起始 CPU PA，必须经 MAP_MMIO 才可在 EL0 访问。
    pub base: u64,
    /// 寄存器或地址窗口长度，单位为字节。
    pub size: u64,
    /// 设备对应的 GIC INTID，实际绑定仍校验 EL1 授权。
    pub intid: u32,
}

#[derive(Clone, Copy, Default)]
pub struct PciRange {
    /// PCI 地址空间属性或中断触发标志，按所属结构解释。
    pub flags: u32,
    /// PCI 总线侧地址窗口起点。
    pub child_base: u64,
    /// 该窗口在 CPU 物理地址空间的起点。
    pub parent_base: u64,
    /// 寄存器或地址窗口长度，单位为字节。
    pub size: u64,
}

#[derive(Clone, Copy, Default)]
pub struct PciIntxRoute {
    /// PCI 子地址高 cell，包含 BDF 匹配位。
    pub child_address_hi: u32,
    /// PCI INTx 引脚，1..4 对应 INTA..INTD。
    pub pin: u32,
    /// 设备对应的 GIC INTID，实际绑定仍校验 EL1 授权。
    pub intid: u32,
    /// PCI 地址空间属性或中断触发标志，按所属结构解释。
    pub flags: u32,
}

#[derive(Clone, Copy)]
pub struct PciHostInfo {
    /// 是否发现受支持的通用 PCI host。
    pub present: u32,
    /// PCI ECAM 配置空间的 CPU PA。
    pub ecam_pa: u64,
    /// ECAM 地址窗口总长度。
    pub ecam_size: u64,
    /// 可访问总线范围的起始编号。
    pub bus_start: u8,
    /// ranges 中有效窗口数量。
    pub range_count: u8,
    /// intx_routes 中有效路由数量。
    pub intx_route_count: u8,
    /// PCI 子地址匹配掩码。
    pub interrupt_map_mask_hi: u32,
    /// PCI 中断引脚匹配掩码。
    pub interrupt_map_mask_pin: u32,
    /// 用户态解析的 PCI 总线地址到 CPU PA 的窗口。
    pub ranges: [PciRange; MAX_PCI_RANGES],
    /// 用户态解析的 BDF、引脚到 GIC INTID 的路由。
    pub intx_routes: [PciIntxRoute; MAX_PCI_INTX_ROUTES],
}

impl Default for PciHostInfo {
    fn default() -> Self {
        Self {
            present: 0,
            ecam_pa: 0,
            ecam_size: 0,
            bus_start: 0,
            range_count: 0,
            intx_route_count: 0,
            interrupt_map_mask_hi: 0,
            interrupt_map_mask_pin: 0,
            ranges: [PciRange::default(); MAX_PCI_RANGES],
            intx_routes: [PciIntxRoute::default(); MAX_PCI_INTX_ROUTES],
        }
    }
}

/// libOS 从 DTB 构建的设备目录；由启动代码持有，存活至任务退出。
#[derive(Clone, Copy, Default)]
pub struct DeviceInfo {
    /// PL011 的 CPU PA、寄存器长度和 GIC INTID。
    pub uart: DeviceResource,
    /// LiteEth 两段 MMIO 和 slot 布局；仅是描述，EL1 仍审核授权。
    pub liteeth: Option<LiteEthInfo>,
    /// Pi5 直接连接的 RP1 xHCI，QEMU 通过 PCI 枚举填充。
    pub xhci: DeviceResource,
    /// virtio transport 的物理窗口，设备类型由驱动读取寄存器识别。
    pub virtio_mmio: DeviceResource,
    /// 根据设备树选择直接 MMIO、PCI 或无 xHCI。
    pub xhci_transport: u32,
    /// 来自内核 BootInfo 的实际上线 CPU 数，不从 DTB 推测上线状态。
    pub cpu_count: u32,
    /// 用户态解析的 ECAM、PCI 地址窗口与 INTx 路由。
    pub pci: PciHostInfo,
}

/// LiteEth CSR 与设备 SRAM；均为设备 MMIO，不是 DMA 主存。
#[derive(Clone, Copy, Default)]
pub struct LiteEthInfo {
    /// CSR CPU PA、长度与 GIC INTID。
    pub mac: DeviceResource,
    /// RX/TX slot 共享设备 SRAM 的 CPU PA 与长度。
    pub buffer: DeviceResource,
    /// SRAM 中 RX slot 数，TX 紧随 RX。
    pub rx_slots: u32,
    /// SRAM 中 TX slot 数。
    pub tx_slots: u32,
    /// 每个 slot 的字节容量。
    pub slot_size: u32,
    /// MAC地址
    pub address: [u8; 6],
}
