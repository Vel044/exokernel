//! SMP 内核同步原语。
//!
//! 这里的锁只保护很短的内核元数据临界区。持锁期间关闭当前 CPU 的 IRQ，
//! 防止本核中断处理再次获取同一把锁；其他 CPU 使用原子交换等待。调度、
//! WFE、IPC 阻塞和用户态返回都不得发生在锁保护范围内。

mod spin;

pub(crate) use spin::SpinLock;
