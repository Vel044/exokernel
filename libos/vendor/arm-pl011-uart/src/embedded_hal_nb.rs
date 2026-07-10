// SPDX-FileCopyrightText: Copyright The arm-pl011-uart Contributors.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{Error, Uart};
use embedded_hal_nb::{
    nb,
    serial::{self, ErrorKind, ErrorType, Read, Write},
};

impl ErrorType for Uart<'_> {
    type Error = Error;
}

impl serial::Error for Error {
    fn kind(&self) -> serial::ErrorKind {
        match self {
            Error::InvalidParameter => ErrorKind::Other,
            Error::Overrun => ErrorKind::Overrun,
            Error::Break => ErrorKind::Other,
            Error::Parity => ErrorKind::Parity,
            Error::Framing => ErrorKind::FrameFormat,
        }
    }
}

impl Write for Uart<'_> {
    fn write(&mut self, word: u8) -> nb::Result<(), Self::Error> {
        if self.is_tx_fifo_full() {
            return Err(nb::Error::WouldBlock);
        }

        self.write_word(word);

        Ok(())
    }

    fn flush(&mut self) -> nb::Result<(), Self::Error> {
        if self.is_busy() {
            Err(nb::Error::WouldBlock)
        } else {
            Ok(())
        }
    }
}

impl Read for Uart<'_> {
    fn read(&mut self) -> nb::Result<u8, Self::Error> {
        match self.read_word() {
            Ok(None) => Err(nb::Error::WouldBlock),
            Ok(Some(word)) => Ok(word),
            Err(err) => Err(nb::Error::Other(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::FakePL011Registers;

    #[test]
    fn error_kind() {
        assert_eq!(
            ErrorKind::Other,
            serial::Error::kind(&Error::InvalidParameter)
        );

        assert_eq!(
            serial::ErrorKind::Overrun,
            serial::Error::kind(&Error::Overrun)
        );

        assert_eq!(ErrorKind::Other, serial::Error::kind(&Error::Break));

        assert_eq!(ErrorKind::Parity, serial::Error::kind(&Error::Parity));

        assert_eq!(ErrorKind::FrameFormat, serial::Error::kind(&Error::Framing));
    }

    #[test]
    fn serial_write() {
        let mut regs = FakePL011Registers::new();

        {
            let mut uart = regs.uart_for_test();
            assert_eq!(Ok(()), Write::write(&mut uart, 0x41));
            assert_eq!(0x41, regs.reg_read(0x000));
        }

        regs.clear();

        {
            regs.reg_write(0x018, 1 << 5);
            let mut uart = regs.uart_for_test();
            assert_eq!(Err(nb::Error::WouldBlock), Write::write(&mut uart, 0x41));
        }

        regs.clear();

        {
            let mut uart = regs.uart_for_test();
            assert_eq!(Ok(()), Write::flush(&mut uart));
        }
        regs.clear();

        {
            regs.reg_write(0x018, 1 << 3);
            let mut uart = regs.uart_for_test();
            assert_eq!(Err(nb::Error::WouldBlock), Write::flush(&mut uart));
        }
    }

    #[test]
    fn serial_read() {
        let mut regs = FakePL011Registers::new();

        {
            regs.reg_write(0x000, 0x41);

            let mut uart = regs.uart_for_test();
            assert_eq!(Ok(0x41), Read::read(&mut uart));
        }

        regs.clear();

        {
            regs.reg_write(0x000, 0x41);

            let mut uart = regs.uart_for_test();
            assert_eq!(Ok(0x41), Read::read(&mut uart));
        }

        regs.clear();

        {
            regs.reg_write(0x000, 1 << 11);

            let mut uart = regs.uart_for_test();
            assert_eq!(Err(nb::Error::Other(Error::Overrun)), Read::read(&mut uart));
        }

        regs.clear();

        {
            regs.reg_write(0x018, 1 << 4);

            let mut uart = regs.uart_for_test();
            assert_eq!(Err(nb::Error::WouldBlock), Read::read(&mut uart));
        }
    }
}
