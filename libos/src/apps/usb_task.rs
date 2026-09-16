//! USB线程入口、启动顺序和统一错误处理。
//!
//! 该任务依次初始化日志、xHCI和上层应用。它只负责组合模块，不解析PCI
//! 配置空间、不实现USB descriptor协议，也不包含SCServo数据帧。

/// 综合实验用于等待USB任务真正可服务的通知目标。
#[derive(Clone, Copy)]
pub(crate) struct ReadySignal {
    /// system-smoke主线程创建的Notification句柄；这里只借用，不负责销毁。
    pub notification: exo_abi::NotificationHandle,
    /// USB_READY事件位。Notification会把多次signal按位OR合并。
    pub badge: u64,
}

/// 普通单任务启动入口，不需要向其他线程报告ready。
pub(crate) fn run(info: &exo_abi::UserBootInfo) -> ! {
    // None表示USB应用就绪后直接继续运行，不执行NOTIFICATION_SIGNAL。
    run_with_ready_signal(info, None)
}

extern "C" fn dedicated_usb_entry(boot_info: u64, _thread: u64, _ipc: u64) -> ! {
    let info = unsafe { &*(boot_info as *const exo_abi::UserBootInfo) };
    run(info)
}

/// 创建固定运行在 CPU2 的 USB 线程，并让当前启动线程停放等待。
pub(crate) fn run_dedicated(info: &exo_abi::UserBootInfo) -> ! {
    // 本应用要求 CPU2 在线，用它专门运行 USB 栈。
    if info.cpu_count < 3 {
        fail("SMP CPU2 is not online", 0x20e);
    }

    // 把入口、UserBootInfo 地址和调度参数交给 Kernel，创建一个 CPU2 线程。
    let _usb = crate::thread::Thread::spawn(
        dedicated_usb_entry,
        info as *const exo_abi::UserBootInfo as u64,
        crate::thread::ThreadConfig::new(2, 48, 48),
    )
    .unwrap_or_else(|error| fail_with_code("failed to create USB thread", error));

    // 创建一个仅用于停放当前启动线程的 Notification，不负责 USB 中断。
    let parked = crate::notification::Notification::create()
        .unwrap_or_else(|error| fail_with_code("failed to park main thread", error));

    // CPU0 永久阻塞，把执行权留给 CPU2 上的 USB 线程。
    loop {
        let _ = parked.wait();
    }
}

/// USB线程的完整生命周期入口。
///
/// 返回类型为`!`：成功后上层应用永久运行；失败时调用SYS_EXIT终止任务。
pub(crate) fn run_with_ready_signal(
    info: &exo_abi::UserBootInfo,
    // uvc-smoke没有协调线程；下划线允许该独立feature不产生未使用参数告警。
    _ready_signal: Option<ReadySignal>,
) -> ! {
    // 注册CrabUSB使用的log facade。最终输出仍走libOS已经接管的PL011。
    crate::logger::init();
    // TPIDRRO_EL0由Kernel在每次切换到EL0前写入当前逻辑CPU号。
    // 该日志用于确认普通USB构建确实在CPU2运行，随后IRQ_BIND会把xHCI SPI
    // 路由到同一CPU，避免跨核转发设备中断。
    crate::runtime::puts(b"[libos] USB thread CPU=");
    crate::runtime::hex(crate::thread::current_cpu() as u64);
    crate::runtime::puts(b"\r\n");

    // xHCI模块负责PCI/直连资源、MMIO映射、IRQ Notification和Host复位。
    // 返回的XhciContext拥有USBHost、EventHandler、INTID和Notification，
    // 因此这些对象的生命周期覆盖后续所有USB传输。
    let mut xhci = crate::drivers::xhci::initialize(info)
        .unwrap_or_else(|(message, code)| fail(message, code));

    #[cfg(feature = "app-robot-act-once")]
    {
        // 机器人策略拥有同一Host、EventHandler、INTID和Notification；它只
        // 编排设备角色与动作，不重复初始化xHCI或另建IRQ通道。
        crate::apps::robot_act_once::run_with_resources(
            info,
            &mut xhci.host,
            &xhci.handler,
            xhci.intid,
            &xhci.notification,
        )
    }

    #[cfg(feature = "app-robot-observation")]
    {
        crate::apps::robot_observation::run_with_resources(
            info,
            &mut xhci.host,
            &xhci.handler,
            xhci.intid,
            &xhci.notification,
        )
    }

    // UVC smoke只验证一台摄像头的协议协商和单帧等时接收。设备策略放在
    // apps::uvc_smoke，UVC描述符与数据包协议放在drivers::uvc。
    #[cfg(feature = "app-uvc-smoke")]
    {
        crate::apps::uvc_smoke::run(
            info,
            &mut xhci.host,
            &xhci.handler,
            xhci.intid,
            &xhci.notification,
        )
    }

    // SCServo构建把USB串口当作半双工字节流，不包含回显策略。
    #[cfg(any(
        feature = "ide",
        feature = "app-scservo",
        feature = "app-robot-action-replay"
    ))]
    {
        // open_scservo_transport是async：枚举、控制传输和端点配置都可能Pending。
        // block_on_usb负责在Pending时等待xHCI Notification并推进Event Ring。
        let mut transport = match crate::runtime::usb_executor::block_on_usb(
            crate::drivers::usb_serial::open_scservo_transport(&mut xhci.host, &xhci.handler),
            &xhci.handler,
            xhci.intid,
            &xhci.notification,
        ) {
            Ok(transport) => transport,
            Err(stage) => {
                crate::runtime::puts(b"[libos] CDC ACM failure stage=");
                crate::runtime::hex(stage);
                crate::runtime::puts(b"\r\n");
                fail("CDC ACM setup failed", 0x208);
            }
        };
        // 到这里USB Host和串口端点已经完成配置；把业务控制权交给舵机应用。
        #[cfg(any(feature = "ide", feature = "app-scservo"))]
        crate::apps::scservo_app::run(
            &mut transport,
            &xhci.handler,
            xhci.intid,
            &xhci.notification,
            _ready_signal,
        );
        #[cfg(feature = "app-robot-action-replay")]
        match crate::runtime::usb_executor::block_on_usb(
            crate::apps::robot_action_replay::run(info, &mut transport),
            &xhci.handler,
            xhci.intid,
            &xhci.notification,
        ) {
            Ok(()) => crate::runtime::exit(0),
            Err(code) => fail_with_code("ACT action replay failed", code),
        }
    }

    #[cfg(any(
        feature = "ide",
        feature = "app-usb-echo",
        feature = "app-system-smoke"
    ))]
    {
        // usb_echo::run本身是一个永久Future：枚举USB设备 识别CDC ACM或FTDI
        // 持续提交Bulk IN，再把payload
        // 通过Bulk OUT写回。正常情况下该调用永远不会返回。
        if let Err(stage) = crate::runtime::usb_executor::block_on_usb(
            crate::apps::usb_echo::run(&mut xhci.host, &xhci.handler, _ready_signal),
            &xhci.handler,
            xhci.intid,
            &xhci.notification,
        ) {
            crate::runtime::puts(b"[libos] USB echo failure stage=");
            crate::runtime::hex(stage);
            crate::runtime::puts(b"\r\n");
            fail("USB echo stopped", 0x209);
        }
        // Ok返回同样不符合永久回显任务的状态，因此也按错误处理。
        fail("USB echo returned unexpectedly", 0x20a)
    }

    #[cfg(not(any(
        feature = "ide",
        feature = "app-scservo",
        feature = "app-uvc-smoke",
        feature = "app-usb-echo",
        feature = "app-system-smoke",
        feature = "app-robot-act-once",
        feature = "app-robot-observation",
        feature = "app-robot-action-replay"
    )))]
    fail("no xHCI application feature selected", 0x20b)
}

/// 应用准备完成后，向system-smoke主线程发送badge。
pub(crate) fn signal_ready(signal: Option<ReadySignal>) {
    // 非system-smoke路径没有观察者，不需要额外系统调用。
    let Some(signal) = signal else {
        return;
    };
    // from_raw只包装现有handle，不创建新的Kernel Notification对象。
    let notification = crate::notification::Notification::from_raw(signal.notification.0);
    // signal最终执行NOTIFICATION_SIGNAL，唤醒等待USB_READY的主线程。
    if notification.signal(signal.badge).is_err() {
        fail("USB ready Notification signal failed", 0x20d);
    }
}

/// 输出稳定的错误文本，并让EL1统一回收当前任务的MMIO、DMA和IRQ。
fn fail(message: &'static str, code: u64) -> ! {
    crate::runtime::puts(b"[libos] ");
    crate::runtime::puts(message.as_bytes());
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(code)
}

fn fail_with_code(message: &'static str, code: u64) -> ! {
    crate::runtime::puts(b"[libos] ");
    crate::runtime::puts(message.as_bytes());
    crate::runtime::puts(b" code=");
    crate::runtime::hex(code);
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(0x20f)
}
