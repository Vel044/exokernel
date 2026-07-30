//! 四核静态优先级与CPU亲和性烟雾测试。

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static RUN: AtomicBool = AtomicBool::new(false);
static DONE: AtomicU64 = AtomicU64::new(0);
static CPU_MASK: AtomicU64 = AtomicU64::new(0);
static COUNTERS: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

static RR_RUN: AtomicBool = AtomicBool::new(false);
static RR_STARTED: AtomicU64 = AtomicU64::new(0);
static RR_COUNTERS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

static PREEMPT_NOTIFICATION: AtomicU64 = AtomicU64::new(0);
static HIGH_WAITING: AtomicBool = AtomicBool::new(false);
static HIGH_RAN: AtomicBool = AtomicBool::new(false);
static LOW_RUN: AtomicBool = AtomicBool::new(false);
static LOW_COUNTER: AtomicU64 = AtomicU64::new(0);
static HIGH_OBSERVED_LOW_COUNTER: AtomicU64 = AtomicU64::new(0);

static PI_ENDPOINT: AtomicU64 = AtomicU64::new(0);
static PI_SERVER_WAITING: AtomicBool = AtomicBool::new(false);
static PI_CLIENT_DONE: AtomicBool = AtomicBool::new(false);
static PI_MEDIUM_RUN: AtomicBool = AtomicBool::new(false);
static PI_MEDIUM_COUNTER: AtomicU64 = AtomicU64::new(0);

extern "C" fn affinity_worker(arg: u64, _handle: u64, _ipc: u64) -> ! {
    let expected = arg as usize;
    let actual = crate::thread::current_cpu();
    if actual == expected {
        CPU_MASK.fetch_or(1u64 << actual, Ordering::AcqRel);
    }
    while RUN.load(Ordering::Acquire) {
        COUNTERS[expected].fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
    DONE.fetch_or(1u64 << expected, Ordering::AcqRel);
    crate::thread::exit(0)
}

/// 两个实例固定在CPU1且优先级相同。它们都不主动yield，只能依靠1ms
/// Generic Timer把队首移到同级FIFO队尾。
extern "C" fn rr_worker(index: u64, _handle: u64, _ipc: u64) -> ! {
    RR_STARTED.fetch_or(1u64 << index, Ordering::AcqRel);
    while RR_RUN.load(Ordering::Acquire) {
        RR_COUNTERS[index as usize].fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
    crate::thread::exit(0)
}

extern "C" fn high_waiter(_arg: u64, _handle: u64, _ipc: u64) -> ! {
    let notification =
        crate::notification::Notification::from_raw(PREEMPT_NOTIFICATION.load(Ordering::Acquire));
    HIGH_WAITING.store(true, Ordering::Release);
    let badge = notification
        .wait()
        .unwrap_or_else(|error| fail(b"high wait", error));
    if badge != 1 {
        fail(b"high badge", badge);
    }
    HIGH_OBSERVED_LOW_COUNTER.store(LOW_COUNTER.load(Ordering::Acquire), Ordering::Release);
    HIGH_RAN.store(true, Ordering::Release);
    crate::thread::exit(0)
}

extern "C" fn low_busy(_arg: u64, _handle: u64, _ipc: u64) -> ! {
    while LOW_RUN.load(Ordering::Acquire) {
        LOW_COUNTER.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
    crate::thread::exit(0)
}

/// 低优先级Server先阻塞在Endpoint。高优先级Client执行Call后，Kernel
/// 通过一次性Reply关系把Client的effective priority传递给本线程。
extern "C" fn pi_server(_arg: u64, _handle: u64, ipc_va: u64) -> ! {
    let endpoint = crate::ipc::Endpoint::from_raw(PI_ENDPOINT.load(Ordering::Acquire));
    let mut buffer = unsafe { crate::ipc::IpcBuffer::from_va(ipc_va) };
    PI_SERVER_WAITING.store(true, Ordering::Release);
    endpoint
        .recv()
        .unwrap_or_else(|error| fail(b"PI server recv", error));
    let request = buffer.read();
    if request.label != 0x810 || request.reply.0 == 0 {
        fail(b"PI server request", 0x810);
    }
    buffer.write(exo_abi::IpcMessage {
        label: 0x811,
        words: [request.words[0] + 1, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    endpoint
        .reply(request.reply)
        .unwrap_or_else(|error| fail(b"PI server reply", error));
    crate::thread::exit(0)
}

extern "C" fn pi_medium(_arg: u64, _handle: u64, _ipc: u64) -> ! {
    while PI_MEDIUM_RUN.load(Ordering::Acquire) {
        PI_MEDIUM_COUNTER.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
    crate::thread::exit(0)
}

extern "C" fn pi_client(_arg: u64, _handle: u64, ipc_va: u64) -> ! {
    let endpoint = crate::ipc::Endpoint::from_raw(PI_ENDPOINT.load(Ordering::Acquire));
    let mut buffer = unsafe { crate::ipc::IpcBuffer::from_va(ipc_va) };
    buffer.write(exo_abi::IpcMessage {
        label: 0x810,
        words: [7, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    endpoint
        .call()
        .unwrap_or_else(|error| fail(b"PI client call", error));
    let reply = buffer.read();
    if reply.label != 0x811 || reply.words[0] != 8 {
        fail(b"PI client reply", 0x811);
    }
    PI_CLIENT_DONE.store(true, Ordering::Release);
    crate::thread::exit(0)
}

fn wait_until(predicate: impl Fn() -> bool, stage: &[u8]) {
    let deadline = crate::runtime::counter().wrapping_add(crate::runtime::counter_frequency() / 2);
    while !predicate() && (crate::runtime::counter().wrapping_sub(deadline) as i64) < 0 {
        core::hint::spin_loop();
    }
    if !predicate() {
        fail(stage, 0);
    }
}

fn wait_thread_exit(thread: &crate::thread::Thread, stage: &[u8]) {
    wait_until(
        || thread.runtime_ticks() == Err(exo_abi::SYS_ERR_NOT_FOUND),
        stage,
    );
}

fn run_round_robin_test() {
    RR_COUNTERS[0].store(0, Ordering::Release);
    RR_COUNTERS[1].store(0, Ordering::Release);
    RR_STARTED.store(0, Ordering::Release);
    RR_RUN.store(true, Ordering::Release);
    let first =
        crate::thread::Thread::spawn(rr_worker, 0, crate::thread::ThreadConfig::new(1, 40, 40))
            .unwrap_or_else(|error| fail(b"RR first create", error));
    let second =
        crate::thread::Thread::spawn(rr_worker, 1, crate::thread::ThreadConfig::new(1, 40, 40))
            .unwrap_or_else(|error| fail(b"RR second create", error));
    // 两个线程都至少获得一次CPU后才开始公平性观察，避免把首次栈映射、
    // TLB广播和QEMU vCPU启动抖动误算进1ms FIFO轮转结果。
    let start_deadline =
        crate::runtime::counter().wrapping_add(crate::runtime::counter_frequency() / 2);
    while RR_STARTED.load(Ordering::Acquire) != 0b11
        && (crate::runtime::counter().wrapping_sub(start_deadline) as i64) < 0
    {
        core::hint::spin_loop();
    }
    if RR_STARTED.load(Ordering::Acquire) != 0b11 {
        crate::runtime::puts(b"[libos] RR start mask=");
        crate::runtime::hex(RR_STARTED.load(Ordering::Acquire));
        crate::runtime::puts(b" handles=");
        crate::runtime::hex(first.handle().0);
        crate::runtime::puts(b"/");
        crate::runtime::hex(second.handle().0);
        crate::runtime::puts(b" runtime=");
        crate::runtime::hex(first.runtime_ticks().unwrap_or(u64::MAX));
        crate::runtime::puts(b"/");
        crate::runtime::hex(second.runtime_ticks().unwrap_or(u64::MAX));
        crate::runtime::puts(b"\r\n");
        fail(
            b"RR workers did not start",
            RR_STARTED.load(Ordering::Acquire),
        );
    }
    crate::runtime::delay_ns(100_000_000);
    let first_ticks = first
        .runtime_ticks()
        .unwrap_or_else(|error| fail(b"RR first runtime", error));
    let second_ticks = second
        .runtime_ticks()
        .unwrap_or_else(|error| fail(b"RR second runtime", error));
    RR_RUN.store(false, Ordering::Release);
    wait_thread_exit(&first, b"RR first exit");
    wait_thread_exit(&second, b"RR second exit");

    let a = RR_COUNTERS[0].load(Ordering::Acquire);
    let b = RR_COUNTERS[1].load(Ordering::Acquire);
    crate::runtime::puts(b"[libos] RR counters=");
    crate::runtime::hex(a);
    crate::runtime::puts(b"/");
    crate::runtime::hex(b);
    crate::runtime::puts(b" ticks=");
    crate::runtime::hex(first_ticks);
    crate::runtime::puts(b"/");
    crate::runtime::hex(second_ticks);
    crate::runtime::puts(b"\r\n");
    // QEMU主机调度会产生抖动，因此只要求两者都运行且长期计数不超过4:1。
    if a == 0
        || b == 0
        || first_ticks == 0
        || second_ticks == 0
        || first_ticks > second_ticks.saturating_mul(4)
        || second_ticks > first_ticks.saturating_mul(4)
    {
        fail(b"same-priority FIFO", a ^ b);
    }
    crate::runtime::puts(b"[libos] same-priority 1ms FIFO passed\r\n");
}

fn run_preemption_test() {
    let notification = crate::notification::Notification::create()
        .unwrap_or_else(|error| fail(b"preempt notification", error));
    PREEMPT_NOTIFICATION.store(notification.raw(), Ordering::Release);
    HIGH_WAITING.store(false, Ordering::Release);
    HIGH_RAN.store(false, Ordering::Release);
    LOW_COUNTER.store(0, Ordering::Release);

    let high =
        crate::thread::Thread::spawn(high_waiter, 0, crate::thread::ThreadConfig::new(1, 55, 55))
            .unwrap_or_else(|error| fail(b"high create", error));
    wait_until(
        || HIGH_WAITING.load(Ordering::Acquire),
        b"high did not wait",
    );

    LOW_RUN.store(true, Ordering::Release);
    let low =
        crate::thread::Thread::spawn(low_busy, 0, crate::thread::ThreadConfig::new(1, 20, 20))
            .unwrap_or_else(|error| fail(b"low create", error));
    wait_until(
        || LOW_COUNTER.load(Ordering::Acquire) != 0,
        b"low did not run",
    );

    notification
        .signal(1)
        .unwrap_or_else(|error| fail(b"preempt signal", error));
    wait_until(|| HIGH_RAN.load(Ordering::Acquire), b"high preemption");
    let observed = HIGH_OBSERVED_LOW_COUNTER.load(Ordering::Acquire);
    if observed == 0 {
        fail(b"high observed no low work", observed);
    }
    LOW_RUN.store(false, Ordering::Release);
    wait_thread_exit(&high, b"high exit");
    wait_thread_exit(&low, b"low exit");
    notification
        .destroy()
        .unwrap_or_else(|error| fail(b"preempt notification destroy", error));
    crate::runtime::puts(b"[libos] high-priority Notification preemption passed\r\n");
}

fn run_priority_inheritance_test() {
    let endpoint =
        crate::ipc::Endpoint::create().unwrap_or_else(|error| fail(b"PI endpoint", error));
    PI_ENDPOINT.store(endpoint.raw(), Ordering::Release);
    PI_SERVER_WAITING.store(false, Ordering::Release);
    PI_CLIENT_DONE.store(false, Ordering::Release);
    PI_MEDIUM_COUNTER.store(0, Ordering::Release);

    let server =
        crate::thread::Thread::spawn(pi_server, 0, crate::thread::ThreadConfig::new(1, 10, 55))
            .unwrap_or_else(|error| fail(b"PI server create", error));
    wait_until(
        || PI_SERVER_WAITING.load(Ordering::Acquire),
        b"PI server did not wait",
    );
    PI_MEDIUM_RUN.store(true, Ordering::Release);
    let medium =
        crate::thread::Thread::spawn(pi_medium, 0, crate::thread::ThreadConfig::new(1, 30, 30))
            .unwrap_or_else(|error| fail(b"PI medium create", error));
    wait_until(
        || PI_MEDIUM_COUNTER.load(Ordering::Acquire) != 0,
        b"PI medium did not run",
    );
    let client =
        crate::thread::Thread::spawn(pi_client, 0, crate::thread::ThreadConfig::new(1, 55, 55))
            .unwrap_or_else(|error| fail(b"PI client create", error));

    // 若Server没有继承55，它会一直被priority 30的忙线程压住，本条件超时。
    wait_until(
        || PI_CLIENT_DONE.load(Ordering::Acquire),
        b"transitive priority inheritance",
    );
    PI_MEDIUM_RUN.store(false, Ordering::Release);
    wait_thread_exit(&server, b"PI server exit");
    wait_thread_exit(&client, b"PI client exit");
    wait_thread_exit(&medium, b"PI medium exit");
    crate::runtime::puts(b"[libos] Endpoint Call priority inheritance passed\r\n");
}

pub fn run() {
    crate::runtime::puts(b"[libos] SMP static-priority smoke\r\n");
    // DTB只公布CPU0..3；CPU4亲和性和“基础优先级高于MCP”的创建请求
    // 都必须在分配栈或发布Ready状态前被Kernel拒绝。
    if crate::thread::Thread::spawn(
        affinity_worker,
        4,
        crate::thread::ThreadConfig::new(4, 40, 40),
    )
    .is_ok()
    {
        fail(b"invalid CPU accepted", 4);
    }
    if crate::thread::Thread::spawn(
        affinity_worker,
        1,
        crate::thread::ThreadConfig::new(1, 41, 40),
    )
    .is_ok()
    {
        fail(b"priority above MCP accepted", 41);
    }

    RUN.store(true, Ordering::Release);
    DONE.store(0, Ordering::Release);
    CPU_MASK.store(1, Ordering::Release);

    let cpu1 = crate::thread::Thread::spawn(
        affinity_worker,
        1,
        crate::thread::ThreadConfig::new(1, 40, 40),
    )
    .unwrap_or_else(|error| fail(b"cpu1 create", error));
    let cpu2 = crate::thread::Thread::spawn(
        affinity_worker,
        2,
        crate::thread::ThreadConfig::new(2, 40, 40),
    )
    .unwrap_or_else(|error| fail(b"cpu2 create", error));
    let cpu3 = crate::thread::Thread::spawn(
        affinity_worker,
        3,
        crate::thread::ThreadConfig::new(3, 40, 40),
    )
    .unwrap_or_else(|error| fail(b"cpu3 create", error));

    // QEMU TCG会把四个vCPU映射到宿主线程；首次创建还包含栈、IPC页映射
    // 和跨核TLBI，给辅助核100ms完成首次调度，验收的仍是最终CPU亲和性。
    crate::runtime::delay_ns(100_000_000);
    if CPU_MASK.load(Ordering::Acquire) != 0b1111 {
        RUN.store(false, Ordering::Release);
        fail(b"affinity mask", CPU_MASK.load(Ordering::Acquire));
    }
    for cpu in 1..4 {
        if COUNTERS[cpu].load(Ordering::Acquire) == 0 {
            fail(b"cpu no progress", cpu as u64);
        }
    }
    if cpu1.set_priority(64).is_ok() {
        fail(b"invalid priority accepted", 64);
    }
    if cpu2.runtime_ticks().unwrap_or(0) == 0 || cpu3.runtime_ticks().unwrap_or(0) == 0 {
        fail(b"runtime accounting", 0);
    }

    RUN.store(false, Ordering::Release);
    let deadline = crate::runtime::counter().wrapping_add(crate::runtime::counter_frequency() / 2);
    while DONE.load(Ordering::Acquire) & 0b1110 != 0b1110
        && (crate::runtime::counter().wrapping_sub(deadline) as i64) < 0
    {
        core::hint::spin_loop();
    }
    if DONE.load(Ordering::Acquire) & 0b1110 != 0b1110 {
        fail(b"worker exit", DONE.load(Ordering::Acquire));
    }

    // DONE是在Worker发起THREAD_EXIT前写入的用户态标志，只能说明它即将
    // 退出。继续等待旧Handle由Kernel判为NOT_FOUND，才能证明远程核已经
    // 完成栈/IPC页回收和slot generation递增，后续机器人测试才可复用slot。
    let exit_deadline =
        crate::runtime::counter().wrapping_add(crate::runtime::counter_frequency() / 2);
    loop {
        let cpu1_exited = cpu1.runtime_ticks() == Err(exo_abi::SYS_ERR_NOT_FOUND);
        let cpu2_exited = cpu2.runtime_ticks() == Err(exo_abi::SYS_ERR_NOT_FOUND);
        let cpu3_exited = cpu3.runtime_ticks() == Err(exo_abi::SYS_ERR_NOT_FOUND);
        if cpu1_exited && cpu2_exited && cpu3_exited {
            break;
        }
        if (crate::runtime::counter().wrapping_sub(exit_deadline) as i64) >= 0 {
            fail(b"kernel exit completion", 0);
        }
        core::hint::spin_loop();
    }
    run_round_robin_test();
    run_preemption_test();
    run_priority_inheritance_test();
    crate::runtime::puts(b"[libos] SMP affinity + priority passed\r\n");
}

fn fail(stage: &[u8], error: u64) -> ! {
    crate::runtime::puts(b"[libos] priority smoke failed: ");
    crate::runtime::puts(stage);
    crate::runtime::puts(b" error=");
    crate::runtime::hex(error);
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(0x500)
}
