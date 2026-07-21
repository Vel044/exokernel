use core::ptr::NonNull;

/// QEMU xHCI 模式启动前的 UART 回归检查。
///
/// 这里不能进入永久 IRQ echo 循环，否则后续 xHCI 永远没有机会运行。
/// 测试覆盖 EL0 UART MMIO 直写和 IRQ 授权绑定/解绑。映射会保留，供后续
/// `runtime::puts()` 直接使用；任务退出时由 EL1 统一撤销。
pub fn smoke_test(info: &exo_abi::UserBootInfo) -> Result<(), u64> {
    let resource = info.uart;
    crate::runtime::puts(b"[libos] 1. UART smoke test\r\n");
    crate::runtime::map_mmio(resource.base, resource.size, exo_abi::UART_VA)?;

    let pointer =
        NonNull::new(exo_abi::UART_VA as *mut arm_pl011_uart::PL011Registers).ok_or(0x110u64)?;
    let mmio = unsafe { arm_pl011_uart::UniqueMmioPointer::new(pointer) };
    let mut uart = arm_pl011_uart::Uart::new(mmio);
    write_bytes(&mut uart, b"[libos:uart-mmio] EL0 PL011 TX works\r\n");
    drop(uart);

    if let Err(error) = crate::runtime::irq_bind(resource.intid) {
        let _ = crate::runtime::unmap_mmio(exo_abi::UART_VA, resource.size);
        return Err(error);
    }
    if let Err(error) = crate::runtime::irq_unbind(resource.intid) {
        let _ = crate::runtime::unmap_mmio(exo_abi::UART_VA, resource.size);
        return Err(error);
    }
    crate::runtime::enable_direct_uart();
    crate::runtime::puts(b"[libos] UART MMIO + IRQ smoke passed\r\n");
    Ok(())
}

pub fn run(info: &exo_abi::UserBootInfo) -> ! {
    let uart = info.uart;
    crate::runtime::puts(b"[libos] UART pa=");
    crate::runtime::hex(uart.base);
    crate::runtime::puts(b" INTID=");
    crate::runtime::hex(uart.intid as u64);
    crate::runtime::puts(b"\r\n");

    if let Err(error) = crate::runtime::map_mmio(uart.base, uart.size, exo_abi::UART_VA) {
        crate::runtime::puts(b"[libos] UART map failed=");
        crate::runtime::hex(error);
        crate::runtime::exit(error);
    }
    if let Err(error) = crate::runtime::irq_bind(uart.intid) {
        crate::runtime::puts(b"[libos] UART bind failed=");
        crate::runtime::hex(error);
        crate::runtime::exit(error);
    }

    let pointer = NonNull::new(exo_abi::UART_VA as *mut arm_pl011_uart::PL011Registers).unwrap();
    let mmio = unsafe { arm_pl011_uart::UniqueMmioPointer::new(pointer) };
    let mut uart = arm_pl011_uart::Uart::new(mmio);
    use arm_pl011_uart::Interrupts;
    uart.set_interrupt_masks(Interrupts::RXI | Interrupts::RTI);
    crate::runtime::puts(b"[libos] UART echo ready\r\n");

    loop {
        let intid = crate::runtime::irq_wait();
        while let Ok(Some(byte)) = uart.read_word() {
            while uart.is_tx_fifo_full() {
                core::hint::spin_loop();
            }
            uart.write_word(byte);
        }
        uart.clear_interrupts(
            Interrupts::RXI
                | Interrupts::RTI
                | Interrupts::OEI
                | Interrupts::BEI
                | Interrupts::PEI
                | Interrupts::FEI,
        );
        crate::runtime::irq_ack(intid);
    }
}

fn write_bytes(uart: &mut arm_pl011_uart::Uart, bytes: &[u8]) {
    for &byte in bytes {
        while uart.is_tx_fifo_full() {
            core::hint::spin_loop();
        }
        uart.write_word(byte);
    }
}
