use core::fmt::{self, Write};

struct Logger;

static LOGGER: Logger = Logger;

pub fn init() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let mut buffer = Buffer {
            bytes: [0; 512],
            length: 0,
        };
        let _ = write!(buffer, "[crab-usb] {}\r\n", record.args());
        crate::runtime::puts(&buffer.bytes[..buffer.length]);
    }

    fn flush(&self) {}
}

struct Buffer {
    bytes: [u8; 512],
    length: usize,
}

impl Write for Buffer {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let available = self.bytes.len().saturating_sub(self.length);
        let count = available.min(value.len());
        self.bytes[self.length..self.length + count].copy_from_slice(&value.as_bytes()[..count]);
        self.length += count;
        Ok(())
    }
}
