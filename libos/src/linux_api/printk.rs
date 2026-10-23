// SPDX-License-Identifier: GPL-2.0-or-later
//! 同名诊断接口和实例独立的限流器；输出由调用者注入，不隐式依赖网卡全局状态。
use core::fmt::Arguments;
pub trait LogSink {
    fn write(&mut self, level: &str, device: &str, message: Arguments<'_>);
}
pub fn dev_dbg(out: &mut impl LogSink, dev: &str, msg: Arguments<'_>) {
    out.write("debug", dev, msg);
}
pub fn dev_err(out: &mut impl LogSink, dev: &str, msg: Arguments<'_>) {
    out.write("error", dev, msg);
}
pub fn netdev_err(out: &mut impl LogSink, dev: &str, msg: Arguments<'_>) {
    dev_err(out, dev, msg);
}
pub fn netdev_info(out: &mut impl LogSink, dev: &str, msg: Arguments<'_>) {
    out.write("info", dev, msg);
}
pub struct RateLimit {
    /// 调用者时钟单位中的窗口起点和持续时间，不声称是 Linux 全局限流参数。
    start: u64,
    interval: u64,
    /// 每窗口预算和已使用次数，仅由所属线程修改。
    burst: u32,
    used: u32,
}
impl RateLimit {
    pub fn new(interval: u64, burst: u32) -> Self {
        Self {
            start: 0,
            interval,
            burst,
            used: 0,
        }
    }
}
pub fn net_ratelimit(limit: &mut RateLimit, now: u64) -> bool {
    if now.wrapping_sub(limit.start) >= limit.interval {
        limit.start = now;
        limit.used = 0;
    }
    if limit.used >= limit.burst {
        return false;
    }
    limit.used += 1;
    true
}
