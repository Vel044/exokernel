//! 已选择平台的启动与授权清单，不执行 DTB 解析或设备枚举。
//!
//! GIC/Timer/CPU 必须在 EL0 启动前就绪；设备访问范围由平台配置授予，
//! libOS 解析 DTB 只获得描述，不能扩张这些授权。新板卡须提供相应平台配置。

use crate::task::{IrqGrant, MmioGrant};

/// PSCI 启动使用的逻辑 CPU 到硬件 MPIDR 对照。
pub struct CpuTopology {
    /// 当前平台要求启动的 CPU 数。
    pub count: usize,
    /// 固件 PSCI_CPU_ON 接口接受的硬件 CPU 标识。
    pub mpidrs: [u64; exo_abi::MAX_CPUS],
}

/// 平台固定的最小启动信息及初始 libOS 可申请的设备范围。
pub struct PlatformResources {
    /// 内核诊断 PL011 的 CPU PA，不用于决定用户态选择哪个 UART。
    pub uart_pa: u64,
    /// GICv2 Distributor / CPU Interface 的 CPU PA，只允许 EL1 访问。
    pub gicd_pa: u64,
    pub gicc_pa: u64,
    /// CNTP / CNTV 的 GIC PPI，分别用于原生 EL2 启动和 HVF EL1 启动。
    pub physical_timer_intid: u32,
    pub virtual_timer_intid: u32,
    /// PSCI CPU 拓扑，不从用户态提交的数据建立。
    pub cpus: CpuTopology,
    /// 页对齐的 MMIO 授权范围，不能包括普通 RAM 或 GIC。
    pub mmio: &'static [MmioGrant],
    /// 设备 GIC SPI 及已审核的触发标志，IRQ_BIND 继续逐项校验。
    pub irqs: &'static [IrqGrant],
}

/// 与 platform/qemu/qemu-virt.dtb、qemu/config.sh 的四核 GICv2 配置一致。
#[cfg(feature = "qemu")]
pub static PLATFORM: PlatformResources = PlatformResources {
    uart_pa: 0x0900_0000,
    gicd_pa: 0x0800_0000,
    gicc_pa: 0x0801_0000,
    physical_timer_intid: 30,
    virtual_timer_intid: 27,
    cpus: CpuTopology {
        count: 4,
        mpidrs: [0, 1, 2, 3],
    },
    mmio: &[
        MmioGrant {
            base: 0x0900_0000,
            size: 0x1000,
        }, // PL011
        MmioGrant {
            base: 0x0a00_0000,
            size: 0x4000,
        }, // 32 个 virtio transport
        MmioGrant {
            base: 0x40_1000_0000,
            size: 0x10_0000,
        }, // PCI bus 0 ECAM
        MmioGrant {
            base: 0x1000_0000,
            size: 0x2eff_0000,
        }, // PCI 32 位 MMIO
        MmioGrant {
            base: 0x80_0000_0000,
            size: 0x80_0000_0000,
        }, // PCI 64 位 MMIO
    ],
    irqs: &[
        IrqGrant {
            intid: 33,
            flags: 4,
        }, // PL011，电平高
        IrqGrant {
            intid: 35,
            flags: 4,
        }, // PCI INTA..INTD
        IrqGrant {
            intid: 36,
            flags: 4,
        },
        IrqGrant {
            intid: 37,
            flags: 4,
        },
        IrqGrant {
            intid: 38,
            flags: 4,
        },
    ],
};

/// Pi5 固件采用 UART10 与已配置的 RP1 直通窗口；保留原先第一路 USB 授权。
/// 配置依据 platform/pi5/bcm2712-rpi-5-b.dtb，运行时 DTB 不可扩大这个边界。
#[cfg(feature = "pi5")]
pub static PLATFORM: PlatformResources = PlatformResources {
    uart_pa: 0x10_7d00_1000,
    gicd_pa: 0x10_7fff_9000,
    gicc_pa: 0x10_7fff_a000,
    physical_timer_intid: 30,
    virtual_timer_intid: 27,
    cpus: CpuTopology {
        count: 4,
        mpidrs: [0, 0x100, 0x200, 0x300],
    },
    mmio: &[
        MmioGrant {
            base: 0x10_7d00_1000,
            size: 0x1000,
        },
        MmioGrant {
            base: 0x1f_0020_0000,
            size: 0x10_0000,
        },
    ],
    irqs: &[
        IrqGrant {
            intid: 153,
            flags: 4,
        }, // UART10
        IrqGrant {
            intid: 158,
            flags: 1,
        }, // 固件已配置的 RP1 USB0，中断上升沿
    ],
};
