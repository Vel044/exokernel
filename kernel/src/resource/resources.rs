//! 从完整 DTB 生成 EL1 内部唯一的平台资源快照。
//!
//! UserBootInfo、平台 MMIO 目录和当前任务 grant 都从这个结果派生；
//! 完整 DTB 本身不映射给 EL0。

use crate::dtb::{IrqSpec, RegRange};
use exo_abi::PciHostInfo;

pub struct PlatformResources {
    pub uart: RegRange,
    pub uart_irq: IrqSpec,
    pub gicd_pa: u64,
    pub gicc_pa: u64,
    pub pci: PciHostInfo,
    pub xhci: Option<(RegRange, IrqSpec)>,
}

pub fn discover(dtb_pa: u64) -> Option<PlatformResources> {
    crate::dtb::total_size(dtb_pa)?;
    let (gicd_pa, gicc_pa) = crate::dtb::find_gic_regs(dtb_pa)?;
    Some(PlatformResources {
        uart: crate::dtb::find_pl011_reg(dtb_pa)?,
        uart_irq: crate::dtb::find_uart_irq(dtb_pa)?,
        gicd_pa,
        gicc_pa,
        pci: crate::pci::from_dtb(dtb_pa).unwrap_or_default(),
        xhci: crate::dtb::find_rp1_xhci(dtb_pa),
    })
}
