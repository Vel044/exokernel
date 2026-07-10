//! libOS —— EL0 UART IRQ 回显程序。
//!
//! DTB 发现由 `dtb.rs` 完成，PL011 寄存器操作由 vendored
//! `arm-pl011-uart` 完成。这里仅组织设备映射、IRQ syscall 和事件循环。

#![no_std]
#![no_main]

mod dtb;

use core::arch::asm;
use core::ptr::NonNull;

const SYS_PUTS: u64 = 2;
const SYS_EXIT: u64 = 5;
const SYS_MAP_MMIO: u64 = 6;
const SYS_IRQ_BIND: u64 = 7;
const SYS_IRQ_WAIT: u64 = 8;
const SYS_IRQ_ACK: u64 = 9;
const SYS_IRQ_UNBIND: u64 = 10;

/// EL0 自己选择 UART MMIO 在用户地址空间中的虚拟地址。
const UART_VA: u64 = 0x4040_0000;

#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start(dtb_va: u64) -> ! {
    svc_puts(b"[libos] EL0 boot, searching UART in DTB...\r\n");

    // fdt crate 负责设备树结构解析；dtb 模块补充 ranges 地址翻译和
    // GIC specifier 转换，最终返回 CPU 物理地址与 GIC INTID。
    let uart = match dtb::find_stdout_uart(dtb_va) {
        Ok(info) => info,
        Err(error) => {
            svc_puts(b"[libos] UART discovery failed, stage=");
            svc_hex(error.code());
            svc_puts(b"\r\n");
            svc(SYS_EXIT, error.code(), 0, 0);
            loop {
                core::hint::spin_loop();
            }
        }
    };

    svc_puts(b"[libos] UART pa=");
    svc_hex(uart.reg.base);
    svc_puts(b" size=");
    svc_hex(uart.reg.size);
    svc_puts(b" INTID=");
    svc_hex(uart.intid as u64);
    svc_puts(b"\r\n");

    // EL0 只能提出映射请求；EL1 检查 protect 表并写 stage-1 PTE。
    let map_result = svc(SYS_MAP_MMIO, uart.reg.base, uart.reg.size, UART_VA);
    if map_result != 0 {
        svc_puts(b"[libos] SYS_MAP_MMIO failed, code=");
        svc_hex(map_result);
        svc_puts(b"\r\n");
        svc(SYS_EXIT, map_result, 0, 0);
    }

    // EL1 只允许绑定它从同一份 DTB 登记的 UART INTID。
    let bind_result = irq_bind(uart.intid);
    if bind_result != 0 {
        svc_puts(b"[libos] SYS_IRQ_BIND failed, code=");
        svc_hex(bind_result);
        svc_puts(b"\r\n");
        svc(SYS_EXIT, bind_result, 0, 0);
    }

    // SAFETY:
    // - UART_VA 已由 SYS_MAP_MMIO 映射到 DTB 发现的 PL011 寄存器页；
    // - 当前只有一个 EL0 任务持有并访问这个寄存器块；
    // - 映射在驱动实例的整个生命周期内保持有效。

    // 把 EL0 虚拟地址 `UART_VA` 解释成一个指向 PL011 寄存器布局的裸指针；
    // `NonNull` 只是告诉 Rust 这个指针不是 0，真实映射由前面的 SYS_MAP_MMIO 保证。
    let uart_ptr = NonNull::new(UART_VA as *mut arm_pl011_uart::PL011Registers).unwrap();
    // 把非空裸指针包装成 arm-pl011-uart 需要的唯一 MMIO 指针；
    // unsafe 的原因是 Rust 无法证明这个地址真的是 UART、真的是 MMIO、且没有别名访问。
    let mmio_ptr = unsafe { arm_pl011_uart::UniqueMmioPointer::new(uart_ptr) };
    // 创建 PL011 驱动对象。之后 read_word/write_word/clear_interrupts
    // 都会通过这个对象对 UART_VA 对应的硬件寄存器做 volatile 访问。
    let mut pl011 = arm_pl011_uart::Uart::new(mmio_ptr);

    // 保留 UEFI/固件配置好的时钟、波特率和线路格式，只打开接收 FIFO
    // 与 receive-timeout 中断。Pi5 DTB 对该 UART 标记了 skip-init。
    use arm_pl011_uart::Interrupts;
    pl011.set_interrupt_masks(Interrupts::RXI | Interrupts::RTI);

    svc_puts(b"[libos] ready for input\r\n");

    loop {
        // WAIT 在 EL1 中启用已绑定 GIC IRQ 并执行 WFI，返回实际 INTID。
        let intid = irq_wait();

        // IRQ 只说明设备可能有数据；真正的数据仍由 EL0 直接读 UARTDR。
        loop {
            match pl011.read_word() {
                Ok(Some(byte)) => {
                    // arm-pl011-uart::write_word() 是非阻塞原语，不检查 UARTFR.TXFF。
                    // EL1 的 IRQ 日志可能刚填满 TX FIFO，因此必须等到有空位再回显。
                    while pl011.is_tx_fifo_full() {
                        core::hint::spin_loop();
                    }
                    pl011.write_word(byte);
                }
                Ok(None) => break,
                Err(error) => {
                    use arm_pl011_uart::Error;
                    match error {
                        Error::Overrun => svc_puts(b"[libos] UART RX error: overrun\r\n"),
                        Error::Break => svc_puts(b"[libos] UART RX error: break\r\n"),
                        Error::Parity => svc_puts(b"[libos] UART RX error: parity\r\n"),
                        Error::Framing => svc_puts(b"[libos] UART RX error: framing\r\n"),
                        Error::InvalidParameter => {
                            svc_puts(b"[libos] UART RX error: invalid parameter\r\n")
                        }
                    }
                    break;
                }
            }
        }

        // 先清 PL011 设备侧中断源，再让 EL1 向 GIC 写 EOIR。
        pl011.clear_interrupts(
            Interrupts::RXI
                | Interrupts::RTI
                | Interrupts::OEI
                | Interrupts::BEI
                | Interrupts::PEI
                | Interrupts::FEI,
        );
        irq_ack(intid as u32);
    }
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

fn irq_bind(intid: u32) -> u64 {
    svc(SYS_IRQ_BIND, intid as u64, 0, 0)
}

fn irq_wait() -> u64 {
    svc(SYS_IRQ_WAIT, 0, 0, 0)
}

fn irq_ack(intid: u32) -> u64 {
    svc(SYS_IRQ_ACK, intid as u64, 0, 0)
}

#[allow(dead_code)]
fn irq_unbind(intid: u32) -> u64 {
    svc(SYS_IRQ_UNBIND, intid as u64, 0, 0)
}

fn svc_puts(bytes: &[u8]) {
    svc(SYS_PUTS, bytes.as_ptr() as u64, bytes.len() as u64, 0);
}

fn svc_hex(value: u64) {
    let mut buffer = [b'0'; 18];
    buffer[0] = b'0';
    buffer[1] = b'x';

    let mut index = 0usize;
    while index < 16 {
        let shift = 60 - index * 4;
        let nibble = ((value >> shift) & 0xf) as u8;
        buffer[index + 2] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + nibble - 10
        };
        index += 1;
    }

    svc_puts(&buffer);
}

fn exit_with_message(message: &[u8], code: u64) -> ! {
    svc_puts(message);
    svc(SYS_EXIT, code, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
