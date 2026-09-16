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

pub fn init(timer_intid: u32, use_virtual_timer: bool) {
    let cpu = cpu();
    assert!(
        super::timer::init(timer_intid, use_virtual_timer),
        "Generic Timer initialization failed"
    );
    CPUS[cpu].initialized.store(true, Ordering::Release);
    // 启动时每核至多有一个运行线程，不需要为了“与自己轮转”每毫秒进入
    // EL1。后续出现同优先级竞争者时，on_thread_ready会按需开启时间片。
    super::timer::disarm();
    CPUS[cpu]
        .active_start
        .store(super::timer::counter(), Ordering::Release);
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
    let running = thread::current_slot_on(target_cpu);
    if running < thread::MAX_THREADS && thread::is_running(running) {
        let ready_priority = thread::effective_priority(slot);
        let running_priority = thread::effective_priority(running);
        if target_cpu != cpu() && ready_priority >= running_priority {
            // 更高优先级线程需要立即抢占；同优先级线程需要让目标核开启
            // FIFO时间片。两种情况都用SGI0让远程核尽快重新判断。
            crate::arch::aarch64::smp::send_reschedule(target_cpu);
        } else if target_cpu == cpu() && ready_priority == running_priority {
            // 当前核在SVC/IRQ路径创建或唤醒了同级线程。先结算此前无时间片
            // 的运行区间，再启动1ms轮转；否则必须等到另一事件才会开timer。
            charge_current();
            CPUS[target_cpu]
                .active_start
                .store(super::timer::arm_full(), Ordering::Release);
        }
    }
}

/// 判断某CPU上是否还有一个与即将运行线程同优先级的Ready线程。
///
/// `selected`在调用`thread::activate`前仍标记为Ready，必须显式排除；否则
/// 调度器会把线程自己误判为竞争者，退化为永久1ms定时中断。
fn has_same_priority_competitor(cpu: usize, priority: u8, selected: usize) -> bool {
    let mut slot = 0usize;
    while slot < thread::MAX_THREADS {
        if slot != selected
            && thread::is_ready(slot)
            && thread::affinity(slot) == cpu
            && thread::effective_priority(slot) == priority
        {
            return true;
        }
        slot += 1;
    }
    false
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
    let start = if has_same_priority_competitor(cpu, thread::effective_priority(slot), slot) {
        // 同级竞争才需要1ms FIFO轮转。
        super::timer::arm_full()
    } else {
        // 独占当前最高优先级时关闭timer。新高优先级线程通过SGI0抢占，
        // 因此不会牺牲静态优先级响应时间。
        super::timer::disarm();
        super::timer::counter()
    };
    CPUS[cpu].active_start.store(start, Ordering::Release);
    thread::activate(slot)
}

fn wait_and_activate() -> *mut TrapFrame {
    loop {
        if let Some(slot) = next_runnable(cpu()) {
            return activate(slot);
        }
        // 空Ready Queue不需要时间片。线程创建/Notification/IPC唤醒会先
        // Release发布Ready状态，再执行SEV；即使SEV早于WFE，event register
        // 也会让WFE立即返回。依靠这个体系结构保证比每毫秒制造一次空转
        // timer IRQ更可靠，也避免HVF为四个idle核承担持续VM-exit。
        super::timer::disarm();
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
    if thread::effective_priority(next) < thread::current_priority() {
        return frame as *const TrapFrame as *mut TrapFrame;
    }
    if thread::effective_priority(next) == thread::current_priority() {
        // SGI0由同级线程变为Ready触发。当前线程继续跑到1ms期满即可，
        // 这里只开启时间片，不在SGI边界提前破坏同优先级FIFO顺序。
        charge_current();
        CPUS[cpu()]
            .active_start
            .store(super::timer::arm_full(), Ordering::Release);
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
