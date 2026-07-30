//! dtb.rs —— 最小设备树 (DeviceTree / FDT) 解析器
//!
//! 从物理地址读 FDT (Flattened DeviceTree), 遍历每个设备节点,
//! 提取 reg (寄存器地址范围) 和 interrupts (中断号) 信息。
//!
//! 设备树是什么: 一个二进制的"硬件清单", 描述机器上所有设备的
//! 物理地址范围、中断、时钟、电源等信息。Linux 开机也先读它。
//!
//! 外核只需要 reg 和 interrupts, 其他属性跳过。
//! reg 信息喂给 protect.rs (建 MMIO 保护表),
//! interrupts 留着以后给中断路由表。

use crate::{protect, uart};

// ── FDT 格式常量 ──
// FDT header 开头是一个 32 位 magic number
const FDT_MAGIC: u32 = 0xd00dfeed; // FDT 魔数 (大端)
                                   // 遍历 structure block 时遇到的 token
const FDT_BEGIN_NODE: u32 = 1; // 开始一个设备节点
const FDT_END_NODE: u32 = 2; // 结束一个设备节点
const FDT_PROP: u32 = 3; // 属性 (key-value 对)
const FDT_NOP: u32 = 4; // 空指令 (跳过)
const FDT_END: u32 = 9; // 整棵树结束

#[derive(Clone, Copy)]
pub struct RegRange {
    pub base: u64,
    pub size: u64,
}

#[derive(Clone, Copy)]
pub struct IrqSpec {
    pub intid: u32,
    pub flags: u32,
}

/// 读取 DTB header 里的 totalsize。
pub fn total_size(dtb_paddr: u64) -> Option<u64> {
    if dtb_paddr == 0 {
        return None;
    }
    let ptr = dtb_paddr as *const u32;
    let magic = unsafe { read_be(ptr.add(0)) };
    if magic != FDT_MAGIC {
        return None;
    }
    let total = unsafe { read_be(ptr.add(1)) } as u64;
    let off_struct = unsafe { read_be(ptr.add(2)) } as u64;
    let off_strings = unsafe { read_be(ptr.add(3)) } as u64;
    let size_strings = unsafe { read_be(ptr.add(8)) } as u64;
    let size_struct = unsafe { read_be(ptr.add(9)) } as u64;
    if total < 40
        || off_struct < 40
        || off_strings < 40
        || off_struct.checked_add(size_struct)? > total
        || off_strings.checked_add(size_strings)? > total
    {
        return None;
    }
    Some(total)
}

/// 静默查找第一个 compatible 匹配的节点, 返回它的第一个 reg 范围。
///
/// 这个函数不打印, 用在 UART 初始化前。
/// 返回值是已经经过父节点 ranges 翻译后的 CPU 物理地址。
pub fn find_pl011_reg(dtb_paddr: u64) -> Option<RegRange> {
    if dtb_paddr == 0 {
        return None;
    }

    find_uart_in_dtb(dtb_paddr)
}

/// 从 DTB 中找 GICv2 中断控制器的 Distributor 和 CPU Interface 物理地址。
/// 返回 (gicd_pa, gicc_pa)。地址已经过 ranges 翻译。
pub fn find_gic_regs(dtb_paddr: u64) -> Option<(u64, u64)> {
    if dtb_paddr == 0 {
        return None;
    }

    let ptr = dtb_paddr as *const u32;
    let magic = unsafe { read_be(ptr.add(0)) };
    if magic != FDT_MAGIC {
        return None;
    }

    let off_struct = unsafe { read_be(ptr.add(2)) };
    let off_strings = unsafe { read_be(ptr.add(3)) };
    let sp = (dtb_paddr + off_struct as u64) as *const u32;
    let ss = (dtb_paddr + off_strings as u64) as *const u8;

    let mut bus_stack = [BusInfo::default(); MAX_DEPTH];
    let mut depth = 0usize;
    // 子节点会重置 node_is_gic/gic_reg_count，所以用栈保存父节点状态。
    let mut gic_flag_stack: [bool; MAX_DEPTH] = [false; MAX_DEPTH];
    let mut gic_reg_stack: [[Option<RegRange>; 4]; MAX_DEPTH] = [[None; 4]; MAX_DEPTH];
    let mut gic_count_stack: [usize; MAX_DEPTH] = [0; MAX_DEPTH];
    let mut node_is_gic = false;
    let mut gic_regs: [Option<RegRange>; 4] = [None; 4];
    let mut gic_reg_count: usize = 0;
    let mut i = 0usize;

    loop {
        let token = unsafe { read_be(sp.add(i)) };
        if token == FDT_END {
            break;
        }
        if token == FDT_BEGIN_NODE {
            i += 1;
            let name_ptr = unsafe { sp.add(i) as *const u8 };
            let mut name_len = 0usize;
            while unsafe { *name_ptr.add(name_len) } != 0 {
                name_len += 1;
            }
            if depth + 1 >= MAX_DEPTH {
                return None;
            }
            // 保存父节点状态
            gic_flag_stack[depth] = node_is_gic;
            gic_reg_stack[depth] = gic_regs;
            gic_count_stack[depth] = gic_reg_count;
            if depth == 0 {
                bus_stack[0] = BusInfo::default();
            } else {
                bus_stack[depth] = BusInfo::default();
            }
            depth += 1;
            node_is_gic = false;
            gic_regs = [None; 4];
            gic_reg_count = 0;
            i += (name_len + 4) / 4;
        } else if token == FDT_END_NODE {
            if node_is_gic && gic_reg_count >= 2 {
                let gicd = gic_regs[0].unwrap();
                let gicc = gic_regs[1].unwrap();
                return Some((gicd.base, gicc.base));
            }
            i += 1;
            if depth == 0 {
                return None;
            }
            depth -= 1;
            // 恢复父节点状态
            node_is_gic = gic_flag_stack[depth];
            gic_regs = gic_reg_stack[depth];
            gic_reg_count = gic_count_stack[depth];
        } else if token == FDT_PROP {
            i += 1;
            let len = unsafe { read_be(sp.add(i)) };
            i += 1;
            let nameoff = unsafe { read_be(sp.add(i)) };
            i += 1;
            let prop_name_ptr = unsafe { ss.add(nameoff as usize) };

            if cstr_eq_address_cells(prop_name_ptr) && len == 4 {
                bus_stack[depth - 1].addr_cells = unsafe { read_be(sp.add(i)) };
                reparse_current_ranges(sp, &mut bus_stack, depth);
            } else if cstr_eq_size_cells(prop_name_ptr) && len == 4 {
                bus_stack[depth - 1].size_cells = unsafe { read_be(sp.add(i)) };
                reparse_current_ranges(sp, &mut bus_stack, depth);
            } else if cstr_eq_ranges(prop_name_ptr) {
                bus_stack[depth - 1].ranges_word_index = i;
                bus_stack[depth - 1].ranges_len = len;
                parse_ranges(sp, i, len, &mut bus_stack, depth);
            } else if cstr_eq_compatible(prop_name_ptr) {
                node_is_gic = prop_contains_gic(unsafe { sp.add(i) } as *const u8, len as usize);
            } else if cstr_eq_reg(prop_name_ptr) && gic_reg_count < 4 && depth >= 2 {
                // GIC reg 属性包含多个条目 (GICD, GICC, GICH, GICV)。
                // 子节点的 reg 格式由其父节点的 #address-cells / #size-cells 定义，
                // 因此用 bus_stack[depth - 2] (父) 而非 bus_stack[depth - 1] (自身)。
                let parent = bus_stack[depth - 2];
                let entry_words = (parent.addr_cells + parent.size_cells) as usize;
                if entry_words > 0 {
                    let total_entries = (len / 4) as usize / entry_words;
                    let mut e = 0usize;
                    while e < total_entries && gic_reg_count < 4 {
                        let off = i + e * entry_words;
                        if let Some(r) = read_first_translated_reg(
                            sp,
                            off,
                            (entry_words * 4) as u32,
                            &bus_stack,
                            depth,
                        ) {
                            gic_regs[gic_reg_count] = Some(r);
                            gic_reg_count += 1;
                        }
                        e += 1;
                    }
                }
            }

            i += ((len + 3) / 4) as usize;
        } else if token == FDT_NOP {
            i += 1;
        } else {
            return None;
        }
    }
    None
}

/// 从 DTB 中找到 stdout UART 的 GIC 中断号 (INTID)。
///
/// 解析 interrupts 属性, 格式为 GIC 标准 3-cell:
///   <type hw_num trigger>
/// INTID = hw_num + 32 (SPI) 或 hw_num + 16 (PPI)。
pub fn find_uart_irq(dtb_paddr: u64) -> Option<IrqSpec> {
    if dtb_paddr == 0 {
        return None;
    }

    let ptr = dtb_paddr as *const u32;
    let magic = unsafe { read_be(ptr.add(0)) };
    if magic != FDT_MAGIC {
        return None;
    }

    let off_struct = unsafe { read_be(ptr.add(2)) };
    let off_strings = unsafe { read_be(ptr.add(3)) };
    let sp = (dtb_paddr + off_struct as u64) as *const u32;
    let ss = (dtb_paddr + off_strings as u64) as *const u8;

    let target_path = find_stdout_target_path(sp, ss);

    let mut bus_stack = [BusInfo::default(); MAX_DEPTH];
    let mut depth = 0usize;
    let mut path = [0u8; MAX_PATH];
    let mut path_len_stack = [0usize; MAX_DEPTH];
    let mut path_len = 0usize;
    let mut node_compatible = false;
    let mut node_disabled = false;
    let mut node_interrupts: Option<IrqSpec> = None;
    let mut fallback_irq: Option<IrqSpec> = None;
    let mut i = 0usize;

    loop {
        let token = unsafe { read_be(sp.add(i)) };
        if token == FDT_END {
            return fallback_irq;
        }

        if token == FDT_BEGIN_NODE {
            i += 1;
            let name_ptr = unsafe { sp.add(i) as *const u8 };
            let mut name_len = 0usize;
            while unsafe { *name_ptr.add(name_len) } != 0 {
                name_len += 1;
            }
            if depth + 1 >= MAX_DEPTH {
                return None;
            }
            path_len_stack[depth] = path_len;
            append_node_to_path(&mut path, &mut path_len, name_ptr, name_len);
            if depth == 0 {
                bus_stack[0] = BusInfo::default();
            } else {
                bus_stack[depth] = BusInfo::default();
            }
            depth += 1;
            node_compatible = false;
            node_disabled = false;
            node_interrupts = None;
            i += (name_len + 4) / 4;
        } else if token == FDT_END_NODE {
            if node_compatible && !node_disabled && path_matches(&path, path_len, &target_path) {
                if let Some(irq) = node_interrupts {
                    return Some(irq);
                }
            }
            if node_compatible && !node_disabled && fallback_irq.is_none() {
                fallback_irq = node_interrupts;
            }
            i += 1;
            if depth == 0 {
                return None;
            }
            depth -= 1;
            path_len = path_len_stack[depth];
            node_compatible = false;
            node_disabled = false;
            node_interrupts = None;
        } else if token == FDT_PROP {
            i += 1;
            let len = unsafe { read_be(sp.add(i)) };
            i += 1;
            let nameoff = unsafe { read_be(sp.add(i)) };
            i += 1;
            let prop_name_ptr = unsafe { ss.add(nameoff as usize) };

            if cstr_eq_address_cells(prop_name_ptr) && len == 4 {
                bus_stack[depth - 1].addr_cells = unsafe { read_be(sp.add(i)) };
                reparse_current_ranges(sp, &mut bus_stack, depth);
            } else if cstr_eq_size_cells(prop_name_ptr) && len == 4 {
                bus_stack[depth - 1].size_cells = unsafe { read_be(sp.add(i)) };
                reparse_current_ranges(sp, &mut bus_stack, depth);
            } else if cstr_eq_ranges(prop_name_ptr) {
                bus_stack[depth - 1].ranges_word_index = i;
                bus_stack[depth - 1].ranges_len = len;
                parse_ranges(sp, i, len, &mut bus_stack, depth);
            } else if cstr_eq_status(prop_name_ptr) {
                node_disabled = prop_is_disabled(unsafe { sp.add(i) } as *const u8, len as usize);
            } else if cstr_eq_compatible(prop_name_ptr) {
                node_compatible =
                    prop_contains_pl011(unsafe { sp.add(i) } as *const u8, len as usize);
            } else if cstr_eq_interrupts(prop_name_ptr) && node_interrupts.is_none() {
                node_interrupts = parse_gic_interrupts(sp, i, len);
            }

            i += ((len + 3) / 4) as usize;
        } else if token == FDT_NOP {
            i += 1;
        } else {
            return None;
        }
    }
}

pub fn find_uart_intid(dtb_paddr: u64) -> Option<u32> {
    find_uart_irq(dtb_paddr).map(|irq| irq.intid)
}

/// 找到 ARM Generic Timer 的 non-secure physical timer PPI。
///
/// `arm,armv8-timer` 的 interrupts 顺序由 binding 固定为 secure physical、
/// non-secure physical、virtual、hypervisor。EL1 使用第二项，对应
/// CNTP_* 寄存器；QEMU virt 和 Pi5 上通常都是 GIC INTID 30。
pub fn find_nonsecure_physical_timer_irq(dtb_paddr: u64) -> Option<IrqSpec> {
    if dtb_paddr == 0 {
        return None;
    }
    let ptr = dtb_paddr as *const u32;
    if unsafe { read_be(ptr) } != FDT_MAGIC {
        return None;
    }
    let off_struct = unsafe { read_be(ptr.add(2)) };
    let off_strings = unsafe { read_be(ptr.add(3)) };
    let sp = (dtb_paddr + off_struct as u64) as *const u32;
    let ss = (dtb_paddr + off_strings as u64) as *const u8;
    let mut compatible = [false; MAX_DEPTH];
    let mut disabled = [false; MAX_DEPTH];
    let mut irq = [None; MAX_DEPTH];
    let mut depth = 0usize;
    let mut i = 0usize;

    loop {
        let token = unsafe { read_be(sp.add(i)) };
        match token {
            FDT_BEGIN_NODE => {
                i += 1;
                let name = unsafe { sp.add(i) as *const u8 };
                let mut len = 0usize;
                while unsafe { *name.add(len) } != 0 {
                    len += 1;
                }
                if depth >= MAX_DEPTH {
                    return None;
                }
                compatible[depth] = false;
                disabled[depth] = false;
                irq[depth] = None;
                depth += 1;
                i += (len + 4) / 4;
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return None;
                }
                let node = depth - 1;
                if compatible[node] && !disabled[node] {
                    if let Some(spec) = irq[node] {
                        return Some(spec);
                    }
                }
                depth -= 1;
                i += 1;
            }
            FDT_PROP => {
                if depth == 0 {
                    return None;
                }
                i += 1;
                let len = unsafe { read_be(sp.add(i)) };
                i += 1;
                let nameoff = unsafe { read_be(sp.add(i)) };
                i += 1;
                let name = unsafe { ss.add(nameoff as usize) };
                let node = depth - 1;
                if cstr_eq_compatible(name) {
                    compatible[node] =
                        prop_contains_armv8_timer(unsafe { sp.add(i) } as *const u8, len as usize);
                } else if cstr_eq_status(name) {
                    disabled[node] =
                        prop_is_disabled(unsafe { sp.add(i) } as *const u8, len as usize);
                } else if cstr_eq_interrupts(name) && len >= 24 {
                    // 跳过第一组三个cell，解析第二组non-secure physical PPI。
                    irq[node] = parse_gic_interrupts(sp, i + 3, len - 12);
                }
                i += ((len + 3) / 4) as usize;
            }
            FDT_NOP => i += 1,
            FDT_END => return None,
            _ => return None,
        }
    }
}

/// 查找 Pi5 RP1 下的 DWC3/xHCI 控制器。
///
/// RP1 的 USB 节点使用两 cell 的本地中断号，而不是 GIC 的三 cell
/// specifier。RP1 中断窗口在当前 Pi5 固件中从 GIC SPI 0x7f 开始，
/// 因此 USB local IRQ 0x1f/0x24 分别转换为 GIC INTID 0x9e/0xa3。
/// 这个转换只用于 compatible="snps,dwc3" 的 RP1 节点；QEMU xHCI
/// 仍然由 EL0 PCI 枚举并通过 PCI INTx 路由取得 INTID。
pub fn find_rp1_xhci(dtb_paddr: u64) -> Option<(RegRange, IrqSpec)> {
    if dtb_paddr == 0 {
        return None;
    }
    let ptr = dtb_paddr as *const u32;
    if unsafe { read_be(ptr) } != FDT_MAGIC {
        return None;
    }
    let off_struct = unsafe { read_be(ptr.add(2)) };
    let off_strings = unsafe { read_be(ptr.add(3)) };
    let sp = (dtb_paddr + off_struct as u64) as *const u32;
    let ss = (dtb_paddr + off_strings as u64) as *const u8;

    let mut bus_stack = [BusInfo::default(); MAX_DEPTH];
    let mut path_len_stack = [0usize; MAX_DEPTH];
    let mut depth = 0usize;
    let mut path = [0u8; MAX_PATH];
    let mut path_len = 0usize;
    let mut node_is_dwc3 = false;
    let mut node_reg = None;
    let mut node_irq = None;
    let mut i = 0usize;

    loop {
        let token = unsafe { read_be(sp.add(i)) };
        if token == FDT_END {
            return None;
        }
        if token == FDT_BEGIN_NODE {
            i += 1;
            let name = unsafe { sp.add(i) as *const u8 };
            let mut name_len = 0usize;
            while unsafe { *name.add(name_len) } != 0 {
                name_len += 1;
            }
            if depth >= MAX_DEPTH {
                return None;
            }
            path_len_stack[depth] = path_len;
            append_node_to_path(&mut path, &mut path_len, name, name_len);
            bus_stack[depth] = BusInfo::default();
            depth += 1;
            node_is_dwc3 = false;
            node_reg = None;
            node_irq = None;
            i += (name_len + 4) / 4;
        } else if token == FDT_END_NODE {
            if node_is_dwc3 {
                if let (Some(reg), Some(irq)) = (node_reg, node_irq) {
                    return Some((reg, irq));
                }
            }
            i += 1;
            if depth == 0 {
                return None;
            }
            depth -= 1;
            path_len = path_len_stack[depth];
            node_is_dwc3 = false;
            node_reg = None;
            node_irq = None;
        } else if token == FDT_PROP {
            i += 1;
            let len = unsafe { read_be(sp.add(i)) };
            i += 1;
            let nameoff = unsafe { read_be(sp.add(i)) };
            i += 1;
            let pname = unsafe { ss.add(nameoff as usize) };
            let value = unsafe { sp.add(i) } as *const u8;

            if cstr_eq_address_cells(pname) && len == 4 {
                bus_stack[depth - 1].addr_cells = unsafe { read_be(sp.add(i)) };
                reparse_current_ranges(sp, &mut bus_stack, depth);
            } else if cstr_eq_size_cells(pname) && len == 4 {
                bus_stack[depth - 1].size_cells = unsafe { read_be(sp.add(i)) };
                reparse_current_ranges(sp, &mut bus_stack, depth);
            } else if cstr_eq_ranges(pname) {
                bus_stack[depth - 1].ranges_word_index = i;
                bus_stack[depth - 1].ranges_len = len;
                parse_ranges(sp, i, len, &mut bus_stack, depth);
            } else if cstr_eq_compatible(pname) {
                node_is_dwc3 = prop_contains_dwc3(value, len as usize);
            } else if cstr_eq_reg(pname) && node_reg.is_none() {
                node_reg = read_first_translated_reg(sp, i, len, &bus_stack, depth);
            } else if cstr_eq_interrupts(pname) && node_irq.is_none() {
                node_irq = parse_rp1_interrupt(sp, i, len);
            }
            i += ((len + 3) / 4) as usize;
        } else if token == FDT_NOP {
            i += 1;
        } else {
            return None;
        }
    }
}

const MAX_DEPTH: usize = 24;
const MAX_RANGES: usize = 8;
const MAX_PATH: usize = 192;

#[derive(Clone, Copy)]
struct RangeMap {
    child: u64,
    parent: u64,
    size: u64,
}

#[derive(Clone, Copy)]
struct BusInfo {
    addr_cells: u32,
    size_cells: u32,
    ranges: [RangeMap; MAX_RANGES],
    range_count: usize,
    ranges_seen: bool,
    ranges_word_index: usize,
    ranges_len: u32,
}

impl BusInfo {
    const fn default() -> Self {
        Self {
            addr_cells: 2,
            size_cells: 1,
            ranges: [RangeMap {
                child: 0,
                parent: 0,
                size: 0,
            }; MAX_RANGES],
            range_count: 0,
            ranges_seen: false,
            ranges_word_index: 0,
            ranges_len: 0,
        }
    }
}

fn find_uart_in_dtb(dtb_paddr: u64) -> Option<RegRange> {
    let ptr = dtb_paddr as *const u32;
    let magic = unsafe { read_be(ptr.add(0)) };
    if magic != FDT_MAGIC {
        return None;
    }

    let off_struct = unsafe { read_be(ptr.add(2)) };
    let off_strings = unsafe { read_be(ptr.add(3)) };
    let sp = (dtb_paddr + off_struct as u64) as *const u32;
    let ss = (dtb_paddr + off_strings as u64) as *const u8;

    let target_path = find_stdout_target_path(sp, ss);
    let mut bus_stack = [BusInfo::default(); MAX_DEPTH];
    let mut depth = 0usize;
    let mut path = [0u8; MAX_PATH];
    let mut path_len_stack = [0usize; MAX_DEPTH];
    let mut path_len = 0usize;

    let mut node_compatible = false;
    let mut node_disabled = false;
    let mut node_reg: Option<RegRange> = None;
    let mut fallback_uart_reg: Option<RegRange> = None;
    let mut i = 0usize;

    loop {
        let token = unsafe { read_be(sp.add(i)) };
        if token == FDT_END {
            if node_compatible && !node_disabled && path_matches(&path, path_len, &target_path) {
                if let Some(reg) = node_reg {
                    return Some(reg);
                }
            }
            return fallback_uart_reg;
        }

        if token == FDT_BEGIN_NODE {
            i += 1;
            let name_ptr = unsafe { sp.add(i) as *const u8 };
            let mut name_len = 0usize;
            while unsafe { *name_ptr.add(name_len) } != 0 {
                name_len += 1;
            }

            if depth + 1 >= MAX_DEPTH {
                return None;
            }
            path_len_stack[depth] = path_len;
            append_node_to_path(&mut path, &mut path_len, name_ptr, name_len);

            if depth == 0 {
                bus_stack[0] = BusInfo::default();
            } else {
                bus_stack[depth] = BusInfo::default();
            }
            depth += 1;

            node_compatible = false;
            node_disabled = false;
            node_reg = None;
            i += (name_len + 4) / 4;
        } else if token == FDT_END_NODE {
            if node_compatible && !node_disabled && path_matches(&path, path_len, &target_path) {
                if let Some(reg) = node_reg {
                    return Some(reg);
                }
            }
            if node_compatible && !node_disabled && fallback_uart_reg.is_none() {
                fallback_uart_reg = node_reg;
            }
            i += 1;
            if depth == 0 {
                return None;
            }
            depth -= 1;
            path_len = path_len_stack[depth];
            node_compatible = false;
            node_reg = None;
            node_disabled = false;
        } else if token == FDT_PROP {
            i += 1;
            let len = unsafe { read_be(sp.add(i)) };
            i += 1;
            let nameoff = unsafe { read_be(sp.add(i)) };
            i += 1;

            let prop_name_ptr = unsafe { ss.add(nameoff as usize) };

            if cstr_eq_address_cells(prop_name_ptr) && len == 4 {
                bus_stack[depth - 1].addr_cells = unsafe { read_be(sp.add(i)) };
                reparse_current_ranges(sp, &mut bus_stack, depth);
            } else if cstr_eq_size_cells(prop_name_ptr) && len == 4 {
                bus_stack[depth - 1].size_cells = unsafe { read_be(sp.add(i)) };
                reparse_current_ranges(sp, &mut bus_stack, depth);
            } else if cstr_eq_ranges(prop_name_ptr) {
                bus_stack[depth - 1].ranges_word_index = i;
                bus_stack[depth - 1].ranges_len = len;
                parse_ranges(sp, i, len, &mut bus_stack, depth);
            } else if cstr_eq_status(prop_name_ptr) {
                node_disabled = prop_is_disabled(unsafe { sp.add(i) } as *const u8, len as usize);
            } else if cstr_eq_compatible(prop_name_ptr) {
                node_compatible =
                    prop_contains_pl011(unsafe { sp.add(i) } as *const u8, len as usize);
            } else if cstr_eq_reg(prop_name_ptr) && node_reg.is_none() {
                node_reg = read_first_translated_reg(sp, i, len, &bus_stack, depth);
            }

            i += ((len + 3) / 4) as usize;
        } else if token == FDT_NOP {
            i += 1;
        } else {
            return None;
        }
    }
}

/// 从物理地址 dtb_paddr 解析设备树, 打印每个节点的 reg 信息
/// dtb_paddr 来自 BootInfo.dtb (UEFI ConfigTable 里的 FDT_GUID 条目)
pub fn parse(dtb_paddr: u64) {
    if dtb_paddr == 0 {
        uart::puts("[dtb] DTB not found (paddr=0), skipping\r\n");
        return;
    }

    // 把物理地址转成指针, 直接读内存
    let ptr = dtb_paddr as *const u32;

    // ── 读 FDT header (前 40 字节) ──
    let magic = unsafe { read_be(ptr.add(0)) };
    if magic != FDT_MAGIC {
        uart::puts("[dtb] bad magic=");
        uart::hex(magic as u64);
        uart::puts("\r\n");
        return;
    }

    let totalsize = unsafe { read_be(ptr.add(1)) }; // DTB 总字节数
    let off_struct = unsafe { read_be(ptr.add(2)) }; // structure block 偏移
    let off_strings = unsafe { read_be(ptr.add(3)) }; // strings block 偏移

    uart::puts("[dtb] FDT found, size=");
    uart::hex(totalsize as u64);
    uart::puts(" struct_off=");
    uart::hex(off_struct as u64);
    uart::puts(" strings_off=");
    uart::hex(off_strings as u64);
    uart::puts("\r\n");

    // ── 遍历 structure block ──
    // sp = structure block 的基址
    // ss = strings block 的基址
    let sp = (dtb_paddr + off_struct as u64) as *const u32;
    let ss = (dtb_paddr + off_strings as u64) as *const u8;
    let mut addr_cells: u32 = 2; // 地址单元格数: 默认 2 = 64 位地址
    let mut size_cells: u32 = 2; // 大小单元格数: 默认 2 = 64 位大小
    let mut current_is_memory = false;

    let mut i = 0usize; // 当前指向 structure block 的第几个 u32
    loop {
        let token = unsafe { read_be(sp.add(i)) };
        if token == FDT_END {
            break; // 整棵树结束
        }
        if token == FDT_BEGIN_NODE {
            i += 1;
            // 读节点名 (null 结尾, 对齐到 4 字节)
            let name_ptr = unsafe { sp.add(i) as *const u8 };
            let mut name_len = 0usize;
            while unsafe { *name_ptr.add(name_len) } != 0 {
                name_len += 1;
            }
            let name = unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(name_ptr, name_len))
            };
            current_is_memory = name == "memory" || name.starts_with("memory@");
            uart::puts("[dtb] node: ");
            uart::puts(name);
            uart::puts("\r\n");
            // 跳过节点名字段 (向上取整到 4 字节对齐)
            i += (name_len + 4) / 4;
        } else if token == FDT_END_NODE {
            i += 1;
            // 退出节点, 恢复默认地址/大小格数
            addr_cells = 2;
            size_cells = 2;
            current_is_memory = false;
        } else if token == FDT_PROP {
            i += 1;
            let len = unsafe { read_be(sp.add(i)) }; // 属性值的字节数
            i += 1;
            let nameoff = unsafe { read_be(sp.add(i)) }; // 属性名在 strings block 的偏移
            i += 1;
            // 读属性名
            let prop_name_ptr = unsafe { ss.add(nameoff as usize) };
            let mut pnl = 0usize;
            while unsafe { *prop_name_ptr.add(pnl) } != 0 {
                pnl += 1;
            }
            let pname = unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(prop_name_ptr, pnl))
            };

            // ═══ 只抓两个关键属性 ═══
            // #address-cells / #size-cells: 父节点指定子节点的地址/大小格式
            if pname == "#address-cells" && len == 4 {
                addr_cells = unsafe { read_be(sp.add(i)) };
            } else if pname == "#size-cells" && len == 4 {
                size_cells = unsafe { read_be(sp.add(i)) };
            } else if pname == "reg" {
                // reg = 寄存器地址范围列表
                // 每个条目 = addr_cells × 4B + size_cells × 4B
                let entry_words = (addr_cells + size_cells) as usize;
                let entries = (len / 4) as usize / entry_words;
                for e in 0..entries {
                    let base_offset: usize = i + e * entry_words;
                    let base: u64 = if addr_cells == 2 {
                        ((unsafe { read_be(sp.add(base_offset)) }) as u64) << 32
                            | (unsafe { read_be(sp.add(base_offset + 1)) }) as u64
                    } else {
                        (unsafe { read_be(sp.add(base_offset)) }) as u64
                    };
                    let size: u64 = if size_cells == 2 {
                        ((unsafe { read_be(sp.add(base_offset + addr_cells as usize)) }) as u64)
                            << 32
                            | (unsafe { read_be(sp.add(base_offset + addr_cells as usize + 1)) })
                                as u64
                    } else {
                        (unsafe { read_be(sp.add(base_offset + addr_cells as usize)) }) as u64
                    };
                    uart::puts("  reg[");
                    uart::hex(e as u64);
                    uart::puts("]: base=");
                    uart::hex(base);
                    uart::puts(" size=");
                    uart::hex(size);
                    uart::puts("\r\n");
                    if !current_is_memory && size != 0 {
                        protect::register(base, size);
                    }
                }
            }

            // 跳过属性值 (对齐到 4 字节)
            let data_words = (len + 3) / 4;
            i += data_words as usize;
        } else if token == FDT_NOP {
            i += 1; // 空操作, 跳过
        } else {
            uart::puts("[dtb] unknown token=");
            uart::hex(token as u64);
            uart::puts("\r\n");
            break;
        }
    }
}

/// 读一个 32 位大端整数 (FDT 所有多字节值都是大端)
unsafe fn read_be(p: *const u32) -> u32 {
    u32::from_be(unsafe { core::ptr::read_volatile(p) })
}

fn read_first_translated_reg(
    sp: *const u32,
    i: usize,
    len: u32,
    bus_stack: &[BusInfo; MAX_DEPTH],
    depth: usize,
) -> Option<RegRange> {
    if depth < 2 {
        return None;
    }

    let parent = bus_stack[depth - 2];
    let entry_words = parent.addr_cells + parent.size_cells;
    if parent.addr_cells == 0 || parent.size_cells == 0 || entry_words == 0 || len < entry_words * 4
    {
        return None;
    }

    let raw_base = read_addr_cells(sp, i, parent.addr_cells);
    let size = read_size_cells(sp, i + parent.addr_cells as usize, parent.size_cells);
    if size == 0 {
        return None;
    }

    translate_to_cpu(raw_base, bus_stack, depth - 2).map(|base| RegRange { base, size })
}

fn translate_to_cpu(
    mut addr: u64,
    bus_stack: &[BusInfo; MAX_DEPTH],
    mut bus_index: usize,
) -> Option<u64> {
    loop {
        let bus = bus_stack[bus_index];
        addr = translate_one_bus(addr, bus)?;
        if bus_index == 0 {
            return Some(addr);
        }
        bus_index -= 1;
    }
}

fn translate_one_bus(addr: u64, bus: BusInfo) -> Option<u64> {
    // Linux 的 of_translate_address() 语义里:
    // - ranges 存在但长度为 0: 子总线地址和父总线地址 1:1。
    // - ranges 不存在: 严格说不少总线不能翻译。
    //
    // 这里为 bring-up 保守采用 1:1, 避免 QEMU 这类简单总线因为缺省 ranges 失效。
    if !bus.ranges_seen || bus.range_count == 0 {
        return Some(addr);
    }

    let mut i = 0usize;
    while i < bus.range_count {
        let r = bus.ranges[i];
        if r.size != 0 && addr >= r.child && addr - r.child < r.size {
            return Some(r.parent + (addr - r.child));
        }
        i += 1;
    }
    None
}

fn parse_ranges(
    sp: *const u32,
    i: usize,
    len: u32,
    bus_stack: &mut [BusInfo; MAX_DEPTH],
    depth: usize,
) {
    if depth < 2 {
        return;
    }

    let parent = bus_stack[depth - 2];
    let bus = &mut bus_stack[depth - 1];
    bus.ranges_seen = true;
    bus.range_count = 0;

    if len == 0 {
        return;
    }

    let entry_words = bus.addr_cells + parent.addr_cells + bus.size_cells;
    if entry_words == 0 {
        return;
    }

    let entries = (len / 4) / entry_words;
    let mut e = 0usize;
    while e < entries as usize && e < MAX_RANGES {
        let off = i + e * entry_words as usize;
        let child = read_addr_cells(sp, off, bus.addr_cells);
        let parent_addr = read_addr_cells(sp, off + bus.addr_cells as usize, parent.addr_cells);
        let size = read_size_cells(
            sp,
            off + bus.addr_cells as usize + parent.addr_cells as usize,
            bus.size_cells,
        );
        bus.ranges[e] = RangeMap {
            child,
            parent: parent_addr,
            size,
        };
        bus.range_count += 1;
        e += 1;
    }
}

fn reparse_current_ranges(sp: *const u32, bus_stack: &mut [BusInfo; MAX_DEPTH], depth: usize) {
    if depth == 0 {
        return;
    }
    let bus = bus_stack[depth - 1];
    if bus.ranges_len != 0 || bus.ranges_seen {
        parse_ranges(sp, bus.ranges_word_index, bus.ranges_len, bus_stack, depth);
    }
}

fn read_addr_cells(sp: *const u32, i: usize, cells: u32) -> u64 {
    if cells == 0 {
        return 0;
    }
    // PCI 地址通常是 3 cells: flags/space + high32 + low32。
    // 地址数值只取 high32/low32, flags 不参与 CPU 物理地址计算。
    if cells == 3 {
        return ((unsafe { read_be(sp.add(i + 1)) } as u64) << 32)
            | unsafe { read_be(sp.add(i + 2)) } as u64;
    }

    let mut value = 0u64;
    let mut c = 0u32;
    while c < cells {
        value = (value << 32) | unsafe { read_be(sp.add(i + c as usize)) } as u64;
        c += 1;
    }
    value
}

fn read_size_cells(sp: *const u32, i: usize, cells: u32) -> u64 {
    let mut value = 0u64;
    let mut c = 0u32;
    while c < cells {
        value = (value << 32) | unsafe { read_be(sp.add(i + c as usize)) } as u64;
        c += 1;
    }
    value
}

fn find_stdout_target_path(sp: *const u32, ss: *const u8) -> [u8; MAX_PATH] {
    let mut selector = [0u8; 64];
    let mut selector_len = 0usize;
    let mut target = [0u8; MAX_PATH];
    let mut path = [0u8; MAX_PATH];
    let mut path_len_stack = [0usize; MAX_DEPTH];
    let mut path_len = 0usize;
    let mut depth = 0usize;
    let mut i = 0usize;

    loop {
        let token = unsafe { read_be(sp.add(i)) };
        if token == FDT_END {
            return target;
        }

        if token == FDT_BEGIN_NODE {
            i += 1;
            let name_ptr = unsafe { sp.add(i) as *const u8 };
            let mut name_len = 0usize;
            while unsafe { *name_ptr.add(name_len) } != 0 {
                name_len += 1;
            }
            if depth < MAX_DEPTH {
                path_len_stack[depth] = path_len;
                append_node_to_path(&mut path, &mut path_len, name_ptr, name_len);
                depth += 1;
            }
            i += (name_len + 4) / 4;
        } else if token == FDT_END_NODE {
            i += 1;
            if depth > 0 {
                depth -= 1;
                path_len = path_len_stack[depth];
            }
        } else if token == FDT_PROP {
            i += 1;
            let len = unsafe { read_be(sp.add(i)) };
            i += 1;
            let nameoff = unsafe { read_be(sp.add(i)) };
            i += 1;
            let prop_name_ptr = unsafe { ss.add(nameoff as usize) };
            let value = unsafe { sp.add(i) } as *const u8;

            if path_eq_bytes(&path, path_len, b"/chosen")
                && (cstr_eq_stdout_path(prop_name_ptr) || cstr_eq_linux_stdout_path(prop_name_ptr))
            {
                copy_stdout_selector(value, len as usize, &mut selector, &mut selector_len);
            } else if path_eq_bytes(&path, path_len, b"/aliases")
                && selector_len != 0
                && cstr_eq_bytes(prop_name_ptr, &selector[..selector_len])
            {
                copy_cstr_to_buf(value, len as usize, &mut target);
            }

            if target[0] != 0 {
                return target;
            }
            i += ((len + 3) / 4) as usize;
        } else if token == FDT_NOP {
            i += 1;
        } else {
            return target;
        }
    }
}

fn append_node_to_path(
    path: &mut [u8; MAX_PATH],
    path_len: &mut usize,
    name_ptr: *const u8,
    name_len: usize,
) {
    if name_len == 0 {
        if *path_len == 0 {
            path[0] = b'/';
            *path_len = 1;
        }
        return;
    }
    if *path_len == 0 {
        path[0] = b'/';
        *path_len = 1;
    } else if *path_len > 1 && *path_len < MAX_PATH {
        path[*path_len] = b'/';
        *path_len += 1;
    }

    let mut i = 0usize;
    while i < name_len && *path_len < MAX_PATH - 1 {
        path[*path_len] = unsafe { *name_ptr.add(i) };
        *path_len += 1;
        i += 1;
    }
    if *path_len < MAX_PATH {
        path[*path_len] = 0;
    }
}

fn path_matches(path: &[u8; MAX_PATH], path_len: usize, target: &[u8; MAX_PATH]) -> bool {
    if target[0] == 0 {
        return false;
    }
    let mut len = 0usize;
    while len < MAX_PATH && target[len] != 0 {
        len += 1;
    }
    if len != path_len {
        return false;
    }
    let mut i = 0usize;
    while i < len {
        if path[i] != target[i] {
            return false;
        }
        i += 1;
    }
    true
}

fn path_eq_bytes(path: &[u8; MAX_PATH], path_len: usize, b: &[u8]) -> bool {
    if path_len != b.len() {
        return false;
    }
    let mut i = 0usize;
    while i < b.len() {
        if path[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

fn copy_stdout_selector(src: *const u8, len: usize, dst: &mut [u8; 64], dst_len: &mut usize) {
    *dst_len = 0;
    let mut i = 0usize;
    while i < len && i < dst.len() - 1 {
        let b = unsafe { *src.add(i) };
        if b == 0 || b == b':' {
            break;
        }
        dst[i] = b;
        *dst_len += 1;
        i += 1;
    }
    if *dst_len < dst.len() {
        dst[*dst_len] = 0;
    }
}

fn copy_cstr_to_buf(src: *const u8, len: usize, dst: &mut [u8; MAX_PATH]) {
    let mut i = 0usize;
    while i < len && i < MAX_PATH - 1 {
        let b = unsafe { *src.add(i) };
        if b == 0 {
            break;
        }
        dst[i] = b;
        i += 1;
    }
    if i < MAX_PATH {
        dst[i] = 0;
    }
}

fn prop_contains_pl011(prop: *const u8, len: usize) -> bool {
    let mut off = 0usize;
    while off < len {
        let start = off;
        while off < len && unsafe { *prop.add(off) } != 0 {
            off += 1;
        }
        if off > start {
            if is_arm_pl011(unsafe { prop.add(start) }, off - start) {
                return true;
            }
        }
        off += 1;
    }
    false
}

fn prop_contains_dwc3(prop: *const u8, len: usize) -> bool {
    let needle = b"snps,dwc3";
    let mut off = 0usize;
    while off < len {
        let start = off;
        while off < len && unsafe { *prop.add(off) } != 0 {
            off += 1;
        }
        if off - start == needle.len() {
            let mut equal = true;
            let mut j = 0usize;
            while j < needle.len() {
                if unsafe { *prop.add(start + j) } != needle[j] {
                    equal = false;
                    break;
                }
                j += 1;
            }
            if equal {
                return true;
            }
        }
        off += 1;
    }
    false
}

fn is_arm_pl011(ptr: *const u8, len: usize) -> bool {
    // "arm,pl011" (9 bytes)
    if len == 9 {
        unsafe {
            if *ptr.add(0) == b'a'
                && *ptr.add(1) == b'r'
                && *ptr.add(2) == b'm'
                && *ptr.add(3) == b','
                && *ptr.add(4) == b'p'
                && *ptr.add(5) == b'l'
                && *ptr.add(6) == b'0'
                && *ptr.add(7) == b'1'
                && *ptr.add(8) == b'1'
            {
                return true;
            }
        }
    }
    // "arm,pl011-axi" (13 bytes) — Raspberry Pi 5 的 UART10 使用这个 compatible。
    if len == 13 {
        unsafe {
            if *ptr.add(0) == b'a'
                && *ptr.add(1) == b'r'
                && *ptr.add(2) == b'm'
                && *ptr.add(3) == b','
                && *ptr.add(4) == b'p'
                && *ptr.add(5) == b'l'
                && *ptr.add(6) == b'0'
                && *ptr.add(7) == b'1'
                && *ptr.add(8) == b'1'
                && *ptr.add(9) == b'-'
                && *ptr.add(10) == b'a'
                && *ptr.add(11) == b'x'
                && *ptr.add(12) == b'i'
            {
                return true;
            }
        }
    }
    // "arm,sbsa-uart" (13 bytes) — PL011 in SBSA generic mode (Pi5 RP1 may use this)
    if len == 13 {
        unsafe {
            if *ptr.add(0) == b'a'
                && *ptr.add(1) == b'r'
                && *ptr.add(2) == b'm'
                && *ptr.add(3) == b','
                && *ptr.add(4) == b's'
                && *ptr.add(5) == b'b'
                && *ptr.add(6) == b's'
                && *ptr.add(7) == b'a'
                && *ptr.add(8) == b'-'
                && *ptr.add(9) == b'u'
                && *ptr.add(10) == b'a'
                && *ptr.add(11) == b'r'
                && *ptr.add(12) == b't'
            {
                return true;
            }
        }
    }
    false
}

fn cstr_eq_address_cells(ptr: *const u8) -> bool {
    unsafe {
        *ptr.add(0) == b'#'
            && *ptr.add(1) == b'a'
            && *ptr.add(2) == b'd'
            && *ptr.add(3) == b'd'
            && *ptr.add(4) == b'r'
            && *ptr.add(5) == b'e'
            && *ptr.add(6) == b's'
            && *ptr.add(7) == b's'
            && *ptr.add(8) == b'-'
            && *ptr.add(9) == b'c'
            && *ptr.add(10) == b'e'
            && *ptr.add(11) == b'l'
            && *ptr.add(12) == b'l'
            && *ptr.add(13) == b's'
            && *ptr.add(14) == 0
    }
}

fn cstr_eq_size_cells(ptr: *const u8) -> bool {
    unsafe {
        *ptr.add(0) == b'#'
            && *ptr.add(1) == b's'
            && *ptr.add(2) == b'i'
            && *ptr.add(3) == b'z'
            && *ptr.add(4) == b'e'
            && *ptr.add(5) == b'-'
            && *ptr.add(6) == b'c'
            && *ptr.add(7) == b'e'
            && *ptr.add(8) == b'l'
            && *ptr.add(9) == b'l'
            && *ptr.add(10) == b's'
            && *ptr.add(11) == 0
    }
}

fn cstr_eq_compatible(ptr: *const u8) -> bool {
    unsafe {
        *ptr.add(0) == b'c'
            && *ptr.add(1) == b'o'
            && *ptr.add(2) == b'm'
            && *ptr.add(3) == b'p'
            && *ptr.add(4) == b'a'
            && *ptr.add(5) == b't'
            && *ptr.add(6) == b'i'
            && *ptr.add(7) == b'b'
            && *ptr.add(8) == b'l'
            && *ptr.add(9) == b'e'
            && *ptr.add(10) == 0
    }
}

fn cstr_eq_reg(ptr: *const u8) -> bool {
    unsafe { *ptr.add(0) == b'r' && *ptr.add(1) == b'e' && *ptr.add(2) == b'g' && *ptr.add(3) == 0 }
}

fn cstr_eq_ranges(ptr: *const u8) -> bool {
    unsafe {
        *ptr.add(0) == b'r'
            && *ptr.add(1) == b'a'
            && *ptr.add(2) == b'n'
            && *ptr.add(3) == b'g'
            && *ptr.add(4) == b'e'
            && *ptr.add(5) == b's'
            && *ptr.add(6) == 0
    }
}

fn cstr_eq_status(ptr: *const u8) -> bool {
    unsafe {
        *ptr.add(0) == b's'
            && *ptr.add(1) == b't'
            && *ptr.add(2) == b'a'
            && *ptr.add(3) == b't'
            && *ptr.add(4) == b'u'
            && *ptr.add(5) == b's'
            && *ptr.add(6) == 0
    }
}

fn cstr_eq_stdout_path(ptr: *const u8) -> bool {
    unsafe {
        *ptr.add(0) == b's'
            && *ptr.add(1) == b't'
            && *ptr.add(2) == b'd'
            && *ptr.add(3) == b'o'
            && *ptr.add(4) == b'u'
            && *ptr.add(5) == b't'
            && *ptr.add(6) == b'-'
            && *ptr.add(7) == b'p'
            && *ptr.add(8) == b'a'
            && *ptr.add(9) == b't'
            && *ptr.add(10) == b'h'
            && *ptr.add(11) == 0
    }
}

fn cstr_eq_linux_stdout_path(ptr: *const u8) -> bool {
    unsafe {
        *ptr.add(0) == b'l'
            && *ptr.add(1) == b'i'
            && *ptr.add(2) == b'n'
            && *ptr.add(3) == b'u'
            && *ptr.add(4) == b'x'
            && *ptr.add(5) == b','
            && *ptr.add(6) == b's'
            && *ptr.add(7) == b't'
            && *ptr.add(8) == b'd'
            && *ptr.add(9) == b'o'
            && *ptr.add(10) == b'u'
            && *ptr.add(11) == b't'
            && *ptr.add(12) == b'-'
            && *ptr.add(13) == b'p'
            && *ptr.add(14) == b'a'
            && *ptr.add(15) == b't'
            && *ptr.add(16) == b'h'
            && *ptr.add(17) == 0
    }
}

fn cstr_eq_bytes(ptr: *const u8, bytes: &[u8]) -> bool {
    let mut i = 0usize;
    while i < bytes.len() {
        if unsafe { *ptr.add(i) } != bytes[i] {
            return false;
        }
        i += 1;
    }
    unsafe { *ptr.add(bytes.len()) == 0 }
}

fn prop_is_disabled(prop: *const u8, len: usize) -> bool {
    if len < 8 {
        return false;
    }
    unsafe {
        *prop.add(0) == b'd'
            && *prop.add(1) == b'i'
            && *prop.add(2) == b's'
            && *prop.add(3) == b'a'
            && *prop.add(4) == b'b'
            && *prop.add(5) == b'l'
            && *prop.add(6) == b'e'
            && *prop.add(7) == b'd'
    }
}

fn prop_contains_gic(prop: *const u8, len: usize) -> bool {
    let mut off = 0usize;
    while off < len {
        let start = off;
        while off < len && unsafe { *prop.add(off) } != 0 {
            off += 1;
        }
        if off > start {
            if is_gic_compatible(unsafe { prop.add(start) }, off - start) {
                return true;
            }
        }
        off += 1;
    }
    false
}

fn is_gic_compatible(ptr: *const u8, len: usize) -> bool {
    // "arm,cortex-a15-gic" (18 chars + null)
    if len == 18 {
        unsafe {
            if *ptr.add(0) == b'a'
                && *ptr.add(1) == b'r'
                && *ptr.add(2) == b'm'
                && *ptr.add(3) == b','
                && *ptr.add(4) == b'c'
                && *ptr.add(5) == b'o'
                && *ptr.add(6) == b'r'
                && *ptr.add(7) == b't'
                && *ptr.add(8) == b'e'
                && *ptr.add(9) == b'x'
                && *ptr.add(10) == b'-'
                && *ptr.add(11) == b'a'
                && *ptr.add(12) == b'1'
                && *ptr.add(13) == b'5'
                && *ptr.add(14) == b'-'
                && *ptr.add(15) == b'g'
                && *ptr.add(16) == b'i'
                && *ptr.add(17) == b'c'
            {
                return true;
            }
        }
    }
    // "arm,gic-400" (11 chars + null) — Pi5
    if len == 11 {
        unsafe {
            if *ptr.add(0) == b'a'
                && *ptr.add(1) == b'r'
                && *ptr.add(2) == b'm'
                && *ptr.add(3) == b','
                && *ptr.add(4) == b'g'
                && *ptr.add(5) == b'i'
                && *ptr.add(6) == b'c'
                && *ptr.add(7) == b'-'
                && *ptr.add(8) == b'4'
                && *ptr.add(9) == b'0'
                && *ptr.add(10) == b'0'
            {
                return true;
            }
        }
    }
    false
}

fn cstr_eq_interrupts(ptr: *const u8) -> bool {
    unsafe {
        *ptr.add(0) == b'i'
            && *ptr.add(1) == b'n'
            && *ptr.add(2) == b't'
            && *ptr.add(3) == b'e'
            && *ptr.add(4) == b'r'
            && *ptr.add(5) == b'r'
            && *ptr.add(6) == b'u'
            && *ptr.add(7) == b'p'
            && *ptr.add(8) == b't'
            && *ptr.add(9) == b's'
            && *ptr.add(10) == 0
    }
}

fn prop_contains_armv8_timer(ptr: *const u8, len: usize) -> bool {
    prop_contains_string(ptr, len, b"arm,armv8-timer")
}

fn prop_contains_string(ptr: *const u8, len: usize, expected: &[u8]) -> bool {
    let mut start = 0usize;
    while start < len {
        let mut end = start;
        while end < len && unsafe { *ptr.add(end) } != 0 {
            end += 1;
        }
        if end - start == expected.len() {
            let mut index = 0usize;
            while index < expected.len() && unsafe { *ptr.add(start + index) } == expected[index] {
                index += 1;
            }
            if index == expected.len() {
                return true;
            }
        }
        start = end.saturating_add(1);
    }
    false
}

/// 解析 GIC 标准 3-cell interrupts 属性: <type hw_num trigger>
/// 返回 GIC INTID。SPI: 32 + hw_num, PPI: 16 + hw_num。
fn parse_gic_interrupts(sp: *const u32, i: usize, len: u32) -> Option<IrqSpec> {
    if len < 12 {
        return None;
    }
    let itype = unsafe { read_be(sp.add(i)) };
    let hw_num = unsafe { read_be(sp.add(i + 1)) };
    let flags = unsafe { read_be(sp.add(i + 2)) };

    let intid = if itype == 0 {
        32u32.checked_add(hw_num)?
    } else if itype == 1 {
        16u32.checked_add(hw_num)?
    } else {
        return None;
    };
    Some(IrqSpec { intid, flags })
}

fn parse_rp1_interrupt(sp: *const u32, i: usize, len: u32) -> Option<IrqSpec> {
    if len >= 12 {
        return parse_gic_interrupts(sp, i, len);
    }
    if len < 8 {
        return None;
    }
    let local_irq = unsafe { read_be(sp.add(i)) };
    let flags = unsafe { read_be(sp.add(i + 1)) };
    let intid = 0x7f_u32.checked_add(local_irq)?;
    Some(IrqSpec { intid, flags })
}
