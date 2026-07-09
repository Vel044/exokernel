//! libOS —— EL0 测试程序
//! 用 SVC 陷入 EL1 外核, 验证 EL0 用户态路径能跑。

#![no_std]
#![no_main]

use core::arch::asm;

const SYS_PUTS: u64 = 2;
const SYS_EXIT: u64 = 5;
const SYS_MAP_MMIO: u64 = 6;

const UART_VA: u64 = 0x4040_0000;
const PL011_DR: usize = 0x00;
const PL011_FR: usize = 0x18;
const PL011_FR_TXFF: u32 = 1 << 5;

const FDT_MAGIC: u32 = 0xd00dfeed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

#[derive(Clone, Copy)]
struct RegRange {
    base: u64,
    size: u64,
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

#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start(dtb_va: u64) -> ! {
    let msg = b"[libos] EL0 svc hello\r\n";
    svc(SYS_PUTS, msg.as_ptr() as u64, msg.len() as u64, 0);

    if let Some(uart_reg) = find_pl011_reg(dtb_va) {
        svc(SYS_MAP_MMIO, uart_reg.base, uart_reg.size, UART_VA);
        pl011_puts(UART_VA, b"U0\r\n");
    } else {
        let fail = b"[libos] PL011 not found in DTB\r\n";
        svc(SYS_PUTS, fail.as_ptr() as u64, fail.len() as u64, 0);
    }

    svc(SYS_EXIT, 0, 0, 0);

    loop {}
}

fn svc(sysno: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x8") sysno => _,
            inlateout("x0") arg0 => ret,
            inlateout("x1") arg1 => _,
            inlateout("x2") arg2 => _,
            lateout("x3") _,
            lateout("x4") _,
            lateout("x5") _,
            lateout("x6") _,
            lateout("x7") _,
            lateout("x9") _,
            lateout("x10") _,
            lateout("x11") _,
            lateout("x12") _,
            lateout("x13") _,
            lateout("x14") _,
            lateout("x15") _,
            lateout("x16") _,
            lateout("x17") _,
            options(nostack)
        );
    }
    ret
}

fn pl011_puts(uart_va: u64, s: &[u8]) {
    for &b in s {
        pl011_putc(uart_va, b);
    }
}

fn pl011_putc(uart_va: u64, c: u8) {
    let uart = uart_va as *mut u8;
    unsafe {
        for _ in 0..100_000 {
            let fr = (uart.add(PL011_FR) as *const u32).read_volatile();
            if (fr & PL011_FR_TXFF) == 0 {
                break;
            }
        }
        (uart.add(PL011_DR) as *mut u32).write_volatile(c as u32);
    }
}

fn find_pl011_reg(dtb_va: u64) -> Option<RegRange> {
    if dtb_va == 0 {
        return None;
    }

    let ptr = dtb_va as *const u32;
    if unsafe { read_be(ptr.add(0)) } != FDT_MAGIC {
        return None;
    }

    let off_struct = unsafe { read_be(ptr.add(2)) };
    let off_strings = unsafe { read_be(ptr.add(3)) };
    let sp = (dtb_va + off_struct as u64) as *const u32;
    let ss = (dtb_va + off_strings as u64) as *const u8;
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
            bus_stack[depth] = BusInfo::default();
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
            node_disabled = false;
            node_reg = None;
        } else if token == FDT_PROP {
            i += 1;
            let len = unsafe { read_be(sp.add(i)) };
            i += 1;
            let nameoff = unsafe { read_be(sp.add(i)) };
            i += 1;

            let pname = string_at(ss, nameoff as usize);
            if bytes_eq(pname, b"#address-cells") && len == 4 {
                bus_stack[depth - 1].addr_cells = unsafe { read_be(sp.add(i)) };
                reparse_current_ranges(sp, &mut bus_stack, depth);
            } else if bytes_eq(pname, b"#size-cells") && len == 4 {
                bus_stack[depth - 1].size_cells = unsafe { read_be(sp.add(i)) };
                reparse_current_ranges(sp, &mut bus_stack, depth);
            } else if bytes_eq(pname, b"ranges") {
                bus_stack[depth - 1].ranges_word_index = i;
                bus_stack[depth - 1].ranges_len = len;
                parse_ranges(sp, i, len, &mut bus_stack, depth);
            } else if bytes_eq(pname, b"status") {
                node_disabled = prop_is_disabled(unsafe { sp.add(i) } as *const u8, len as usize);
            } else if bytes_eq(pname, b"compatible") {
                let prop = unsafe { sp.add(i) } as *const u8;
                node_compatible = prop_contains_string(prop, len as usize, b"arm,pl011")
                    || prop_contains_string(prop, len as usize, b"arm,pl011-axi")
                    || prop_contains_string(prop, len as usize, b"arm,sbsa-uart");
            } else if bytes_eq(pname, b"reg") && node_reg.is_none() {
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
    if parent.addr_cells == 0 || parent.size_cells == 0 || len < entry_words * 4 {
        return None;
    }
    let raw_base = read_addr_cells(sp, i, parent.addr_cells);
    let size = read_size_cells(sp, i + parent.addr_cells as usize, parent.size_cells);
    if size == 0 {
        return None;
    }
    translate_to_cpu(raw_base, bus_stack, depth - 2).map(|base| RegRange { base, size })
}

fn translate_to_cpu(mut addr: u64, bus_stack: &[BusInfo; MAX_DEPTH], mut bus_index: usize) -> Option<u64> {
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

fn parse_ranges(sp: *const u32, i: usize, len: u32, bus_stack: &mut [BusInfo; MAX_DEPTH], depth: usize) {
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
        bus.ranges[e] = RangeMap {
            child: read_addr_cells(sp, off, bus.addr_cells),
            parent: read_addr_cells(sp, off + bus.addr_cells as usize, parent.addr_cells),
            size: read_size_cells(
                sp,
                off + bus.addr_cells as usize + parent.addr_cells as usize,
                bus.size_cells,
            ),
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
    if cells == 3 {
        return ((unsafe { read_be(sp.add(i + 1)) } as u64) << 32) | unsafe { read_be(sp.add(i + 2)) } as u64;
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

fn string_at(base: *const u8, off: usize) -> &'static [u8] {
    let ptr = unsafe { base.add(off) };
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    unsafe { core::slice::from_raw_parts(ptr, len) }
}

fn prop_contains_string(prop: *const u8, len: usize, needle: &[u8]) -> bool {
    let mut off = 0usize;
    while off < len {
        let start = off;
        while off < len && unsafe { *prop.add(off) } != 0 {
            off += 1;
        }
        if off > start {
            let s = unsafe { core::slice::from_raw_parts(prop.add(start), off - start) };
            if bytes_eq(s, needle) {
                return true;
            }
        }
        off += 1;
    }
    false
}

fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0usize;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
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
            let pname = string_at(ss, nameoff as usize);
            let value = unsafe { sp.add(i) } as *const u8;

            if path_eq_bytes(&path, path_len, b"/chosen")
                && (bytes_eq(pname, b"stdout-path") || bytes_eq(pname, b"linux,stdout-path"))
            {
                copy_stdout_selector(value, len as usize, &mut selector, &mut selector_len);
            } else if path_eq_bytes(&path, path_len, b"/aliases")
                && selector_len != 0
                && cstr_eq_bytes(value_name_ptr(ss, nameoff as usize), &selector[..selector_len])
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

fn value_name_ptr(ss: *const u8, nameoff: usize) -> *const u8 {
    unsafe { ss.add(nameoff) }
}

fn append_node_to_path(path: &mut [u8; MAX_PATH], path_len: &mut usize, name_ptr: *const u8, name_len: usize) {
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

unsafe fn read_be(p: *const u32) -> u32 {
    u32::from_be(unsafe { core::ptr::read_volatile(p) })
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
