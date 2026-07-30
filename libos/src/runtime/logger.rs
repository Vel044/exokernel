//! CrabUSB `log` facade到libOS UART输出的适配。
//!
//! 裸机环境没有stdout和堆格式化器，因此每条日志写入固定栈缓冲，再交给
//! runtime::puts。UART接管后puts直接执行PL011 MMIO写，不为日志陷入Kernel。

use core::fmt::{self, Write};

/// 无状态logger；所有临时格式化状态都放在调用栈。
struct Logger;

/// `log` crate要求logger具有`'static`生命周期。
static LOGGER: Logger = Logger;

/// 注册全局logger，并允许CrabUSB输出到Debug级别。
pub fn init() {
    // 重复初始化只会返回错误；USB任务忽略它可支持单任务和综合实验两条入口。
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
}

impl log::Log for Logger {
    /// 编译保留的最高级别之外，再在运行时过滤一次。
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // 缓冲区位于USB线程栈上，不使用LockedHeap，避免日志影响DMA/协议时序。
        let mut buffer = Buffer {
            bytes: [0; 512],
            length: 0,
        };
        // core::fmt通过下面的Write实现逐段写入；超长日志会安全截断。
        let _ = write!(buffer, "[crab-usb] {}\r\n", record.args());
        crate::runtime::puts(&buffer.bytes[..buffer.length]);
    }

    // PL011写入是同步轮询，不存在需要额外flush的用户态缓冲。
    fn flush(&self) {}
}

/// 固定容量格式化目标。
struct Buffer {
    bytes: [u8; 512],
    length: usize,
}

impl Write for Buffer {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        // saturating_sub防止length异常时发生usize下溢。
        let available = self.bytes.len().saturating_sub(self.length);
        // 只复制剩余容量，日志截断不应导致驱动panic。
        let count = available.min(value.len());
        self.bytes[self.length..self.length + count].copy_from_slice(&value.as_bytes()[..count]);
        self.length += count;
        Ok(())
    }
}
