//! 从完整 DTB 生成 EL1 内部唯一的平台资源快照。
//!
//! UserBootInfo、平台 MMIO 目录和当前任务 grant 都从这个结果派生；
//! 完整 DTB 本身不映射给 EL0。

use crate::dtb::{IrqSpec, RegRange};
use exo_abi::PciHostInfo;
use fdt::Fdt;

#[derive(Clone, Copy)]
pub struct CpuTopology {
    pub count: usize,
    pub mpidrs: [u64; exo_abi::MAX_CPUS],
}

impl CpuTopology {
    const EMPTY: Self = Self {
        count: 0,
        mpidrs: [0; exo_abi::MAX_CPUS],
    };
}

pub struct PlatformResources {
    pub uart: RegRange,
    pub uart_irq: IrqSpec,
    pub gicd_pa: u64,
    pub gicc_pa: u64,
    pub pci: PciHostInfo,
    pub xhci: Option<(RegRange, IrqSpec)>,
    /// 连续的virtio-mmio transport窗口。Kernel只授权MMIO，不识别块设备类型。
    pub virtio_mmio: Option<RegRange>,
    pub timer_irq: IrqSpec,
    pub cpus: CpuTopology,
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
        virtio_mmio: discover_virtio_mmio_window(dtb_pa),
        timer_irq: crate::dtb::find_nonsecure_physical_timer_irq(dtb_pa)?,
        cpus: discover_cpus(dtb_pa)?,
    })
}

/// 合并DTB中的连续`virtio,mmio`节点，供EL0自行读取device_id并选择驱动。
///
/// QEMU virt目前给出32个相邻的0x200字节transport。授权时按4KiB页向外
/// 对齐，但仍从完整DTB计算窗口，避免把0x0a00_0000写死进Kernel策略。
fn discover_virtio_mmio_window(dtb_pa: u64) -> Option<RegRange> {
    let tree = unsafe { Fdt::from_ptr(dtb_pa as *const u8) }.ok()?;
    let mut first = u64::MAX;
    let mut last = 0u64;
    let mut count = 0usize;
    for node in tree.all_nodes() {
        let compatible = node
            .compatible()
            .is_some_and(|list| list.all().any(|value| value == "virtio,mmio"));
        if !compatible {
            continue;
        }
        let region = node.reg()?.next()?;
        let base = region.starting_address as u64;
        let size = region.size? as u64;
        let end = base.checked_add(size)?;
        first = first.min(base);
        last = last.max(end);
        count += 1;
    }
    if count == 0 || first >= last {
        return None;
    }
    let page_first = first & !(exo_abi::PAGE_SIZE - 1);
    let page_last = last.checked_add(exo_abi::PAGE_SIZE - 1)? & !(exo_abi::PAGE_SIZE - 1);
    Some(RegRange {
        base: page_first,
        size: page_last.checked_sub(page_first)?,
    })
}

/// QEMU virt和Pi5都通过PSCI SMC启动辅助核。这里只解析CPU硬件ID，
/// PSCI调用和辅助核入口属于AArch64架构层，不放进DTB解析器。
fn discover_cpus(dtb_pa: u64) -> Option<CpuTopology> {
    let tree = unsafe { Fdt::from_ptr(dtb_pa as *const u8) }.ok()?;
    let psci = tree.find_node("/psci")?;
    if psci.property("method")?.value != b"smc\0" {
        return None;
    }
    let cpus = tree.find_node("/cpus")?;
    let address_cells = cpus
        .property("#address-cells")
        .and_then(|property| read_be_u32(property.value))
        .unwrap_or(1) as usize;
    if address_cells == 0 || address_cells > 2 {
        return None;
    }

    let mut topology = CpuTopology::EMPTY;
    for node in cpus.children() {
        if !node.name.starts_with("cpu@") {
            continue;
        }
        if let Some(status) = node.property("status") {
            if status.value != b"okay\0" && status.value != b"ok\0" {
                continue;
            }
        }
        if let Some(method) = node.property("enable-method") {
            if method.value != b"psci\0" {
                return None;
            }
        }
        let reg = node.property("reg")?.value;
        if reg.len() < address_cells * 4 || topology.count == exo_abi::MAX_CPUS {
            return None;
        }
        let mut mpidr = 0u64;
        let mut cell = 0usize;
        while cell < address_cells {
            let start = cell * 4;
            mpidr = (mpidr << 32) | read_be_u32(&reg[start..start + 4])? as u64;
            cell += 1;
        }
        topology.mpidrs[topology.count] = mpidr;
        topology.count += 1;
    }
    if topology.count == exo_abi::MAX_CPUS {
        Some(topology)
    } else {
        None
    }
}

fn read_be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}
