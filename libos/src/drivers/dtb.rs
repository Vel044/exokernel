//! EL0 完整设备树解析与设备目录生成。
//!
//! 输入是 EL1 只读映射的有界 DTB 字节；先校验容器和节点边界，再由 fdt
//! 遍历属性。地址翻译、设备选择和 PCI 路由都在 EL0，不据此授予访问权限。

use super::platform::*;
use alloc::{format, string::String, vec::Vec};
use fdt::{node::FdtNode, Fdt};

/// 不支持的设备布局明确失败，不猜测寄存器地址或中断号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidFdt,
    MissingUart,
    InvalidResource,
    UnsupportedInterrupt,
    InvalidPci,
}

/// CPU 数来自内核启动交接，设备树中的 CPU 节点不代表实际上线状态。
pub fn discover(bytes: &[u8], cpu_count: u32) -> Result<DeviceInfo, Error> {
    validate(bytes)?;
    let tree = Fdt::new(bytes).map_err(|_| Error::InvalidFdt)?;
    let mut paths = Vec::new();
    collect_paths(
        tree.find_node("/").ok_or(Error::InvalidFdt)?,
        "",
        &mut paths,
    );
    let uart_path = stdout_path(&tree)
        .or_else(|| {
            paths
                .iter()
                .find(|path| tree.find_node(path).is_some_and(is_pl011))
                .map(String::as_str)
        })
        .ok_or(Error::MissingUart)?;
    let uart = tree.find_node(uart_path).ok_or(Error::MissingUart)?;
    if !is_pl011(uart) || !paths.iter().any(|path| path == uart_path) {
        return Err(Error::MissingUart);
    }
    let mut info = DeviceInfo {
        cpu_count,
        uart: resource(&tree, uart_path, false)?,
        ..Default::default()
    };
    // 找到通用 ECAM 节点但解析失败时显式报错，不把它当作无 PCI。
    if paths.iter().any(|path| {
        tree.find_node(path)
            .is_some_and(|node| compatible(node, "pci-host-ecam-generic"))
    }) {
        info.pci = super::pci_host::from_tree(&tree).ok_or(Error::InvalidPci)?;
    }
    let mut virtio = Vec::new();
    for path in &paths {
        let node = tree.find_node(path).ok_or(Error::InvalidFdt)?;
        if compatible(node, "snps,dwc3") && path.contains("/rp1/") && info.xhci.size == 0 {
            info.xhci = resource(&tree, path, true)?;
        }
        if compatible(node, "litex,liteeth") {
            if info.liteeth.is_some() {
                return Err(Error::InvalidResource);
            }
            let names = node
                .property("reg-names")
                .ok_or(Error::InvalidResource)?
                .value;
            let names: Vec<_> = names.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
            // 原驱动按名字取资源，允许标准 DTB 在中间附带当前未用的 MDIO 窗口。
            if names.first().copied() != Some(b"mac".as_slice()) {
                return Err(Error::InvalidResource);
            }
            let buffer_index = names
                .iter()
                .position(|n| *n == b"buffer")
                .ok_or(Error::InvalidResource)?;
            let mac = resource(&tree, path, false)?;
            let (base, size) = reg_at(&tree, path, buffer_index)?;
            let property = |key, default| -> Result<u32, Error> {
                node.property(key).map_or(Ok(default), |p| {
                    if p.value.len() == 4 {
                        u32be(p.value).ok_or(Error::InvalidResource)
                    } else {
                        Err(Error::InvalidResource)
                    }
                })
            };
            let rx_slots = property("litex,rx-slots", 2)?;
            let tx_slots = property("litex,tx-slots", 2)?;
            let slot_size = property("litex,slot-size", 2048)?;
            if rx_slots == 0
                || tx_slots == 0
                || rx_slots > 256
                || tx_slots > 256
                || slot_size == 0
                || slot_size > 2048
                || (rx_slots as u64 + tx_slots as u64) * slot_size as u64 > size
            {
                return Err(Error::InvalidResource);
            }
            let mut address = [0; 6];
            if let Some(p) = node
                .property("local-mac-address")
                .or_else(|| node.property("mac-address"))
            {
                address = p.value.try_into().map_err(|_| Error::InvalidResource)?;
            }
            info.liteeth = Some(LiteEthInfo {
                mac,
                buffer: DeviceResource {
                    base,
                    size,
                    intid: 0,
                },
                rx_slots,
                tx_slots,
                slot_size,
                address,
            });
        }
        if compatible(node, "virtio,mmio") {
            virtio.push(reg(&tree, path)?);
        }
    }
    if !virtio.is_empty() {
        virtio.sort_unstable_by_key(|region| region.0);
        let first = virtio[0].0;
        let mut end = first;
        for (base, size) in virtio {
            if base != end {
                return Err(Error::InvalidResource);
            }
            end = base.checked_add(size).ok_or(Error::InvalidResource)?;
        }
        let base = first & !4095;
        let end = end.checked_add(4095).ok_or(Error::InvalidResource)? & !4095;
        info.virtio_mmio = DeviceResource {
            base,
            size: end - base,
            intid: 0,
        };
    }
    info.xhci_transport = if info.xhci.size != 0 {
        XHCI_TRANSPORT_DIRECT
    } else if info.pci.present != 0 {
        XHCI_TRANSPORT_PCI
    } else {
        XHCI_TRANSPORT_NONE
    };
    Ok(info)
}
fn compatible(node: FdtNode<'_, '_>, name: &str) -> bool {
    node.compatible()
        .is_some_and(|list| list.all().any(|item| item == name))
}
fn is_pl011(node: FdtNode<'_, '_>) -> bool {
    ["arm,pl011", "arm,pl011-axi", "arm,sbsa-uart"]
        .iter()
        .any(|name| compatible(node, name))
}
fn collect_paths(node: FdtNode<'_, '_>, parent: &str, paths: &mut Vec<String>) {
    if node
        .property("status")
        .is_some_and(|p| !matches!(p.as_str(), Some("okay" | "ok")))
    {
        return;
    }
    let path = if node.name.is_empty() {
        String::from("/")
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), node.name)
    };
    paths.push(path.clone());
    for child in node.children() {
        collect_paths(child, &path, paths);
    }
}
fn stdout_path<'a>(tree: &Fdt<'a>) -> Option<&'a str> {
    if let Some(chosen) = tree.find_node("/chosen") {
        if let Some(p) = chosen
            .property("stdout-path")
            .or_else(|| chosen.property("linux,stdout-path"))
        {
            let selector = p.as_str()?.split(':').next()?;
            return if selector.starts_with('/') {
                Some(selector)
            } else {
                tree.aliases()?.resolve(selector)
            };
        }
    }
    let aliases = tree.aliases()?;
    ["console", "serial10", "uart10", "serial0", "uart0"]
        .iter()
        .find_map(|name| aliases.resolve(name))
}
fn parent_path(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &path[..index],
    }
}
fn u32be(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}
fn cells(bytes: &[u8], count: usize) -> Option<u64> {
    if count > 3 || bytes.len() != count.checked_mul(4)? {
        return None;
    }
    // PCI 三 cell 地址的第一个 cell 是空间属性，后两个组成总线地址。
    let skip = if count == 3 { 4 } else { 0 };
    Some(
        bytes[skip..]
            .chunks_exact(4)
            .fold(0, |value, cell| (value << 32) | u32be(cell).unwrap() as u64),
    )
}
/// 逐级应用父总线 ranges；完整寄存器范围都必须落在选中的窗口内。
fn reg(tree: &Fdt<'_>, path: &str) -> Result<(u64, u64), Error> {
    reg_at(tree, path, 0)
}
fn reg_at(tree: &Fdt<'_>, path: &str, index: usize) -> Result<(u64, u64), Error> {
    let parse = || {
        let node = tree.find_node(path)?;
        let mut bus_path = parent_path(path);
        let parent = tree.find_node(bus_path)?;
        let sizes = parent.cell_sizes();
        let raw = node.property("reg")?.value;
        let split = sizes.address_cells.checked_mul(4)?;
        let length = sizes.size_cells.checked_mul(4)?;
        let stride = split.checked_add(length)?;
        if stride == 0 || raw.len() % stride != 0 {
            return None;
        }
        let raw = raw.get(index.checked_mul(stride)?..)?;
        let mut address = cells(raw.get(..split)?, sizes.address_cells)?;
        let size = cells(
            raw.get(split..split.checked_add(length)?)?,
            sizes.size_cells,
        )?;
        if size == 0 {
            return None;
        }
        address.checked_add(size)?;
        while bus_path != "/" {
            let bus = tree.find_node(bus_path)?;
            let next_path = parent_path(bus_path);
            let parent = tree.find_node(next_path)?;
            // 当前平台简单总线允许缺省 1:1；有 ranges 则必须匹配。
            if let Some(ranges) = bus.property("ranges").filter(|p| !p.value.is_empty()) {
                let child = bus.cell_sizes();
                let parent = parent.cell_sizes();
                let ca = child.address_cells.checked_mul(4)?;
                let pa = parent.address_cells.checked_mul(4)?;
                let sz = child.size_cells.checked_mul(4)?;
                let stride = ca.checked_add(pa)?.checked_add(sz)?;
                if stride == 0 || ranges.value.len() % stride != 0 {
                    return None;
                }
                let mut translated = None;
                for entry in ranges.value.chunks_exact(stride) {
                    let start = cells(&entry[..ca], child.address_cells)?;
                    let target = cells(&entry[ca..ca + pa], parent.address_cells)?;
                    let span = cells(&entry[ca + pa..], child.size_cells)?;
                    if address >= start && address.checked_sub(start)?.checked_add(size)? <= span {
                        translated = Some(target.checked_add(address - start)?);
                        break;
                    }
                }
                address = translated?;
            }
            bus_path = next_path;
        }
        address.checked_add(size)?;
        Some((address, size))
    };
    parse().ok_or(Error::InvalidResource)
}
fn resource(tree: &Fdt<'_>, path: &str, rp1: bool) -> Result<DeviceResource, Error> {
    let (base, size) = reg(tree, path)?;
    let node = tree.find_node(path).ok_or(Error::InvalidFdt)?;
    let interrupts = node
        .property("interrupts")
        .ok_or(Error::UnsupportedInterrupt)?
        .value;
    let intid = if rp1 && interrupts.len() == 8 {
        // 当前 Pi5 固件沿用 RP1 local IRQ + 0x7f 的 GIC 路由；不能泛化到任意板卡。
        // 此处只是描述转换；触发方式与 INTID 归属仍由 EL1 平台授权表控制。
        u32be(interrupts).and_then(|irq| irq.checked_add(0x7f))
    } else if interrupts.len() >= 12 {
        let irq = u32be(&interrupts[4..]).ok_or(Error::UnsupportedInterrupt)?;
        match u32be(interrupts) {
            Some(0) => irq.checked_add(32),
            Some(1) => irq.checked_add(16),
            _ => None,
        }
    } else {
        None
    }
    .ok_or(Error::UnsupportedInterrupt)?;
    if intid < 32 {
        return Err(Error::UnsupportedInterrupt);
    }
    Ok(DeviceResource { base, size, intid })
}
/// 在 fdt 遍历前验证切片边界、字符串、嵌套和属性布局。
/// 损坏的固件 blob 应返回错误，不能通过裸指针越过只读映射。
fn validate(bytes: &[u8]) -> Result<(), Error> {
    let check = || {
        if bytes.len() < 40 || u32be(bytes)? != 0xd00d_feed {
            return None;
        }
        let total = u32be(&bytes[4..])? as usize;
        if total != bytes.len() {
            return None;
        }
        let range = |off, len| {
            let start = u32be(&bytes[off..])? as usize;
            let size = u32be(&bytes[len..])? as usize;
            if start < 40 {
                return None;
            }
            Some(start..start.checked_add(size).filter(|end| *end <= total)?)
        };
        let structure = range(8, 36)?;
        let strings = range(12, 32)?;
        if structure.start % 4 != 0
            || structure.start < strings.end && strings.start < structure.end
        {
            return None;
        }
        let names = bytes.get(strings)?;
        let data = bytes.get(structure)?;
        let mut offset = 0usize;
        let mut depth = 0usize;
        let mut seen_root = false;
        // 每层在第一个子节点之后不得再出现父节点属性。
        let mut children = [false; 64];
        while offset < data.len() {
            let token = u32be(data.get(offset..)?)?;
            offset = offset.checked_add(4)?;
            match token {
                1 => {
                    if depth == 0 && seen_root || depth == children.len() {
                        return None;
                    }
                    if depth > 0 {
                        children[depth - 1] = true;
                    }
                    let name_len = data.get(offset..)?.iter().position(|byte| *byte == 0)?;
                    core::str::from_utf8(data.get(offset..offset.checked_add(name_len)?)?).ok()?;
                    if depth == 0 && name_len != 0 {
                        return None;
                    }
                    seen_root = true;
                    children[depth] = false;
                    depth += 1;
                    offset = offset.checked_add(name_len)?.checked_add(4)? & !3;
                }
                2 => {
                    depth = depth.checked_sub(1)?;
                }
                3 => {
                    if depth == 0 || children[depth - 1] {
                        return None;
                    }
                    let length = u32be(data.get(offset..)?)? as usize;
                    let name = u32be(data.get(offset.checked_add(4)?..)?)? as usize;
                    let name = names.get(name..)?;
                    let end = name.iter().position(|byte| *byte == 0)?;
                    let property_name = core::str::from_utf8(&name[..end]).ok()?;
                    let value_start = offset.checked_add(8)?;
                    let value_end = value_start.checked_add(length)?;
                    let value = data.get(value_start..value_end)?;
                    // fdt 的 cell_sizes() 对过短属性会 panic，提前拒绝损坏的计数。
                    if matches!(
                        property_name,
                        "#address-cells" | "#size-cells" | "#interrupt-cells"
                    ) {
                        if value.len() != 4 || u32be(value)? > 3 {
                            return None;
                        }
                    }
                    if property_name == "compatible" {
                        if value.last() != Some(&0) {
                            return None;
                        }
                        core::str::from_utf8(value).ok()?;
                    }
                    offset = value_end.checked_add(3)? & !3;
                }
                4 => {}
                9 => {
                    return if seen_root && depth == 0 {
                        Some(())
                    } else {
                        None
                    }
                }
                _ => return None,
            }
            if offset > data.len() {
                return None;
            }
        }
        None
    };
    check().ok_or(Error::InvalidFdt)
}
