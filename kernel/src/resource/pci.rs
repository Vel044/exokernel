//! 从完整 DTB 裁剪通用 PCI host 资源，不在 EL1 识别具体 PCI 设备。

use exo_abi::{PciHostInfo, PciIntxRoute, PciRange, MAX_PCI_INTX_ROUTES, MAX_PCI_RANGES};
use fdt::Fdt;

pub fn from_dtb(dtb_pa: u64) -> Option<PciHostInfo> {
    let tree = unsafe { Fdt::from_ptr(dtb_pa as *const u8) }.ok()?;
    let node = tree.find_compatible(&["pci-host-ecam-generic"])?;
    let reg = node.property("reg")?.value;
    if reg.len() < 16 {
        return None;
    }

    let mut info = PciHostInfo::default();
    info.present = 1;
    info.ecam_pa = read_cells(&reg[0..8])?;
    info.ecam_size = read_cells(&reg[8..16])?;
    if let Some(bus_range) = node.property("bus-range") {
        if bus_range.value.len() >= 8 {
            info.bus_start = read_u32(&bus_range.value[0..4])? as u8;
        }
    }

    if let Some(ranges) = node.property("ranges") {
        for entry in ranges.value.chunks_exact(28).take(MAX_PCI_RANGES) {
            let index = info.range_count as usize;
            info.ranges[index] = PciRange {
                flags: read_u32(&entry[0..4])?,
                child_base: read_cells(&entry[4..12])?,
                parent_base: read_cells(&entry[12..20])?,
                size: read_cells(&entry[20..28])?,
            };
            info.range_count += 1;
        }
    }

    if let Some(mask) = node.property("interrupt-map-mask") {
        if mask.value.len() >= 16 {
            info.interrupt_map_mask_hi = read_u32(&mask.value[0..4])?;
            info.interrupt_map_mask_pin = read_u32(&mask.value[12..16])?;
        }
    }

    if let Some(map) = node.property("interrupt-map") {
        let mut offset = 0usize;
        while offset + 20 <= map.value.len()
            && (info.intx_route_count as usize) < MAX_PCI_INTX_ROUTES
        {
            let child_hi = read_u32(&map.value[offset..offset + 4])?;
            let pin = read_u32(&map.value[offset + 12..offset + 16])?;
            let phandle = read_u32(&map.value[offset + 16..offset + 20])?;
            let parent = tree.find_phandle(phandle)?;
            let parent_addr_cells = parent.cell_sizes().address_cells;
            let parent_irq_cells = parent
                .property("#interrupt-cells")
                .and_then(|property| read_u32(property.value))?
                as usize;
            let parent_spec = offset + 20 + parent_addr_cells * 4;
            let entry_bytes = (3 + 1 + 1 + parent_addr_cells + parent_irq_cells) * 4;
            if parent_spec + parent_irq_cells * 4 > map.value.len() || parent_irq_cells < 3 {
                return None;
            }
            let irq_type = read_u32(&map.value[parent_spec..parent_spec + 4])?;
            let hwirq = read_u32(&map.value[parent_spec + 4..parent_spec + 8])?;
            let flags = read_u32(&map.value[parent_spec + 8..parent_spec + 12])?;
            let intid = match irq_type {
                0 => hwirq.checked_add(32)?,
                1 => hwirq.checked_add(16)?,
                _ => return None,
            };
            let index = info.intx_route_count as usize;
            info.intx_routes[index] = PciIntxRoute {
                child_address_hi: child_hi,
                pin,
                intid,
                flags,
            };
            info.intx_route_count += 1;
            offset = offset.checked_add(entry_bytes)?;
        }
    }
    Some(info)
}

fn read_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(0..4)?.try_into().ok()?))
}

fn read_cells(bytes: &[u8]) -> Option<u64> {
    if bytes.len() != 8 {
        return None;
    }
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}
