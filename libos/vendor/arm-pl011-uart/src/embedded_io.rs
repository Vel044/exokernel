// SPDX-FileCopyrightText: Copyright The arm-pl011-uart Contributors.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{Error, Uart};
use embedded_io::{ErrorKind, ErrorType, Read, ReadReady, Write, WriteReady};

impl ErrorType for Uart<'_> {
    type Error = Error;
}

impl embedded_io::Error for Error {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Break | Self::Overrun => ErrorKind::Other,
            Self::Framing | Self::Parity => ErrorKind::InvalidData,
            Self::InvalidParameter => ErrorKind::InvalidInput,
        }
    }
}

impl Write for Uart<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let mut bytes_written = 0;
        if !buf.is_empty() {
            // Wait until there is room in the TX buffer.
            while self.is_tx_fifo_full() {}

            // Write until the TX buffer is full or we run out of bytes to write. The caller will
            // take care of retrying until the full buffer is written.
            for byte in buf {
                self.write_word(*byte);
                bytes_written += 1;
                if self.is_tx_fifo_full() {
                    break;
                }
            }
        }
        Ok(bytes_written)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        while self.is_busy() {}
        Ok(())
    }
}

impl WriteReady for Uart<'_> {
    fn write_ready(&mut self) -> Result<bool, Self::Error> {
        Ok(!self.is_tx_fifo_full())
    }
}

impl Read for Uart<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if buf.is_empty() {
            Ok(0)
        } else {
            // Wait until a byte is available to read.
            loop {
                // Read a single byte. No need to wait for more, the caller will retry until it has
                // as many as it wants.
                if let Some(byte) = self.read_word()? {
                    buf[0] = byte;
                    return Ok(1);
                }
            }
        }
    }
}

impl ReadReady for Uart<'_> {
    fn read_ready(&mut self) -> Result<bool, Self::Error> {
        Ok(!self.is_rx_fifo_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::FakePL011Registers;

    #[test]
    fn error_kind() {
        assert_eq!(ErrorKind::Other, embedded_io::Error::kind(&Error::Break));

        assert_eq!(
            ErrorKind::InvalidData,
            embedded_io::Error::kind(&Error::Framing)
        );

        assert_eq!(
            ErrorKind::InvalidInput,
            embedded_io::Error::kind(&Error::InvalidParameter)
        );
    }

    #[test]
    fn embeddedio_write_empty() {
        let mut regs = FakePL011Registers::new();
        let mut uart = regs.uart_for_test();
        assert_eq!(Ok(0), Write::write(&mut uart, &[]));
        assert_eq!(Ok(()), Write::flush(&mut uart));
    }

    #[test]
    fn embeddedio_write() {
        let mut regs = FakePL011Registers::new();
        let mut uart = regs.uart_for_test();
        assert_eq!(Ok(2), Write::write(&mut uart, &[1, 2]));
        assert_eq!(Ok(()), Write::write_all(&mut uart, &[1, 2]));
        assert_eq!(Ok(()), Write::flush(&mut uart));
    }

    #[test]
    fn embeddedio_write_fifo_full() {
        let mut regs = FakePL011Registers::new();
        {
            let mut uart = regs.uart_for_test();
            assert_eq!(Ok(true), uart.write_ready());
        }

        {
            regs.reg_write(0x018, 1 << 5);
            let mut uart = regs.uart_for_test();
            assert_eq!(Ok(false), uart.write_ready());
        }
    }

    #[test]
    fn embeddedio_read_empty() {
        let mut regs = FakePL011Registers::new();
        let mut uart = regs.uart_for_test();
        let mut data = [];
        assert_eq!(Ok(0), Read::read(&mut uart, &mut data));
    }

    #[test]
    fn embeddedio_read() {
        let mut regs = FakePL011Registers::new();
        let mut uart = regs.uart_for_test();
        let mut data = [0u8; 2];
        assert_eq!(Ok(1), Read::read(&mut uart, &mut data));
        assert_eq!(data, [0, 0]);
        assert_eq!(Ok(()), Read::read_exact(&mut uart, &mut data));
        assert_eq!(data, [0, 0]);
    }

    #[test]
    fn embeddedio_read_fifo_empty() {
        let mut regs = FakePL011Registers::new();
        {
            let mut uart = regs.uart_for_test();
            assert_eq!(Ok(true), uart.read_ready());
        }

        {
            regs.reg_write(0x018, 1 << 4);
            let mut uart = regs.uart_for_test();
            assert_eq!(Ok(false), uart.read_ready());
        }
    }
}
