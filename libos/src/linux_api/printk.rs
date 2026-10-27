// SPDX-License-Identifier: GPL-2.0-or-later
//! 同名日志与全局限流；可变参数以 Rust 宏表达，调用处仍传设备、格式和原参数。

use super::{kernel, netdevice::net_device, platform::device};
use core::{
    fmt::{self, Write},
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
// 错误日志不再临时申请堆内存，避免分配失败路径再次依赖分配器。
struct LogBuffer {
    bytes: [u8; 512],
    used: usize,
}
impl LogBuffer {
    fn flush(&mut self) {
        if self.used != 0 {
            // write_str 只在字符边界截断，当前片段始终是 UTF-8。
            (kernel::ops().log)(unsafe {
                core::str::from_utf8_unchecked(&self.bytes[..self.used])
            });
            self.used = 0;
        }
    }
}
impl fmt::Write for LogBuffer {
    fn write_str(&mut self, mut text: &str) -> fmt::Result {
        while !text.is_empty() {
            let mut end = (512 - self.used).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == 0 {
                self.flush();
                continue;
            }
            self.bytes[self.used..self.used + end].copy_from_slice(text[..end].as_bytes());
            self.used += end;
            text = &text[end..];
            if self.used == 512 {
                self.flush();
            }
        }
        Ok(())
    }
}
static LOG_LOCK: super::lock::Lock<()> = super::lock::Lock::new(());
fn emit(level: &str, name: &str, args: fmt::Arguments<'_>) {
    let _guard = loop {
        if let Some(g) = LOG_LOCK.try_lock() {
            break g;
        }
        (kernel::ops().yield_now)();
    };
    let mut buf = LogBuffer {
        bytes: [0; 512],
        used: 0,
    };
    let _ = write!(&mut buf, "[{}] {}: {}", level, name, args);
    buf.flush();
}
pub unsafe fn netlog(dev: *const net_device, level: &str, args: fmt::Arguments<'_>) {
    emit(level, (*dev).name.as_str(), args);
}
pub unsafe fn devlog(dev: *const device, level: &str, args: fmt::Arguments<'_>) {
    emit(level, (*dev).name, args);
}
#[macro_export]
macro_rules! netdev_err{($dev:expr,$fmt:expr $(,$args:expr)* $(,)?)=>{{$crate::linux_api::printk::netlog($dev,"error",format_args!($fmt $(,$args)*));}};}
#[macro_export]
macro_rules! netdev_info{($dev:expr,$fmt:expr $(,$args:expr)* $(,)?)=>{{$crate::linux_api::printk::netlog($dev,"info",format_args!($fmt $(,$args)*));}};}
#[macro_export]
macro_rules! dev_err{($dev:expr,$fmt:expr $(,$args:expr)* $(,)?)=>{{$crate::linux_api::printk::devlog($dev,"error",format_args!($fmt $(,$args)*));}};}
#[macro_export]
macro_rules! dev_dbg{($dev:expr,$fmt:expr $(,$args:expr)* $(,)?)=>{{if $crate::linux_api::printk::debug_enabled() {$crate::linux_api::printk::devlog($dev,"debug",format_args!($fmt $(,$args)*));}}};}
static START: AtomicU64 = AtomicU64::new(0);
static USED: AtomicU32 = AtomicU32::new(0);
pub fn net_ratelimit() -> i32 {
    let now = (kernel::ops().ticks)();
    let interval = (kernel::ops().frequency)().saturating_mul(5);
    let start = START.load(Ordering::Acquire);
    if now.wrapping_sub(start) >= interval
        && START
            .compare_exchange(start, now, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    {
        USED.store(0, Ordering::Release);
    }
    (USED.fetch_add(1, Ordering::AcqRel) < 10) as i32
}

// 对应未启用 DEBUG/动态调试的默认配置；启用后才求值并输出 dev_dbg 参数。
static DEBUG_ENABLED: AtomicBool = AtomicBool::new(false);
pub fn debug_enabled() -> bool {
    DEBUG_ENABLED.load(Ordering::Acquire)
}
pub fn set_debug(enabled: bool) {
    DEBUG_ENABLED.store(enabled, Ordering::Release);
}
