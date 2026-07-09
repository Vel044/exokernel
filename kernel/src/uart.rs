//! uart.rs —— DTB 发现后的 PL011 调试串口驱动
//!
//! EBS 前不要用这个模块打印, 因为那时应该走 UEFI console。
//! EBS 后 EL1 先从 DTB 找到 compatible="arm,pl011" 的 reg 基址,
//! 调 uart::init(base) 后, 这里才真正写 MMIO UART。

use core::ptr::{read_volatile, write_volatile};

const PL011_DR: usize = 0x00;       // 数据寄存器偏移: 写=发字符, 读=收字符
const PL011_FR: usize = 0x18;       // 标志寄存器偏移: 读状态
const PL011_FR_TXFF: u32 = 1 << 5;  // FR 的第 5 位: 1 = 发送 FIFO 满了 (不能写)

static mut UART_BASE: u64 = 0;

/// Pi5 UART10 的已知 CPU 物理地址, 只作为 early bring-up 诊断兜底。
///
/// 正式路径仍然是从 DTB 的 stdout-path/aliases/reg/ranges 解析 UART。
#[cfg(feature = "pi5")]
pub const PI5_DEBUG_UART_BASE: u64 = 0x107d_001000;

/// 初始化运行时 UART MMIO 基址。
///
/// base 来自 DTB 的 reg 物理地址。当前 EL1 打开 MMU 前是物理地址直接访问;
/// 打开 MMU 后 kmain 会给同一物理地址做 identity Device 映射, 所以这里仍可用。
pub fn init(base: u64) {
    unsafe {
        UART_BASE = base;
    }
}

/// UART 是否已经由 DTB 初始化。
pub fn is_ready() -> bool {
    unsafe { UART_BASE != 0 }
}

/// 返回当前 UART 基址。未初始化时返回 0。
pub fn base() -> u64 {
    unsafe { UART_BASE }
}

/// 发一个字符到串口
/// 先忙等 FR 的 TXFF=0 (FIFO 有空位), 再写 DR
pub fn putc(c: u8) {
    unsafe {
        let base = UART_BASE;
        if base == 0 {
            return;
        }
        let uart = base as *mut u8;
        // 当 TX FF (Transmit FIFO Full) 位=1 时, FIFO 满了, 短暂等待。
        // bring-up 阶段不能让串口状态把内核永久卡死, 所以这里有限自旋。
        for _ in 0..100_000 {
            if (read_volatile(uart.add(PL011_FR) as *const u32) & PL011_FR_TXFF) == 0 {
                break;
            }
        }
        // 往数据寄存器写一个字节 = 串口发出去
        write_volatile(uart.add(PL011_DR) as *mut u32, c as u32);
    }
}

/// 发字符串
/// \n 自动补 \r (终端的换行需要 \r\n 光标才能回到行首)
pub fn puts(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}

/// 发 64 位数的 16 进制格式 (固定 16 位)
/// 用于调试打印地址和状态码, 如 puts("0x00000000deadbeef")
pub fn hex(v: u64) {
    puts("0x");
    let mut shift = 60;
    loop {
        let nibble = ((v >> shift) & 0xf) as u8;
        let ch = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + (nibble - 10)
        };
        putc(ch);
        if shift == 0 {
            break;
        }
        shift -= 4;
    }
}
