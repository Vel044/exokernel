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
const FDT_MAGIC: u32 = 0xd00dfeed;  // FDT 魔数 (大端)
// 遍历 structure block 时遇到的 token
const FDT_BEGIN_NODE: u32 = 1;       // 开始一个设备节点
const FDT_END_NODE: u32 = 2;         // 结束一个设备节点
const FDT_PROP: u32 = 3;             // 属性 (key-value 对)
const FDT_NOP: u32 = 4;              // 空指令 (跳过)
const FDT_END: u32 = 9;              // 整棵树结束

#[derive(Clone, Copy)]
pub struct RegRange {
    pub base: u64,
    pub size: u64,
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
    Some(unsafe { read_be(ptr.add(1)) } as u64)
}

/// 静默查找第一个 compatible 匹配的节点, 返回它的第一个 reg 范围。
///
/// 这个函数不打印, 用在 UART 初始化前。当前 parser 只实现 bring-up 需要的
/// FDT 子集: root 下 64-bit address/size cell, compatible 字符串列表, reg。
pub fn find_pl011_reg(dtb_paddr: u64) -> Option<RegRange> {
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

    let mut addr_cells: u32 = 2;
    let mut size_cells: u32 = 2;
    let mut node_compatible = false;
    let mut node_reg: Option<RegRange> = None;
    let mut i = 0usize;

    loop {
        let token = unsafe { read_be(sp.add(i)) };
        if token == FDT_END {
            return None;
        }

        if token == FDT_BEGIN_NODE {
            i += 1;
            let name_ptr = unsafe { sp.add(i) as *const u8 };
            let mut name_len = 0usize;
            while unsafe { *name_ptr.add(name_len) } != 0 {
                name_len += 1;
            }
            node_compatible = false;
            node_reg = None;
            i += (name_len + 4) / 4;
        } else if token == FDT_END_NODE {
            if node_compatible {
                if let Some(reg) = node_reg {
                    return Some(reg);
                }
            }
            i += 1;
            addr_cells = 2;
            size_cells = 2;
            node_compatible = false;
            node_reg = None;
        } else if token == FDT_PROP {
            i += 1;
            let len = unsafe { read_be(sp.add(i)) };
            i += 1;
            let nameoff = unsafe { read_be(sp.add(i)) };
            i += 1;

            let prop_name_ptr = unsafe { ss.add(nameoff as usize) };

            if cstr_eq_address_cells(prop_name_ptr) && len == 4 {
                addr_cells = unsafe { read_be(sp.add(i)) };
            } else if cstr_eq_size_cells(prop_name_ptr) && len == 4 {
                size_cells = unsafe { read_be(sp.add(i)) };
            } else if cstr_eq_compatible(prop_name_ptr) {
                node_compatible = prop_contains_pl011(unsafe { sp.add(i) } as *const u8, len as usize);
            } else if cstr_eq_reg(prop_name_ptr) && node_reg.is_none() {
                node_reg = read_first_reg(sp, i, addr_cells, size_cells);
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

    let totalsize = unsafe { read_be(ptr.add(1)) };   // DTB 总字节数
    let off_struct = unsafe { read_be(ptr.add(2)) };   // structure block 偏移
    let off_strings = unsafe { read_be(ptr.add(3)) };  // strings block 偏移

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
    let mut addr_cells: u32 = 2;  // 地址单元格数: 默认 2 = 64 位地址
    let mut size_cells: u32 = 2;  // 大小单元格数: 默认 2 = 64 位大小
    let mut current_is_memory = false;

    let mut i = 0usize;  // 当前指向 structure block 的第几个 u32
    loop {
        let token = unsafe { read_be(sp.add(i)) };
        if token == FDT_END {
            break;  // 整棵树结束
        }
        if token == FDT_BEGIN_NODE {
            i += 1;
            // 读节点名 (null 结尾, 对齐到 4 字节)
            let name_ptr = unsafe { sp.add(i) as *const u8 };
            let mut name_len = 0usize;
            while unsafe { *name_ptr.add(name_len) } != 0 {
                name_len += 1;
            }
            let name = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(name_ptr, name_len)) };
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
            let len = unsafe { read_be(sp.add(i)) };     // 属性值的字节数
            i += 1;
            let nameoff = unsafe { read_be(sp.add(i)) };  // 属性名在 strings block 的偏移
            i += 1;
            // 读属性名
            let prop_name_ptr = unsafe { ss.add(nameoff as usize) };
            let mut pnl = 0usize;
            while unsafe { *prop_name_ptr.add(pnl) } != 0 {
                pnl += 1;
            }
            let pname =
                unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(prop_name_ptr, pnl)) };

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
                        ((unsafe { read_be(sp.add(base_offset + addr_cells as usize)) }) as u64) << 32
                            | (unsafe { read_be(sp.add(base_offset + addr_cells as usize + 1)) }) as u64
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
            i += 1;  // 空操作, 跳过
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

fn read_first_reg(sp: *const u32, i: usize, addr_cells: u32, size_cells: u32) -> Option<RegRange> {
    if addr_cells == 0 || size_cells == 0 {
        return None;
    }

    let base = if addr_cells == 2 {
        ((unsafe { read_be(sp.add(i)) }) as u64) << 32 | (unsafe { read_be(sp.add(i + 1)) }) as u64
    } else {
        (unsafe { read_be(sp.add(i)) }) as u64
    };
    let size_offset = i + addr_cells as usize;
    let size = if size_cells == 2 {
        ((unsafe { read_be(sp.add(size_offset)) }) as u64) << 32
            | (unsafe { read_be(sp.add(size_offset + 1)) }) as u64
    } else {
        (unsafe { read_be(sp.add(size_offset)) }) as u64
    };

    if size == 0 {
        None
    } else {
        Some(RegRange { base, size })
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

fn is_arm_pl011(ptr: *const u8, len: usize) -> bool {
    if len != 9 {
        return false;
    }
    unsafe {
        *ptr.add(0) == b'a'
            && *ptr.add(1) == b'r'
            && *ptr.add(2) == b'm'
            && *ptr.add(3) == b','
            && *ptr.add(4) == b'p'
            && *ptr.add(5) == b'l'
            && *ptr.add(6) == b'0'
            && *ptr.add(7) == b'1'
            && *ptr.add(8) == b'1'
    }
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
