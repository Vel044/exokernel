//! USB串口回显诊断应用。
//!
//! 本文件只定义“收到字节后原样写回”的策略。CDC ACM/FTDI识别与控制请求
//! 位于drivers::usb_serial，xHCI初始化位于drivers::xhci。

use crab_usb::{EventHandler, ProbedDevice, USBHost};

use crate::apps::usb_task::{signal_ready, ReadySignal};

pub(crate) async fn run(
    // USBHost保存xHCI command/transfer ring及已枚举设备状态。
    host: &mut USBHost,
    // EventHandler从xHCI Event Ring中取出completion并唤醒对应Future。
    handler: &EventHandler,
    // 综合实验使用；普通回显测试传None。
    ready_signal: Option<ReadySignal>,
) -> Result<(), u64> {
    crate::runtime::puts(b"[libos] usb-echo diagnostic mode\r\n");

    let devices = loop {
        // probe_devices会复位Root Hub端口，执行Enable Slot、Address Device，
        // 再通过Endpoint 0读取Device和Configuration descriptor。
        let devices = host.probe_devices().await.map_err(|_| 40u64)?;
        if !devices.is_empty() {
            break devices;
        }
        // 某些端口变化已经写入Event Ring但没有新的IRQ边沿，主动消费一次。
        handler.handle_event();
        // 避免未插设备时持续轮询占满CPU。
        crate::runtime::delay_ns(10_000_000);
    };

    // 仅输出设备身份；选择与配置仍由下面的类驱动判断。
    for device in &devices {
        crate::drivers::usb_serial::log_device(device);
    }

    // 优先选择CDC ACM，若没有则使用QEMU usb-serial模拟的FTDI。
    for device in &devices {
        // Hub等拓扑对象也会出现在ProbedDevice中，只有Device才有普通接口。
        if let ProbedDevice::Device(info) = device {
            if crate::drivers::usb_serial::is_cdc_acm(info) {
                // 配置configuration、control/data interface和1Mbaud 8N1。
                let transport = crate::drivers::usb_serial::configure_cdc_acm(host, info).await?;
                return echo_transport(transport, ready_signal).await;
            }
        }
    }
    for device in &devices {
        if let ProbedDevice::Device(info) = device {
            if crate::drivers::usb_serial::is_ftdi(info) {
                // QEMU usb-serial模拟FT232BM；除数26对应115200 baud。
                let transport =
                    crate::drivers::usb_serial::configure_ftdi(host, info, 26, false).await?;
                return echo_transport(transport, ready_signal).await;
            }
        }
    }
    Err(43)
}

async fn echo_transport(
    // transport拥有Bulk IN和Bulk OUT Endpoint，离开函数前一直保持claim。
    mut transport: crate::drivers::usb_serial::UsbSerialTransport,
    ready_signal: Option<ReadySignal>,
) -> Result<(), u64> {
    // “ready”表示端点已经完成配置，socket现在可以安全发送数据。
    signal_ready(ready_signal);
    // 固定栈缓冲避免每轮回显进行heap分配。
    let mut input = [0u8; 512];
    loop {
        // read提交Bulk IN TRB。Future Pending时，外层执行器阻塞等待xHCI IRQ。
        let length = transport.read(&mut input).await.map_err(|_| 41u64)?;
        if length != 0 {
            // 只回写本次实际完成长度；FTDI状态头已在驱动层删除。
            transport.write(&input[..length]).await.map_err(|_| 42u64)?;
        }
    }
}
