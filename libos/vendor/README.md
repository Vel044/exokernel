# libOS 第三方驱动

## arm-pl011-uart

`arm-pl011-uart/` 是 `arm-pl011-uart 0.5.0` 的上游源码快照，许可证为
MIT OR Apache-2.0。libOS 通过本地 Cargo `path` 依赖使用它。

主要阅读入口：

- `src/lib.rs` 中的 `PL011Registers`：PL011 MMIO 寄存器布局。
- `Uart::read_word()`：读取 UARTDR，并处理 framing/parity/break/overrun 错误。
- `Uart::write_word()`：等待 TX FIFO 可写后写 UARTDR。
- `Uart::set_interrupt_masks()`：写 UARTIMSC，打开 RX/timeout 中断。
- `Uart::masked_interrupt_status()`：读 UARTMIS。
- `Uart::clear_interrupts()`：写 UARTICR，清除设备侧中断源。

这个库只操作已经映射好的 PL011 MMIO，不负责：

- 解析 DTB；
- 建立 EL0 页表映射；
- 配置 GIC；
- 提供 `SYS_IRQ_WAIT/ACK`；
- 决定哪个任务有权访问 UART。

这些保护和路由工作仍由 EL1 外核完成。

## fdt

`fdt/` 是 `fdt 0.1.5` 的上游源码快照，许可证为 MPL-2.0。它负责
校验和遍历 Flattened Device Tree；Pi5 所需的多层 `ranges` 地址翻译与
GIC interrupt specifier 转换保留在 `libos/src/dtb.rs`。
