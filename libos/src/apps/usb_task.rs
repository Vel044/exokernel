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

/// 普通机器人构建把USB栈固定到CPU2。CPU0只负责启动和协调，随后阻塞，
/// xHCI SPI由USB线程在CPU2执行IRQ_BIND时直接路由到同一CPU。
pub(crate) fn run_dedicated(info: &exo_abi::UserBootInfo) -> ! {
    if info.cpu_count < 3 {
        fail("SMP CPU2 is not online", 0x20e);
    }
    let _usb = crate::thread::Thread::spawn(
        dedicated_usb_entry,
        info as *const exo_abi::UserBootInfo as u64,
        crate::thread::ThreadConfig::new(2, 48, 48),
    )
    .unwrap_or_else(|error| fail_with_code("failed to create USB thread", error));
    let parked = crate::notification::Notification::create()
        .unwrap_or_else(|error| fail_with_code("failed to park main thread", error));
    loop {
        let _ = parked.wait();
    }
}

/// USB线程的完整生命周期入口。
///
/// 返回类型为`!`：成功后上层应用永久运行；失败时调用SYS_EXIT终止任务。
pub(crate) fn run_with_ready_signal(
    info: &exo_abi::UserBootInfo,
    ready_signal: Option<ReadySignal>,
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

    // SCServo构建把USB串口当作半双工字节流，不包含回显策略。
    #[cfg(feature = "scservo")]
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
        crate::apps::scservo_app::run(
            &mut transport,
            &xhci.handler,
            xhci.intid,
            &xhci.notification,
            ready_signal,
        )
    }

    #[cfg(all(not(feature = "scservo"), feature = "usb-echo"))]
    {
        // usb_echo::run本身是一个永久Future：枚举USB设备 识别CDC ACM或FTDI
        // 持续提交Bulk IN，再把payload
        // 通过Bulk OUT写回。正常情况下该调用永远不会返回。
        if let Err(stage) = crate::runtime::usb_executor::block_on_usb(
            crate::apps::usb_echo::run(&mut xhci.host, &xhci.handler, ready_signal),
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

    #[cfg(not(any(feature = "scservo", feature = "usb-echo")))]
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
