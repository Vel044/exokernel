// SPDX-FileCopyrightText: Copyright The arm-pl011-uart Contributors.
// SPDX-License-Identifier: MIT OR Apache-2.0

#![no_std]
#![doc = include_str!("../README.md")]
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(unsafe_op_in_unsafe_fn)]

//! ## Example
//!
//! ```rust
//! use arm_pl011_uart::{DataBits, LineConfig, Parity, PL011Registers, StopBits, Uart, UniqueMmioPointer};
//! use core::{fmt::Write, ptr::NonNull};
//! # use zerocopy::transmute_mut;
//! # let mut fake_registers = [0u32; 1024];
//! # let UART_ADDRESS : *mut PL011Registers = transmute_mut!(&mut fake_registers);
//!
//! // SAFETY: `UART_ADDRESS` is the base address of a PL011 UART register block. It remains valid for
//! // the lifetime of the application and nothing else references this address range.
//! let uart_pointer = unsafe { UniqueMmioPointer::new(NonNull::new(UART_ADDRESS).unwrap()) };
//!
//! // Create driver instance
//! let mut uart = Uart::new(uart_pointer);
//!
//! // Configure and enable UART
//! let line_config = LineConfig {
//!     data_bits: DataBits::Bits8,
//!     parity: Parity::None,
//!     stop_bits: StopBits::One,
//! };
//! uart.enable(line_config, 115_200, 16_000_000);
//!
//! // Send and receive data
//! uart.write_word(0x5a);
//! uart.write_str("Hello Uart!");
//! println!("{:?}", uart.read_word());
//! ```

#[cfg(feature = "embedded-hal-nb")]
mod embedded_hal_nb;
#[cfg(feature = "embedded-io")]
mod embedded_io;

use bitflags::bitflags;
use core::fmt;
pub use safe_mmio::UniqueMmioPointer;
use safe_mmio::{
    field, field_shared,
    fields::{ReadPure, ReadPureWrite, ReadWrite, WriteOnly},
};
use thiserror::Error;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// Register descriptions

/// Data Register
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
struct DataRegister(u32);

/// Receive Status Register/Error Clear Register, UARTRSR/UARTECR
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
struct ReceiveStatusRegister(u32);

/// Flag Register, UARTFR
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
struct FlagsRegister(u32);

/// Line Control Register, UARTLCR_H
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
struct LineControlRegister(u32);

/// Control Register, UARTCR
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
struct ControlRegister(u32);

/// Set of interrupts. This is used for the interrupt status registers (UARTRIS and UARTMIS),
/// interrupt mask register (UARTIMSC) and and interrupt clear register (UARTICR).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
pub struct Interrupts(u32);

bitflags! {
    impl DataRegister: u32 {
        /// Overrun error
        const OE = 1 << 11;
        /// Break error
        const BE = 1 << 10;
        /// Parity error
        const PE = 1 << 9;
        /// Framing error
        const FE = 1 << 8;
    }

    impl ReceiveStatusRegister: u32 {
        /// Overrun error
        const OE = 1 << 3;
        /// Break error
        const BE = 1 << 2;
        /// Parity error
        const PE = 1 << 1;
        /// Framing error
        const FE = 1 << 0;
    }

    impl FlagsRegister: u32 {
        /// Ring indicator
        const RI = 1 << 8;
        /// Transmit FIFO is empty
        const TXFE = 1 << 7;
        /// Receive FIFO is full
        const RXFF = 1 << 6;
        /// Transmit FIFO is full
        const TXFF = 1 << 5;
        /// Receive FIFO is empty
        const RXFE = 1 << 4;
        /// UART busy
        const BUSY = 1 << 3;
        /// Data carrier detect
        const DCD = 1 << 2;
        /// Data set ready
        const DSR = 1 << 1;
        /// Clear to send
        const CTS = 1 << 0;
    }

    impl LineControlRegister: u32 {
        /// Stick parity select.
        const SPS = 1 << 7;
        /// Word length
        const WLEN_5BITS = 0b00 << 5;
        const WLEN_6BITS = 0b01 << 5;
        const WLEN_7BITS = 0b10 << 5;
        const WLEN_8BITS = 0b11 << 5;
        /// Enable FIFOs
        const FEN = 1 << 4;
        /// Two stop bits select
        const STP2 = 1 << 3;
        /// Even parity select
        const EPS = 1 << 2;
        /// Parity enable
        const PEN = 1 << 1;
        /// Send break
        const BRK = 1 << 0;
    }

    impl ControlRegister: u32 {
        /// CTS hardware flow control enable
        const CTSEn = 1 << 15;
        /// RTS hardware flow control enable
        const RTSEn = 1 << 14;
        /// This bit is the complement of the UART Out2 (nUARTOut2) modem status output
        const Out2 = 1 << 13;
        /// This bit is the complement of the UART Out1 (nUARTOut1) modem status output
        const Out1 = 1 << 12;
        /// Request to send
        const RTS = 1 << 11;
        /// Data transmit ready
        const DTR = 1 << 10;
        /// Receive enable
        const RXE = 1 << 9;
        /// Transmit enable
        const TXE = 1 << 8;
        /// Loopback enable
        const LBE = 1 << 7;
        /// SIR low-power IrDA mode
        const SIRLP = 1 << 2;
        /// SIR enable
        const SIREN = 1 << 1;
        /// UART enable
        const UARTEN = 1 << 0;
    }

    impl Interrupts: u32 {
        /// Overrun error interrupt.
        const OEI = 1 << 10;
        /// Break error interrupt.
        const BEI = 1 << 9;
        /// Parity error interrupt.
        const PEI = 1 << 8;
        /// Framing error interrupt.
        const FEI = 1 << 7;
        /// Receive timeout interrupt.
        const RTI = 1 << 6;
        /// Transmit interrupt.
        const TXI = 1 << 5;
        /// Receive interrupt.
        const RXI = 1 << 4;
        /// nUARTDSR modem interrupt.
        const DSRMI = 1 << 3;
        /// nUARTDCD modem interrupt.
        const DCDMI = 1 << 2;
        /// nUARTCTS modem interrupt.
        const CTSMI = 1 << 1;
        /// nUARTRI modem interrupt.
        const RIMI = 1 << 0;
    }
}

/// PL011 register map
#[derive(Clone, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
#[repr(C, align(4))]
pub struct PL011Registers {
    /// 0x000: Data Register
    uartdr: ReadWrite<u32>,
    /// 0x004: Receive Status Register/Error Clear Register
    uartrsr_ecr: ReadPureWrite<u32>,
    /// 0x008 - 0x014
    reserved_08: [u32; 4],
    /// 0x018: Flag Register
    uartfr: ReadPure<FlagsRegister>,
    /// 0x01C
    reserved_1c: u32,
    /// 0x020: IrDA Low-Power Counter Register
    uartilpr: ReadPureWrite<u32>,
    /// 0x024: Integer Baud Rate Register
    uartibrd: ReadPureWrite<u32>,
    /// 0x028: Fractional Baud Rate Register
    uartfbrd: ReadPureWrite<u32>,
    /// 0x02C: Line Control Register
    uartlcr_h: ReadPureWrite<LineControlRegister>,
    /// 0x030: Control Register
    uartcr: ReadPureWrite<ControlRegister>,
    /// 0x034: Interrupt FIFO Level Select Register
    uartifls: ReadPureWrite<u32>,
    /// 0x038: Interrupt Mask Set/Clear Register
    uartimsc: ReadPureWrite<Interrupts>,
    /// 0x03C: Raw Interrupt Status Register
    uartris: ReadPure<Interrupts>,
    /// 0x040: Masked INterrupt Status Register
    uartmis: ReadPure<Interrupts>,
    /// 0x044: Interrupt Clear Register
    uarticr: WriteOnly<Interrupts>,
    /// 0x048: DMA control Register
    uartdmacr: ReadPureWrite<u32>,
    /// 0x04C - 0xFDC
    reserved_4c: [u32; 997],
    /// 0xFE0: UARTPeriphID0 Register
    uartperiphid0: ReadPure<u32>,
    /// 0xFE4: UARTPeriphID1 Register
    uartperiphid1: ReadPure<u32>,
    /// 0xFE8: UARTPeriphID2 Register
    uartperiphid2: ReadPure<u32>,
    /// 0xFEC: UARTPeriphID3 Register
    uartperiphid3: ReadPure<u32>,
    /// 0xFF0: UARTPCellID0 Register
    uartpcellid0: ReadPure<u32>,
    /// 0xFF4: UARTPCellID1 Register
    uartpcellid1: ReadPure<u32>,
    /// 0xFF8: UARTPCellID2 Register
    uartpcellid2: ReadPure<u32>,
    /// 0xFFC: UARTPCellID3 Register
    uartpcellid3: ReadPure<u32>,
}

// Config

/// Data bit count
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataBits {
    Bits5,
    Bits6,
    Bits7,
    Bits8,
}

/// Parity
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Parity {
    None,
    Even,
    Odd,
    One,
    Zero,
}

/// Stop bit count
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopBits {
    One,
    Two,
}

/// UART line config structure
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineConfig {
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
}

/// RX/TX interrupt FIFO levels
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FifoLevel {
    Bytes4 = 0b000,
    Bytes8 = 0b001,
    Bytes16 = 0b010,
    Bytes24 = 0b011,
    Bytes28 = 0b100,
}

/// UART peripheral identification structure
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Identification {
    pub part_number: u16,
    pub designer: u8,
    pub revision_number: u8,
    pub configuration: u8,
}

impl Identification {
    const PART_NUMBER: u16 = 0x11;
    const DESIGNER_ARM: u8 = b'A';
    const REVISION_MAX: u8 = 0x03;
    const CONFIGURATION: u8 = 0x00;

    /// Check if the identification block describes a valid PL011 peripheral
    pub fn is_valid(&self) -> bool {
        self.part_number == Self::PART_NUMBER
            && self.designer == Self::DESIGNER_ARM
            && self.revision_number <= Self::REVISION_MAX
            && self.configuration == Self::CONFIGURATION
    }
}

/// PL011 UART error type
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum Error {
    #[error("Invalid parameter")]
    InvalidParameter,
    #[error("Overrun")]
    Overrun,
    #[error("Break")]
    Break,
    #[error("Parity")]
    Parity,
    #[error("Framing")]
    Framing,
}

/// PL011 UART implementation
pub struct Uart<'a> {
    regs: UniqueMmioPointer<'a, PL011Registers>,
}

impl<'a> Uart<'a> {
    /// Creates new UART instance.
    pub fn new(regs: UniqueMmioPointer<'a, PL011Registers>) -> Self {
        Self { regs }
    }

    /// Configure and enable UART
    pub fn enable(&mut self, config: LineConfig, baud_rate: u32, sysclk: u32) -> Result<(), Error> {
        // Baud rate
        let (uartibrd, uartfbrd) = Self::calculate_baud_rate_divisor(baud_rate, sysclk)?;

        // Line control register
        let line_control = match config.data_bits {
            DataBits::Bits5 => LineControlRegister::WLEN_5BITS,
            DataBits::Bits6 => LineControlRegister::WLEN_6BITS,
            DataBits::Bits7 => LineControlRegister::WLEN_7BITS,
            DataBits::Bits8 => LineControlRegister::WLEN_8BITS,
        } | match config.parity {
            Parity::None => LineControlRegister::empty(),
            Parity::Even => LineControlRegister::PEN | LineControlRegister::EPS,
            Parity::Odd => LineControlRegister::PEN,
            Parity::One => LineControlRegister::PEN | LineControlRegister::SPS,
            Parity::Zero => {
                LineControlRegister::PEN | LineControlRegister::EPS | LineControlRegister::SPS
            }
        } | match config.stop_bits {
            StopBits::One => LineControlRegister::empty(),
            StopBits::Two => LineControlRegister::STP2,
        } | LineControlRegister::FEN;

        field!(self.regs, uartrsr_ecr).write(0);
        field!(self.regs, uartcr).write(ControlRegister::empty());

        field!(self.regs, uartibrd).write(uartibrd);
        field!(self.regs, uartfbrd).write(uartfbrd);
        field!(self.regs, uartlcr_h).write(line_control);

        field!(self.regs, uartcr)
            .write(ControlRegister::RXE | ControlRegister::TXE | ControlRegister::UARTEN);

        Ok(())
    }

    /// Disable UART
    pub fn disable(&mut self) {
        field!(self.regs, uartcr).write(ControlRegister::empty());
    }

    /// Check if receive FIFO is empty
    pub fn is_rx_fifo_empty(&self) -> bool {
        self.flags().contains(FlagsRegister::RXFE)
    }

    /// Check if receive FIFO is full
    pub fn is_rx_fifo_full(&self) -> bool {
        self.flags().contains(FlagsRegister::RXFF)
    }

    /// Check if transmit FIFO is empty
    pub fn is_tx_fifo_empty(&self) -> bool {
        self.flags().contains(FlagsRegister::TXFE)
    }

    /// Check if transmit FIFO is full
    pub fn is_tx_fifo_full(&self) -> bool {
        self.flags().contains(FlagsRegister::TXFF)
    }

    /// Check if UART is busy
    pub fn is_busy(&self) -> bool {
        self.flags().contains(FlagsRegister::BUSY)
    }

    /// Reads and returns the flag register.
    fn flags(&self) -> FlagsRegister {
        field_shared!(self.regs, uartfr).read()
    }

    /// Non-blocking read of a single byte from the UART.
    ///
    /// Returns `Ok(None)` if no data is available to read.
    pub fn read_word(&mut self) -> Result<Option<u8>, Error> {
        if self.is_rx_fifo_empty() {
            return Ok(None);
        }

        let dr = field!(self.regs, uartdr).read();

        let flags = DataRegister::from_bits_truncate(dr);

        if flags.contains(DataRegister::OE) {
            return Err(Error::Overrun);
        } else if flags.contains(DataRegister::BE) {
            return Err(Error::Break);
        } else if flags.contains(DataRegister::PE) {
            return Err(Error::Parity);
        } else if flags.contains(DataRegister::FE) {
            return Err(Error::Framing);
        }

        Ok(Some(dr as u8))
    }

    /// Non-blocking write of a single byte to the UART
    pub fn write_word(&mut self, word: u8) {
        field!(self.regs, uartdr).write(word as u32);
    }

    /// Read UART peripheral identification structure
    pub fn read_identification(&self) -> Identification {
        // SAFETY: The caller of UniqueMmioPointer::new promised that it wraps a valid and unique
        // register block.
        let id: [u32; 4] = {
            [
                field_shared!(self.regs, uartperiphid0).read(),
                field_shared!(self.regs, uartperiphid1).read(),
                field_shared!(self.regs, uartperiphid2).read(),
                field_shared!(self.regs, uartperiphid3).read(),
            ]
        };

        Identification {
            part_number: (id[0] & 0xff) as u16 | ((id[1] & 0x0f) << 8) as u16,
            designer: ((id[1] & 0xf0) >> 4) as u8 | ((id[2] & 0x0f) << 4) as u8,
            revision_number: ((id[2] & 0xf0) >> 4) as u8,
            configuration: (id[3] & 0xff) as u8,
        }
    }

    fn calculate_baud_rate_divisor(baud_rate: u32, sysclk: u32) -> Result<(u32, u32), Error> {
        // baud_div = sysclk / (baud_rate * 16)
        // baud_div_bits = (baud_div * 2^7 + 1) / 2
        // After simplifying:
        // baud_div_bits = ((sysclk * 8 / baud_rate) + 1) / 2
        let baud_div = sysclk
            .checked_mul(8)
            .and_then(|clk| clk.checked_div(baud_rate))
            .ok_or(Error::InvalidParameter)?;
        let baud_div_bits = baud_div
            .checked_add(1)
            .map(|div| div >> 1)
            .ok_or(Error::InvalidParameter)?;

        let ibrd = baud_div_bits >> 6;
        let fbrd = baud_div_bits & 0x3F;

        if ibrd == 0 || (ibrd == 0xffff && fbrd != 0) || ibrd > 0xffff {
            return Err(Error::InvalidParameter);
        }

        Ok((ibrd, fbrd))
    }

    /// Sets trigger levels for RX and TX interrupts.
    /// The interrupts are generated when the fill level progresses through the trigger level.
    pub fn set_interrupt_fifo_levels(&mut self, rx_level: FifoLevel, tx_level: FifoLevel) {
        let fifo_levels = ((rx_level as u32) << 3) | tx_level as u32;

        field!(self.regs, uartifls).write(fifo_levels);
    }

    /// Reads the raw interrupt status register.
    pub fn raw_interrupt_status(&self) -> Interrupts {
        field_shared!(self.regs, uartris).read()
    }

    /// Reads the masked interrupt status register.
    pub fn masked_interrupt_status(&self) -> Interrupts {
        field_shared!(self.regs, uartmis).read()
    }

    /// Returns the current set of interrupt masks.
    pub fn interrupt_masks(&self) -> Interrupts {
        field_shared!(self.regs, uartimsc).read()
    }

    /// Sets the interrupt masks.
    pub fn set_interrupt_masks(&mut self, masks: Interrupts) {
        field!(self.regs, uartimsc).write(masks)
    }

    /// Clears the given set of interrupts.
    pub fn clear_interrupts(&mut self, interrupts: Interrupts) {
        field!(self.regs, uarticr).write(interrupts)
    }
}

// SAFETY: An `&Uart` only allows operations which read registers, which can safely be done from
// multiple threads simultaneously.
unsafe impl Sync for Uart<'_> {}

impl fmt::Write for Uart<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.as_bytes() {
            // Wait until there is room in the TX buffer.
            while self.is_tx_fifo_full() {}
            self.write_word(*byte);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::transmute_mut;

    pub struct FakePL011Registers {
        regs: [u32; 1024],
    }

    impl FakePL011Registers {
        pub fn new() -> Self {
            Self { regs: [0u32; 1024] }
        }

        pub fn clear(&mut self) {
            self.regs.fill(0);
        }

        pub fn reg_write(&mut self, offset: usize, value: u32) {
            self.regs[offset / 4] = value;
        }

        pub fn reg_read(&self, offset: usize) -> u32 {
            self.regs[offset / 4]
        }

        fn get(&mut self) -> UniqueMmioPointer<'_, PL011Registers> {
            UniqueMmioPointer::from(transmute_mut!(&mut self.regs))
        }

        pub fn uart_for_test(&mut self) -> Uart<'_> {
            Uart::new(self.get())
        }
    }

    #[test]
    fn regs_size() {
        assert_eq!(core::mem::size_of::<PL011Registers>(), 0x1000);
    }

    #[test]
    fn enable_230400_8n1() {
        let mut regs = FakePL011Registers::new();
        let mut uart = regs.uart_for_test();
        let config = LineConfig {
            data_bits: DataBits::Bits8,
            parity: Parity::None,
            stop_bits: StopBits::One,
        };

        // Example 3-1 from PL011 TRM
        assert_eq!(Ok(()), uart.enable(config, 230400, 4_000_000));

        assert_eq!(0x00, regs.reg_read(0x004)); // UARTSR_ECR
        assert_eq!(1, regs.reg_read(0x024)); // UARTIBDR
        assert_eq!(5, regs.reg_read(0x028)); // UARTFBDR
        assert_eq!(0b01110000, regs.reg_read(0x02c)); // UARTLCR_H
        assert_eq!(0x0301, regs.reg_read(0x030)); // UARTCR
    }

    #[test]
    fn enable_example_baudrates() {
        // Table 3-9
        let mut regs = FakePL011Registers::new();

        {
            let mut uart = regs.uart_for_test();
            let config = LineConfig {
                data_bits: DataBits::Bits8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            };

            assert_eq!(Ok(()), uart.enable(config, 230400, 4_000_000));
            assert_eq!(0x1, regs.reg_read(0x024)); // UARTIBDR
            assert_eq!(0x5, regs.reg_read(0x028)); // UARTFBDR
        }

        regs.clear();

        {
            let mut uart = regs.uart_for_test();
            let config = LineConfig {
                data_bits: DataBits::Bits8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            };

            assert_eq!(Ok(()), uart.enable(config, 115200, 4_000_000));
            assert_eq!(0x2, regs.reg_read(0x024)); // UARTIBDR
            assert_eq!(0xb, regs.reg_read(0x028)); // UARTFBDR
        }

        regs.clear();

        {
            let mut uart = regs.uart_for_test();
            let config = LineConfig {
                data_bits: DataBits::Bits8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            };

            assert_eq!(Ok(()), uart.enable(config, 76800, 4_000_000));
            assert_eq!(0x3, regs.reg_read(0x024)); // UARTIBDR
            assert_eq!(0x10, regs.reg_read(0x028)); // UARTFBDR
        }

        regs.clear();

        {
            let mut uart = regs.uart_for_test();
            let config = LineConfig {
                data_bits: DataBits::Bits8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            };

            assert_eq!(Ok(()), uart.enable(config, 38400, 4_000_000));
            assert_eq!(0x6, regs.reg_read(0x024)); // UARTIBDR
            assert_eq!(0x21, regs.reg_read(0x028)); // UARTFBDR
        }

        regs.clear();

        {
            let mut uart = regs.uart_for_test();
            let config = LineConfig {
                data_bits: DataBits::Bits8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            };

            assert_eq!(Ok(()), uart.enable(config, 14400, 4_000_000));
            assert_eq!(0x11, regs.reg_read(0x024)); // UARTIBDR
            assert_eq!(0x17, regs.reg_read(0x028)); // UARTFBDR
        }

        regs.clear();

        {
            let mut uart = regs.uart_for_test();
            let config = LineConfig {
                data_bits: DataBits::Bits8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            };

            assert_eq!(Ok(()), uart.enable(config, 2400, 4_000_000));
            assert_eq!(0x68, regs.reg_read(0x024)); // UARTIBDR
            assert_eq!(0xb, regs.reg_read(0x028)); // UARTFBDR
        }

        regs.clear();

        {
            let mut uart = regs.uart_for_test();
            let config = LineConfig {
                data_bits: DataBits::Bits8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            };

            assert_eq!(Ok(()), uart.enable(config, 110, 4_000_000));
            assert_eq!(0x8e0, regs.reg_read(0x024)); // UARTIBDR
            assert_eq!(0x2f, regs.reg_read(0x028)); // UARTFBDR
        }
    }

    #[test]
    fn enable_invalid_baudrates() {
        let mut regs = FakePL011Registers::new();
        let mut uart = regs.uart_for_test();

        {
            let config = LineConfig {
                data_bits: DataBits::Bits8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            };

            assert_eq!(
                Err(Error::InvalidParameter),
                uart.enable(config, 0, 4_000_000)
            );
        }

        {
            let config = LineConfig {
                data_bits: DataBits::Bits8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            };
            assert_eq!(
                Err(Error::InvalidParameter),
                uart.enable(config, 1, 1048561)
            );
        }

        {
            let config = LineConfig {
                data_bits: DataBits::Bits8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            };
            assert_eq!(
                Err(Error::InvalidParameter),
                uart.enable(config, 1, 100_000_000)
            );
        }

        {
            let config = LineConfig {
                data_bits: DataBits::Bits8,
                parity: Parity::None,
                stop_bits: StopBits::One,
            };
            assert_eq!(Err(Error::InvalidParameter), uart.enable(config, 1, 1));
        }
    }

    #[test]
    fn enable_lineconfigs() {
        let mut regs = FakePL011Registers::new();
        {
            // 8 bits, even parity, 2 stop bits
            let mut uart = regs.uart_for_test();
            let config = LineConfig {
                data_bits: DataBits::Bits7,
                parity: Parity::Even,
                stop_bits: StopBits::Two,
            };

            assert_eq!(Ok(()), uart.enable(config, 230400, 4_000_000));
            assert_eq!(0b01011110, regs.reg_read(0x02c)); // UARTLCR_H
        }

        regs.clear();

        {
            // 6 bits, odd parity, 1 stop bit
            let mut regs = FakePL011Registers::new();
            let mut uart = regs.uart_for_test();
            let config = LineConfig {
                data_bits: DataBits::Bits6,
                parity: Parity::Odd,
                stop_bits: StopBits::One,
            };

            assert_eq!(Ok(()), uart.enable(config, 230400, 4_000_000));
            assert_eq!(0b00110010, regs.reg_read(0x02c)); // UARTLCR_H
        }

        regs.clear();

        {
            // 5 bits, one parity, 1 stop bit
            let mut regs = FakePL011Registers::new();
            let mut uart = regs.uart_for_test();
            let config = LineConfig {
                data_bits: DataBits::Bits5,
                parity: Parity::One,
                stop_bits: StopBits::One,
            };

            assert_eq!(Ok(()), uart.enable(config, 230400, 4_000_000));
            assert_eq!(0b10010010, regs.reg_read(0x02c)); // UARTLCR_H
        }

        {
            // 5 bits, zero paraty, 2 stop bit
            let mut regs = FakePL011Registers::new();
            let mut uart = regs.uart_for_test();
            let config = LineConfig {
                data_bits: DataBits::Bits5,
                parity: Parity::Zero,
                stop_bits: StopBits::Two,
            };

            assert_eq!(Ok(()), uart.enable(config, 230400, 4_000_000));
            assert_eq!(0b10011110, regs.reg_read(0x02c)); // UARTLCR_H
        }
    }

    #[test]
    fn disable() {
        let mut regs = FakePL011Registers::new();
        let mut uart = regs.uart_for_test();
        let config = LineConfig {
            data_bits: DataBits::Bits8,
            parity: Parity::None,
            stop_bits: StopBits::One,
        };

        assert_eq!(Ok(()), uart.enable(config, 230400, 4_000_000));
        uart.disable();
        assert_eq!(0, regs.reg_read(0x030)); // UARTCR
    }

    #[test]
    fn rx_fifo_empty() {
        let mut regs = FakePL011Registers::new();
        {
            let uart = regs.uart_for_test();
            assert!(!uart.is_rx_fifo_empty());
        }

        {
            regs.reg_write(0x018, 1 << 4);
            let uart = regs.uart_for_test();
            assert!(uart.is_rx_fifo_empty());
        }
    }

    #[test]
    fn rx_fifo_full() {
        let mut regs = FakePL011Registers::new();
        {
            let uart = regs.uart_for_test();
            assert!(!uart.is_rx_fifo_full());
        }

        {
            regs.reg_write(0x018, 1 << 6);
            let uart = regs.uart_for_test();
            assert!(uart.is_rx_fifo_full());
        }
    }

    #[test]
    fn tx_fifo_empty() {
        let mut regs = FakePL011Registers::new();
        {
            let uart = regs.uart_for_test();
            assert!(!uart.is_tx_fifo_empty());
        }

        {
            regs.reg_write(0x018, 1 << 7);
            let uart = regs.uart_for_test();
            assert!(uart.is_tx_fifo_empty());
        }
    }

    #[test]
    fn tx_fifo_full() {
        let mut regs = FakePL011Registers::new();
        {
            let uart = regs.uart_for_test();
            assert!(!uart.is_tx_fifo_full());
        }

        {
            regs.reg_write(0x018, 1 << 5);
            let uart = regs.uart_for_test();
            assert!(uart.is_tx_fifo_full());
        }
    }

    #[test]
    fn busy() {
        let mut regs = FakePL011Registers::new();
        {
            let uart = regs.uart_for_test();
            assert!(!uart.is_busy());
        }

        {
            regs.reg_write(0x018, 1 << 3);
            let uart = regs.uart_for_test();
            assert!(uart.is_busy());
        }
    }

    #[test]
    fn read_word() {
        let mut regs = FakePL011Registers::new();

        {
            regs.reg_write(0x000, 1 << 11);

            let mut uart = regs.uart_for_test();
            assert_eq!(Err(Error::Overrun), uart.read_word());
        }

        {
            regs.reg_write(0x000, 1 << 10);

            let mut uart = regs.uart_for_test();
            assert_eq!(Err(Error::Break), uart.read_word());
        }

        {
            regs.reg_write(0x000, 1 << 9);

            let mut uart = regs.uart_for_test();
            assert_eq!(Err(Error::Parity), uart.read_word());
        }

        {
            regs.reg_write(0x000, 1 << 8);

            let mut uart = regs.uart_for_test();
            assert_eq!(Err(Error::Framing), uart.read_word());
        }

        {
            regs.reg_write(0x000, 0x41);

            let mut uart = regs.uart_for_test();
            assert_eq!(Ok(Some(0x41)), uart.read_word());
        }

        {
            regs.reg_write(0x018, 0x10);

            let mut uart = regs.uart_for_test();
            assert_eq!(Ok(None), uart.read_word());
        }
    }

    #[test]
    fn write_word() {
        let mut regs = FakePL011Registers::new();

        let mut uart = regs.uart_for_test();
        uart.write_word(0x41);

        assert_eq!(0x41, regs.reg_read(0x000));
    }

    #[test]
    fn read_identification() {
        let mut regs = FakePL011Registers::new();

        regs.reg_write(0xfe0, 0x11);
        regs.reg_write(0xfe4, 0x10);
        regs.reg_write(0xfe8, 0x34);
        regs.reg_write(0xfec, 0x00);

        let uart = regs.uart_for_test();
        let identification = uart.read_identification();
        assert_eq!(0x0011, identification.part_number);
        assert_eq!(0x41, identification.designer);
        assert_eq!(0x03, identification.revision_number);
        assert_eq!(0x00, identification.configuration);
        assert!(identification.is_valid());
    }

    #[test]
    fn fifo_level() {
        let mut regs = FakePL011Registers::new();

        {
            let mut uart = regs.uart_for_test();
            uart.set_interrupt_fifo_levels(FifoLevel::Bytes4, FifoLevel::Bytes8);
        }
        assert_eq!(regs.reg_read(0x34), 0x01);

        {
            let mut uart = regs.uart_for_test();
            uart.set_interrupt_fifo_levels(FifoLevel::Bytes8, FifoLevel::Bytes16);
        }
        assert_eq!(regs.reg_read(0x34), 0x0a);

        {
            let mut uart = regs.uart_for_test();
            uart.set_interrupt_fifo_levels(FifoLevel::Bytes16, FifoLevel::Bytes24);
        }
        assert_eq!(regs.reg_read(0x34), 0x13);

        {
            let mut uart = regs.uart_for_test();
            uart.set_interrupt_fifo_levels(FifoLevel::Bytes24, FifoLevel::Bytes28);
        }
        assert_eq!(regs.reg_read(0x34), 0x1c);

        {
            let mut uart = regs.uart_for_test();
            uart.set_interrupt_fifo_levels(FifoLevel::Bytes28, FifoLevel::Bytes4);
        }
        assert_eq!(regs.reg_read(0x34), 0x20);
    }

    #[test]
    fn interrupt_status() {
        let mut regs = FakePL011Registers::new();

        {
            let uart = regs.uart_for_test();
            assert_eq!(uart.raw_interrupt_status(), Interrupts::empty());
            assert_eq!(uart.masked_interrupt_status(), Interrupts::empty());
        }

        {
            regs.reg_write(0x3c, 0b0000_0110_0000_1001);
            regs.reg_write(0x40, 0b0000_0100_0000_0001);
            let uart = regs.uart_for_test();
            assert_eq!(
                uart.raw_interrupt_status(),
                Interrupts::OEI | Interrupts::BEI | Interrupts::DSRMI | Interrupts::RIMI
            );
            assert_eq!(
                uart.masked_interrupt_status(),
                Interrupts::OEI | Interrupts::RIMI
            );
        }
    }

    #[test]
    fn interrupt_mask() {
        let mut regs = FakePL011Registers::new();

        {
            let mut uart = regs.uart_for_test();
            assert_eq!(uart.interrupt_masks(), Interrupts::empty());

            uart.set_interrupt_masks(Interrupts::BEI | Interrupts::RTI);
            assert_eq!(uart.interrupt_masks(), Interrupts::BEI | Interrupts::RTI);
        }

        assert_eq!(regs.reg_read(0x38), 0b0000_0010_0100_0000);
    }

    #[test]
    fn interrupt_clear() {
        let mut regs = FakePL011Registers::new();

        {
            let mut uart = regs.uart_for_test();
            uart.clear_interrupts(Interrupts::OEI | Interrupts::RIMI | Interrupts::RTI);
        }

        assert_eq!(regs.reg_read(0x44), 0b0000_0100_0100_0001);
    }

    #[test]
    fn core_write() {
        let mut regs = FakePL011Registers::new();
        let mut uart = regs.uart_for_test();
        assert_eq!(Ok(()), core::fmt::Write::write_str(&mut uart, "hello"));
    }
}
