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

#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start(dtb_va: u64) -> ! {
    let msg = b"[libos] EL0 svc hello\r\n";
    svc(SYS_PUTS, msg.as_ptr() as u64, msg.len() as u64, 0);

    if let Some(uart_reg) = find_compatible_reg(dtb_va, b"arm,pl011") {
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

fn find_compatible_reg(dtb_va: u64, compatible: &[u8]) -> Option<RegRange> {
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

    let mut addr_cells = 2u32;
    let mut size_cells = 2u32;
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
            addr_cells = 2;
            size_cells = 2;
            node_compatible = false;
            node_reg = None;
            i += 1;
        } else if token == FDT_PROP {
            i += 1;
            let len = unsafe { read_be(sp.add(i)) };
            i += 1;
            let nameoff = unsafe { read_be(sp.add(i)) };
            i += 1;

            let pname = string_at(ss, nameoff as usize);
            if bytes_eq(pname, b"#address-cells") && len == 4 {
                addr_cells = unsafe { read_be(sp.add(i)) };
            } else if bytes_eq(pname, b"#size-cells") && len == 4 {
                size_cells = unsafe { read_be(sp.add(i)) };
            } else if bytes_eq(pname, b"compatible") {
                node_compatible = prop_contains_string(unsafe { sp.add(i) } as *const u8, len as usize, compatible);
            } else if bytes_eq(pname, b"reg") && node_reg.is_none() {
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

unsafe fn read_be(p: *const u32) -> u32 {
    u32::from_be(unsafe { core::ptr::read_volatile(p) })
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
