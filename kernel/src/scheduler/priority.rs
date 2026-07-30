//! 四核静态优先级抢占调度器。
//!
//! 每个CPU维护64位Ready bitmap。线程的`ready_sequence`表达同优先级FIFO
//! 次序；高优先级位优先，同级时间片到期后更新序号并排到队尾。

use crate::{thread, trap::TrapFrame};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct CpuScheduler {
    ready_bitmap: AtomicU64,
    active_start: AtomicU64,
    initialized: AtomicBool,
}

impl CpuScheduler {
    const fn new() -> Self {
        Self {
            ready_bitmap: AtomicU64::new(0),
            active_start: AtomicU64::new(0),
            initialized: AtomicBool::new(false),
        }
    }

    fn reset(&self) {
        self.ready_bitmap.store(0, Ordering::Release);
        self.active_start.store(0, Ordering::Release);
        self.initialized.store(false, Ordering::Release);
    }
}

static CPUS: [CpuScheduler; exo_abi::MAX_CPUS] = [
    CpuScheduler::new(),
    CpuScheduler::new(),
    CpuScheduler::new(),
    CpuScheduler::new(),
];
static READY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn cpu() -> usize {
    crate::arch::aarch64::cpu::id()
}

pub fn init(timer_intid: u32) {
    let cpu = cpu();
    assert!(
        super::timer::init(timer_intid),
        "Generic Timer initialization failed"
    );
    CPUS[cpu].initialized.store(true, Ordering::Release);
    CPUS[cpu]
        .active_start
        .store(super::timer::arm_full(), Ordering::Release);
}

pub fn on_thread_ready(slot: usize) {
    let target_cpu = thread::affinity(slot);
    let sequence = READY_SEQUENCE.fetch_add(1, Ordering::AcqRel);
    thread::set_ready_sequence(slot, sequence.max(1));
    // Release保证上面对Thread context/state/sequence的写入先于目标CPU
    // 观察到Ready位；next_runnable的Acquire读取与之配对。
    CPUS[target_cpu]
        .ready_bitmap
        .fetch_or(1u64 << thread::effective_priority(slot), Ordering::Release);
    // 已经在EL1等待队列的CPU不需要制造一次GIC异常；SEV会让所有WFE核
    // 醒来重扫自己的Ready Queue。仍在EL0运行的目标核则由下面SGI0强制
    // 进入异常入口，以实现高优先级立即抢占。
    unsafe { core::arch::asm!("sev", options(nomem, nostack)) };
    if target_cpu != cpu() {
        let running = thread::current_slot_on(target_cpu);
        if running < thread::MAX_THREADS
            && thread::is_running(running)
            && thread::effective_priority(slot) > thread::effective_priority(running)
        {
            crate::arch::aarch64::smp::send_reschedule(target_cpu);
        }
    }
}

fn refresh_priority(cpu: usize, priority: u8) {
    let mut present = false;
    let mut slot = 0usize;
    while slot < thread::MAX_THREADS {
        if thread::is_ready(slot)
            && thread::affinity(slot) == cpu
            && thread::effective_priority(slot) == priority
        {
            present = true;
            break;
        }
        slot += 1;
    }
    if present {
        CPUS[cpu]
            .ready_bitmap
            .fetch_or(1u64 << priority, Ordering::AcqRel);
    } else {
        CPUS[cpu]
            .ready_bitmap
            .fetch_and(!(1u64 << priority), Ordering::AcqRel);
    }
}

fn next_runnable(cpu: usize) -> Option<usize> {
    loop {
        let bitmap = CPUS[cpu].ready_bitmap.load(Ordering::Acquire);
        if bitmap == 0 {
            return None;
        }
        let priority = (63 - bitmap.leading_zeros()) as u8;
        let mut selected = None;
        let mut selected_sequence = u64::MAX;
        let mut slot = 0usize;
        while slot < thread::MAX_THREADS {
            if thread::is_ready(slot)
                && thread::affinity(slot) == cpu
                && thread::effective_priority(slot) == priority
                && thread::ready_sequence(slot) < selected_sequence
            {
                selected = Some(slot);
                selected_sequence = thread::ready_sequence(slot);
            }
            slot += 1;
        }
        if selected.is_some() {
            return selected;
        }
        refresh_priority(cpu, priority);
    }
}

fn charge_current() {
    let cpu = cpu();
    let slot = thread::current_slot();
    if slot < thread::MAX_THREADS && thread::is_running(slot) {
        let now = super::timer::counter();
        let start = CPUS[cpu].active_start.load(Ordering::Acquire);
        thread::charge(slot, now.wrapping_sub(start));
    }
}

fn activate(slot: usize) -> *mut TrapFrame {
    let cpu = cpu();
    if slot >= thread::MAX_THREADS || thread::saved_pc(slot) == 0 {
        crate::uart::puts("[exo] scheduler rejected invalid context cpu=");
        crate::uart::hex(cpu as u64);
        crate::uart::puts(" slot=");
        crate::uart::hex(slot as u64);
        crate::uart::puts(" pc=");
        crate::uart::hex(if slot < thread::MAX_THREADS {
            thread::saved_pc(slot)
        } else {
            0
        });
        crate::uart::puts("\r\n");
        loop {
            unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
        }
    }
    refresh_priority(cpu, thread::effective_priority(slot));
    CPUS[cpu]
        .active_start
        .store(super::timer::arm_full(), Ordering::Release);
    thread::activate(slot)
}

fn wait_and_activate() -> *mut TrapFrame {
    loop {
        if let Some(slot) = next_runnable(cpu()) {
            return activate(slot);
        }
        // 空Ready Queue也保留1ms本地Timer。正常情况下SGI0会立即唤醒
        // 目标核；若平台GIC丢失重复SGI，Timer仍会让本核重新扫描队列，
        // 把远程唤醒延迟限制在一个时间片内，而不是永久睡眠。
        CPUS[cpu()]
            .active_start
            .store(super::timer::arm_full(), Ordering::Release);
        unsafe {
            core::arch::asm!(
                "msr daifclr, #2",
                // WFE既能被SEV唤醒，也不会丢失先于休眠到达的event；
                // 返回后重新检查Ready bitmap，空队列则再次等待。
                "wfe",
                "msr daifset, #2",
                options(nomem, nostack)
            );
        }
    }
}

pub fn idle_loop() -> ! {
    loop {
        if let Some(slot) = next_runnable(cpu()) {
            let frame = activate(slot);
            // 当前核尚未从EL0异常进入Kernel，不存在可由异常尾声恢复的
            // 临时TrapFrame；使用专用汇编入口恢复Thread表内保存的上下文。
            unsafe {
                crate::arch::aarch64::vectors::enter_saved_el0(frame);
            }
        }
        // Ready Queue为空时关闭本地时间片。远程线程创建/唤醒先发布Ready
        // 状态再执行SEV，所以即便事件早于WFE也会保留event register，
        // 不会发生“检查为空后永久睡眠”的丢失唤醒。
        super::timer::disarm();
        unsafe {
            core::arch::asm!(
                "msr daifclr, #2",
                "wfe",
                "msr daifset, #2",
                options(nomem, nostack)
            );
        }
    }
}

pub fn activate_if_idle(frame: &TrapFrame) -> *mut TrapFrame {
    match next_runnable(cpu()) {
        Some(slot) => activate(slot),
        None => frame as *const TrapFrame as *mut TrapFrame,
    }
}

pub fn schedule_after_block() -> *mut TrapFrame {
    charge_current();
    super::timer::disarm();
    wait_and_activate()
}

pub fn on_timer(frame: &TrapFrame) -> *mut TrapFrame {
    charge_current();
    thread::make_current_ready(frame);
    wait_and_activate()
}

pub fn yield_current(frame: &TrapFrame) -> *mut TrapFrame {
    charge_current();
    thread::make_current_ready(frame);
    wait_and_activate()
}

pub fn priority_changed(slot: usize) {
    if thread::is_ready(slot) {
        on_thread_ready(slot);
    } else if thread::is_running(slot) {
        crate::arch::aarch64::smp::send_reschedule(thread::affinity(slot));
    }
}

/// 返回线程尚未写回`runtime_ticks`的当前运行区间。
///
/// 调度切换时`charge_current`会把该区间并入Thread表；如果用户在切换前
/// 查询正在运行的线程，仍需把`now - active_start`加入快照，否则一个
/// 持续占用CPU但尚未被抢占的线程会错误报告0 ticks。
pub fn active_runtime_ticks(slot: usize) -> u64 {
    if !thread::is_running(slot) {
        return 0;
    }
    let running_cpu = thread::running_cpu(slot);
    if running_cpu >= exo_abi::MAX_CPUS {
        return 0;
    }
    let start = CPUS[running_cpu].active_start.load(Ordering::Acquire);
    if start == 0 {
        0
    } else {
        super::timer::counter().wrapping_sub(start)
    }
}

pub fn on_thread_exit(_slot: usize) {}

pub fn preempt_if_needed(frame: &TrapFrame) -> *mut TrapFrame {
    let Some(next) = next_runnable(cpu()) else {
        return frame as *const TrapFrame as *mut TrapFrame;
    };
    if thread::effective_priority(next) <= thread::current_priority() {
        return frame as *const TrapFrame as *mut TrapFrame;
    }
    charge_current();
    thread::make_current_ready(frame);
    activate(next)
}

pub fn cleanup_owner(_owner: u32) {
    super::timer::disarm();
    for scheduler in &CPUS {
        scheduler.reset();
    }
    READY_SEQUENCE.store(1, Ordering::Release);
}
