//! EL0 设备树查询。
//!
//! `fdt` crate 负责校验 FDT、遍历节点、解析属性和 alias。这里仅保留
//! 两类与当前外核 ABI 相关的逻辑：
//! 1. 沿父总线 `ranges` 把设备地址翻译成 CPU 物理地址；
//! 2. 把 GIC 的 `<type, hwirq, flags>` 转换为 INTID。

use fdt::{node::FdtNode, Fdt};

#[derive(Clone, Copy)]
pub struct RegRange {
    pub base: u64,
    pub size: u64,
}

#[derive(Clone, Copy)]
pub struct UartInfo {
    pub reg: RegRange,
    pub intid: u32,
}

#[derive(Clone, Copy)]
#[repr(u64)]
pub enum Error {
    NullDtb = 1,
    InvalidFdt = 2,
    MissingChosen = 3,
    MissingStdout = 4,
    InvalidStdout = 5,
    MissingAlias = 6,
    MissingUartNode = 7,
    DisabledUart = 8,
    UnsupportedUart = 9,
    MissingParent = 10,
    MissingReg = 11,
    InvalidReg = 12,
    AddressTranslation = 13,
    MissingInterrupt = 14,
    InvalidInterrupt = 15,
}

impl Error {
    pub const fn code(self) -> u64 {
        self as u64
    }
}

pub fn find_stdout_uart(dtb_va: u64) -> Result<UartInfo, Error> {
    if dtb_va == 0 {
        return Err(Error::NullDtb);
    }

    // SAFETY: EL1 已把完整 DTB 只读映射到 dtb_va。
    let tree = unsafe { Fdt::from_ptr(dtb_va as *const u8) }.map_err(|_| Error::InvalidFdt)?;

    // tree.chosen().stdout() 能解析 stdout-path 但不会去掉 "serial0:115200n8"
    // 里的波特率后缀。这里手动读 /chosen 的 stdout-path 属性值，用冒号分割得到
    // 选择器，再通过 aliases 或直接路径找到 UART 节点。
    let full_path = stdout_path(&tree)?;
    let node = tree.find_node(full_path).ok_or(Error::MissingUartNode)?;

    if node_is_disabled(node) {
        return Err(Error::DisabledUart);
    }
    if !node_is_pl011(node) {
        return Err(Error::UnsupportedUart);
    }

    let parent_path = parent_path(full_path);
    let parent = tree.find_node(parent_path).ok_or(Error::MissingParent)?;
    let parent_cells = parent.cell_sizes();
    let reg = node.property("reg").ok_or(Error::MissingReg)?.value;
    let (raw_base, size) = read_reg(reg, parent_cells.address_cells, parent_cells.size_cells)
        .ok_or(Error::InvalidReg)?;
    let base = translate_to_cpu(&tree, full_path, raw_base).ok_or(Error::AddressTranslation)?;
    let interrupts = node
        .property("interrupts")
        .ok_or(Error::MissingInterrupt)?
        .value;
    let intid = parse_gic_interrupt(interrupts).ok_or(Error::InvalidInterrupt)?;

    Ok(UartInfo {
        reg: RegRange { base, size },
        intid,
    })
}

/// 返回 `/chosen/stdout-path` 最终指向的完整节点路径。
///
/// Pi5 的值是 `serial10:115200n8`。冒号后是串口参数，不是 alias 名的一部分。
fn stdout_path<'a>(tree: &Fdt<'a>) -> Result<&'a str, Error> {
    let chosen = tree.find_node("/chosen").ok_or(Error::MissingChosen)?;

    // 固件通常通过标准属性指定控制台。Pi5 EDK2 生成的运行时 DTB
    // 可能删除该属性，因此下面还会按常用 alias 名回退。
    let property = chosen
        .property("stdout-path")
        .or_else(|| chosen.property("linux,stdout-path"));

    if let Some(property) = property {
        let value = core::str::from_utf8(property.value)
            .map_err(|_| Error::InvalidStdout)?
            .trim_end_matches('\0');
        let selector = value.split(':').next().ok_or(Error::InvalidStdout)?;
        if selector.is_empty() {
            return Err(Error::InvalidStdout);
        }

        if selector.starts_with('/') {
            return Ok(selector);
        }
        return tree
            .aliases()
            .and_then(|aliases| aliases.resolve(selector))
            .ok_or(Error::MissingAlias);
    }

    let aliases = tree.aliases().ok_or(Error::MissingAlias)?;
    for name in ["console", "serial10", "uart10", "serial0", "uart0"] {
        if let Some(path) = aliases.resolve(name) {
            return Ok(path);
        }
    }

    Err(Error::MissingStdout)
}

fn node_is_pl011(node: FdtNode<'_, '_>) -> bool {
    node.compatible()
        .map(|compatible| {
            compatible.all().any(|name| {
                name == "arm,pl011" || name == "arm,pl011-axi" || name == "arm,sbsa-uart"
            })
        })
        .unwrap_or(false)
}

fn node_is_disabled(node: FdtNode<'_, '_>) -> bool {
    node.property("status")
        .and_then(|property| core::str::from_utf8(property.value).ok())
        .map(|status| status.trim_end_matches('\0') == "disabled")
        .unwrap_or(false)
}

fn read_reg(data: &[u8], address_cells: usize, size_cells: usize) -> Option<(u64, u64)> {
    let address_bytes = address_cells.checked_mul(4)?;
    let size_bytes = size_cells.checked_mul(4)?;
    if data.len() < address_bytes.checked_add(size_bytes)? {
        return None;
    }

    let address = read_cells(&data[..address_bytes], address_cells)?;
    let size = read_cells(&data[address_bytes..address_bytes + size_bytes], size_cells)?;
    if size == 0 {
        return None;
    }
    Some((address, size))
}

/// 沿 UART 所在节点的父总线逐级应用 `ranges`。
fn translate_to_cpu(tree: &Fdt<'_>, device_path: &str, mut address: u64) -> Option<u64> {
    let mut bus_path = parent_path(device_path);

    while bus_path != "/" {
        let bus = tree.find_node(bus_path)?;
        let next_path = parent_path(bus_path);
        let parent = tree.find_node(next_path)?;
        address = translate_one_bus(bus, parent, address)?;
        bus_path = next_path;
    }

    Some(address)
}

fn translate_one_bus(bus: FdtNode<'_, '_>, parent: FdtNode<'_, '_>, address: u64) -> Option<u64> {
    let Some(ranges) = bus.property("ranges") else {
        // 与现有 bring-up 语义一致：未声明 ranges 的简单总线按 1:1 处理。
        return Some(address);
    };
    if ranges.value.is_empty() {
        return Some(address);
    }

    let child_cells = bus.cell_sizes();
    let parent_cells = parent.cell_sizes();
    let entry_cells =
        child_cells.address_cells + parent_cells.address_cells + child_cells.size_cells;
    let entry_bytes = entry_cells.checked_mul(4)?;
    if entry_bytes == 0 || ranges.value.len() % entry_bytes != 0 {
        return None;
    }

    for entry in ranges.value.chunks_exact(entry_bytes) {
        let child_bytes = child_cells.address_cells * 4;
        let parent_bytes = parent_cells.address_cells * 4;
        let child = read_cells(&entry[..child_bytes], child_cells.address_cells)?;
        let parent_address = read_cells(
            &entry[child_bytes..child_bytes + parent_bytes],
            parent_cells.address_cells,
        )?;
        let size = read_cells(&entry[child_bytes + parent_bytes..], child_cells.size_cells)?;

        if size != 0 && address >= child && address - child < size {
            return parent_address.checked_add(address - child);
        }
    }

    None
}

/// 设备树 cell 是大端 u32。PCI 地址常用 3 cells，其中第一个 cell 是
/// space/type flags；当前物理地址计算取后两个 cells 组成 64 位地址。
fn read_cells(bytes: &[u8], cells: usize) -> Option<u64> {
    if cells == 0 {
        return Some(0);
    }
    if bytes.len() != cells.checked_mul(4)? || cells > 3 {
        return None;
    }

    let first = if cells == 3 { 1 } else { 0 };
    let mut value = 0u64;
    let mut index = first;
    while index < cells {
        let offset = index * 4;
        let cell = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]);
        value = (value << 32) | cell as u64;
        index += 1;
    }
    Some(value)
}

fn parse_gic_interrupt(data: &[u8]) -> Option<u32> {
    if data.len() < 12 {
        return None;
    }
    let interrupt_type = read_be_u32(&data[0..4])?;
    let hardware_irq = read_be_u32(&data[4..8])?;

    match interrupt_type {
        0 => hardware_irq.checked_add(32), // SPI
        1 => hardware_irq.checked_add(16), // PPI
        _ => None,
    }
}

fn read_be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

fn parent_path(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &path[..index],
    }
}
