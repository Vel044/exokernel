//! 单核协作式调度器。
//!
//! 调度策略保持简单且确定：优先级较高的 Ready 线程先运行，同优先级
//! 按进入 Ready 队列的顺序运行。时间片抢占留给后续 generic timer 阶段。

/// 从线程表取出下一个可运行线程。
pub fn next_ready() -> Option<usize> {
    crate::thread::take_next_ready()
}
