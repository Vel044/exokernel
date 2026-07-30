//! 单VSpace机器人并发子测试。
//!
//! 该测试把三类典型线程同时放进调度环：
//! - USB线程独占PCI/xHCI/CDC对象，并通过硬件IRQ Notification推进Future；
//! - 推理线程持续忙循环，故意不调用yield，验证Generic Timer强制抢占；
//! - 控制线程通过Endpoint处理同步命令，并用Notification报告生命周期事件。
//!
//! 无xHCI feature时仍运行推理与控制线程；启用`qemu-xhci,usb-echo`时再加入
//! QEMU模拟FTDI，从而覆盖Timer PPI和xHCI SPI同时到达的真实路径。

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const CONTROL_READY: u64 = 1 << 0;
const CONTROL_DONE: u64 = 1 << 1;
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
const USB_READY: u64 = 1 << 2;

const CONTROL_REQUEST: u64 = 0x700;
const CONTROL_REPLY: u64 = 0x701;
const CONTROL_STOP: u64 = 0x702;

static CONTROL_ENDPOINT: AtomicU64 = AtomicU64::new(0);
static ROBOT_EVENTS: AtomicU64 = AtomicU64::new(0);
static INFERENCE_RUN: AtomicBool = AtomicBool::new(false);
static INFERENCE_READY: AtomicBool = AtomicBool::new(false);
static INFERENCE_DONE: AtomicBool = AtomicBool::new(false);
static INFERENCE_STEPS: AtomicU64 = AtomicU64::new(0);

/// 模拟CPU密集型推理。循环中完全没有syscall或yield，只有EL1的1ms
/// Generic Timer能够强制打断它，让USB和控制线程继续运行。
extern "C" fn inference_worker(_arg: u64, _thread_handle: u64, _ipc_va: u64) -> ! {
    INFERENCE_READY.store(true, Ordering::Release);
    while INFERENCE_RUN.load(Ordering::Acquire) {
        INFERENCE_STEPS.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
    INFERENCE_DONE.store(true, Ordering::Release);
    crate::thread::exit(0)
}

/// 控制线程只拥有控制协议状态，不直接接触USB对象。请求和回复分别从该线程
/// 独占的IPC Buffer读写，展示同一VSpace内仍可使用受控的同步调用边界。
extern "C" fn control_worker(_arg: u64, _thread_handle: u64, ipc_va: u64) -> ! {
    let endpoint = crate::ipc::Endpoint::from_raw(CONTROL_ENDPOINT.load(Ordering::Acquire));
    let events = crate::notification::Notification::from_raw(ROBOT_EVENTS.load(Ordering::Acquire));
    let mut buffer = unsafe { crate::ipc::IpcBuffer::from_va(ipc_va) };

    events
        .signal(CONTROL_READY)
        .unwrap_or_else(|error| fail(b"control ready signal", error));
    endpoint
        .recv()
        .unwrap_or_else(|error| fail(b"control recv", error));
    let request = buffer.read();
    if request.label != CONTROL_REQUEST || request.words[0] != 41 || request.reply.0 == 0 {
        fail(b"control request", 0x601);
    }

    buffer.write(exo_abi::IpcMessage {
        label: CONTROL_REPLY,
        words: [request.words[0] + 1, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    // 常驻服务通常使用REPLY_RECV：完成Reply并原子进入下一次接收，
    // 避免Reply和Recv之间出现遗漏请求的窗口。
    endpoint
        .reply_recv(request.reply)
        .unwrap_or_else(|error| fail(b"control reply-recv", error));
    let stop = buffer.read();
    if stop.label != CONTROL_STOP || stop.reply.0 != 0 {
        fail(b"control stop", 0x602);
    }
    events
        .signal(CONTROL_DONE)
        .unwrap_or_else(|error| fail(b"control done signal", error));
    crate::thread::exit(0)
}

/// USB线程获得BootInfo地址后独占整个USB软件栈。回显模式在Bulk endpoint
/// 配置完成后报告ready；SCServo模式会等PING、读取和可选中位运动完成。
#[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
extern "C" fn usb_worker(boot_info_va: u64, _thread_handle: u64, _ipc_va: u64) -> ! {
    let info = unsafe { &*(boot_info_va as *const exo_abi::UserBootInfo) };
    let notification = exo_abi::NotificationHandle(ROBOT_EVENTS.load(Ordering::Acquire));
    crate::usb_task::run_with_ready_signal(
        info,
        Some(crate::usb_task::ReadySignal {
            notification,
            badge: USB_READY,
        }),
    )
}

pub fn run(_info: &exo_abi::UserBootInfo) -> ! {
    crate::runtime::puts(b"[libos] Robot thread integration start\r\n");

    let events = crate::notification::Notification::create()
        .unwrap_or_else(|error| fail(b"robot events create", error));
    let endpoint =
        crate::ipc::Endpoint::create().unwrap_or_else(|error| fail(b"control endpoint", error));
    ROBOT_EVENTS.store(events.raw(), Ordering::Release);
    CONTROL_ENDPOINT.store(endpoint.raw(), Ordering::Release);

    INFERENCE_RUN.store(true, Ordering::Release);
    INFERENCE_READY.store(false, Ordering::Release);
    INFERENCE_DONE.store(false, Ordering::Release);
    INFERENCE_STEPS.store(0, Ordering::Release);

    let _control = crate::thread::Thread::spawn(
        control_worker,
        0,
        crate::thread::ThreadConfig::new(1, 56, 56),
    )
    .unwrap_or_else(|error| fail(b"control thread", error));
    let inference = crate::thread::Thread::spawn(
        inference_worker,
        0,
        crate::thread::ThreadConfig::new(3, 16, 16),
    )
    .unwrap_or_else(|error| fail(b"inference thread", error));

    #[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
    let usb = crate::thread::Thread::spawn(
        usb_worker,
        _info as *const exo_abi::UserBootInfo as u64,
        crate::thread::ThreadConfig::new(2, 48, 48),
    )
    .unwrap_or_else(|error| fail(b"USB thread", error));

    // 等待控制线程和可选USB线程真正进入可服务状态。Notification会OR合并
    // 同时到达的badge，所以循环必须累计，而不能假设固定到达顺序。
    #[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
    let required = CONTROL_READY | USB_READY;
    #[cfg(not(any(feature = "qemu-xhci", feature = "pi5-xhci")))]
    let required = CONTROL_READY;
    let mut observed = 0u64;
    while observed & required != required {
        observed |= events
            .wait()
            .unwrap_or_else(|error| fail(b"robot ready wait", error));
    }
    if !INFERENCE_READY.load(Ordering::Acquire) {
        // 推理线程固定运行在CPU3；让CPU0短暂等待其首次被调度。
        crate::runtime::delay_ns(2_000_000);
    }

    let mut main_buffer = crate::ipc::IpcBuffer::initial();
    main_buffer.write(exo_abi::IpcMessage {
        label: CONTROL_REQUEST,
        words: [41, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    endpoint
        .call()
        .unwrap_or_else(|error| fail(b"control call", error));
    let reply = main_buffer.read();
    if reply.label != CONTROL_REPLY || reply.words[0] != 42 || reply.reply.0 != 0 {
        fail(b"control reply", 0x603);
    }

    // 主线程忙等40ms，故意不yield。推理计数仍必须增长；有USB时，其初始化
    // 也已在同一时段依靠xHCI IRQ完成，证明PPI与SPI可以共同工作。
    let before = INFERENCE_STEPS.load(Ordering::Acquire);
    crate::runtime::delay_ns(40_000_000);
    if INFERENCE_STEPS.load(Ordering::Acquire) <= before {
        fail(b"inference preemption progress", 0x604);
    }
    if inference
        .runtime_ticks()
        .unwrap_or_else(|error| fail(b"inference ticks", error))
        == 0
    {
        fail(b"inference zero ticks", 0x605);
    }
    #[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
    if usb
        .runtime_ticks()
        .unwrap_or_else(|error| fail(b"USB ticks", error))
        == 0
    {
        fail(b"USB zero ticks", 0x606);
    }

    main_buffer.write(exo_abi::IpcMessage {
        label: CONTROL_STOP,
        words: [0; exo_abi::IPC_MESSAGE_WORDS],
        reply: exo_abi::ReplyHandle(0),
    });
    endpoint
        .send()
        .unwrap_or_else(|error| fail(b"control stop send", error));
    let done = events
        .wait()
        .unwrap_or_else(|error| fail(b"control done wait", error));
    if done & CONTROL_DONE == 0 {
        fail(b"control done badge", done);
    }

    INFERENCE_RUN.store(false, Ordering::Release);
    let deadline = crate::runtime::counter().wrapping_add(crate::runtime::counter_frequency() / 2);
    while !INFERENCE_DONE.load(Ordering::Acquire)
        && (crate::runtime::counter().wrapping_sub(deadline) as i64) < 0
    {
        core::hint::spin_loop();
    }
    if !INFERENCE_DONE.load(Ordering::Acquire) {
        fail(b"inference exit", 0x607);
    }

    // USB线程仍阻塞在真实硬件事件上，整个Task退出时由Kernel统一撤销它的
    // IRQ、DMA和线程资源。这同时验证带活跃设备线程的退出回收。
    #[cfg(any(feature = "qemu-xhci", feature = "pi5-xhci"))]
    {
        core::mem::forget(usb);
    }
    crate::runtime::puts(b"[libos] Robot thread integration passed\r\n");
    crate::runtime::exit(0)
}

fn fail(stage: &[u8], error: u64) -> ! {
    crate::runtime::puts(b"[libos] robot integration failed: ");
    crate::runtime::puts(stage);
    crate::runtime::puts(b" error=");
    crate::runtime::hex(error);
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(0x600)
}
