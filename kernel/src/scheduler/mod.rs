//! 每核静态优先级抢占调度器。

pub mod priority;
pub mod timer;

pub use priority::{
    init, on_thread_exit, on_thread_ready, on_timer, preempt_if_needed, priority_changed,
    schedule_after_block, yield_current,
};
