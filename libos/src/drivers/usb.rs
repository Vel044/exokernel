//! QEMU PCI xHCI / Pi5 RP1 DWC3-xHCI 上的 CDC ACM 回显。
//!
//! 控制器资源获取分为两个后端，USB 枚举和 CDC ACM 协议保持公共：
//! - QEMU: EL0 扫描 PCI ECAM，读取 xHCI BAR；
//! - Pi5: EL1 从 DTB 提供 RP1 DWC3 的 MMIO/IRQ，EL0 直接映射；
//! - 两条路径最终都调用 CrabUSB 的标准 xHCI Host 实现。

#[cfg(feature = "usb-echo")]
use alloc::vec::Vec;

use core::{
    future::Future,
    pin::Pin,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use crab_usb::usb_if::{
    descriptor::EndpointType,
    endpoint::TransferRequest,
    err::TransferError,
    host::ControlSetup,
    transfer::{Direction, Recipient, Request, RequestType},
};
use crab_usb::{Endpoint, EventHandler, ProbedDevice, USBHost};

const CDC_CONTROL_CLASS: u8 = 0x02;
const CDC_CONTROL_SUBCLASS: u8 = 0x02;
const CDC_CONTROL_PROTOCOL: u8 = 0x01;
const CDC_DATA_CLASS: u8 = 0x0a;
const FTDI_VENDOR_ID: u16 = 0x0403;
const FTDI_PRODUCT_ID: u16 = 0x6001;
const XHCI_IRQ_BADGE: u64 = 1;

#[derive(Clone, Copy)]
struct HostResource {
    base: u64,
    size: u64,
    intid: u32,
}

/// CDC ACM 数据接口的公共 Bulk 传输。
///
/// xHCI、PCI 和 RP1 的差异在这个对象之上已经被 CrabUSB 隐藏；
/// 协议层只看到一个可异步读写的串行字节流。
pub(crate) struct CdcAcmTransport {
    endpoint_in: Endpoint,
    endpoint_out: Endpoint,
    /// QEMU FTDI的每个Bulk IN packet前有两个modem/status字节。
    ftdi_packet_size: Option<usize>,
}

impl CdcAcmTransport {
    pub(crate) async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, TransferError> {
        let Some(packet_size) = self.ftdi_packet_size else {
            return Ok(self
                .endpoint_in
                .wait(TransferRequest::bulk_in(buffer))
                .await?
                .actual_length
                .min(buffer.len()));
        };

        let mut usb_packet = [0u8; 512];
        let length = self
            .endpoint_in
            .wait(TransferRequest::bulk_in(&mut usb_packet))
            .await?
            .actual_length
            .min(usb_packet.len());
        let mut copied = 0;
        for packet in usb_packet[..length].chunks(packet_size.max(2)) {
            if packet.len() <= 2 || copied == buffer.len() {
                continue;
            }
            let payload = &packet[2..];
            let count = payload.len().min(buffer.len() - copied);
            buffer[copied..copied + count].copy_from_slice(&payload[..count]);
            copied += count;
        }
        Ok(copied)
    }

    pub(crate) async fn write(&mut self, buffer: &[u8]) -> Result<usize, TransferError> {
        Ok(self
            .endpoint_out
            .wait(TransferRequest::bulk_out(buffer))
            .await?
            .actual_length
            .min(buffer.len()))
    }

    /// 严格按半双工顺序完成一次命令/应答事务。
    ///
    /// macOS 的 QEMU usb-host 后端可能把预先挂起的 IN 请求排在物理 OUT
    /// 之前，导致 CH34x 串口命令没有真正推进。舵机转接板会缓存短状态包，
    /// 因此顺序与 pyserial 和原始 libusb 的成功路径保持一致：先 OUT，后 IN。
    pub(crate) async fn exchange(
        &mut self,
        output: &[u8],
        input: &mut [u8],
    ) -> Result<(usize, usize), TransferError> {
        let written = match self
            .endpoint_out
            .wait(TransferRequest::bulk_out(output))
            .await
        {
            Ok(completion) => completion.actual_length.min(output.len()),
            Err(error) => return Err(error),
        };

        // 这条日志把物理透传故障分成两段：看到它说明命令已经经过
        // xHCI Bulk OUT 发出，若随后卡住就是 Bulk IN 应答没有完成。
        crate::runtime::puts(b"[libos] SCServo Bulk OUT complete; waiting Bulk IN\r\n");

        // Endpoint Future保持Pending时，最外层执行器进入SYS_IRQ_WAIT；xHCI
        // completion到达后消费Event Ring并唤醒Future，避免自唤醒忙轮询。
        let read = self.read(input).await?;
        Ok((written, read))
    }
}

pub fn run(info: &exo_abi::UserBootInfo) -> ! {
    crate::logger::init();

    let resource = if info.xhci_transport == exo_abi::XHCI_TRANSPORT_DIRECT {
        if info.xhci.base == 0 || info.xhci.size == 0 || info.xhci.intid < 32 {
            fail("invalid direct RP1 xHCI resource", 0x200);
        }
        crate::runtime::puts(b"[libos] Pi5 direct RP1 xHCI backend\r\n");
        HostResource {
            base: info.xhci.base,
            size: info.xhci.size,
            intid: info.xhci.intid,
        }
    } else if info.xhci_transport == exo_abi::XHCI_TRANSPORT_PCI {
        crate::runtime::puts(b"[libos] QEMU PCI xHCI backend\r\n");
        let xhci = match crate::pci::find_xhci(&info.pci) {
            Ok(xhci) => xhci,
            Err(message) => fail(message, 0x201),
        };
        crate::runtime::puts(b"[libos] xHCI BDF=");
        crate::runtime::hex(
            ((xhci.bdf.bus as u64) << 16)
                | ((xhci.bdf.device as u64) << 8)
                | xhci.bdf.function as u64,
        );
        crate::runtime::puts(b" BAR=");
        crate::runtime::hex(xhci.bar_pa);
        crate::runtime::puts(b" IRQ=");
        crate::runtime::hex(xhci.intid as u64);
        crate::runtime::puts(b"\r\n");
        HostResource {
            base: xhci.bar_pa,
            size: xhci.bar_size,
            intid: xhci.intid,
        }
    } else {
        fail("no xHCI transport in UserBootInfo", 0x202);
    };

    if crate::runtime::map_mmio(resource.base, resource.size, exo_abi::XHCI_VA).is_err() {
        fail("failed to map xHCI MMIO", 0x203);
    }
    let irq_notification = crate::notification::Notification::create()
        .unwrap_or_else(|_| fail("failed to create xHCI Notification", 0x204));
    if irq_notification
        .bind_irq(resource.intid, XHCI_IRQ_BADGE)
        .is_err()
    {
        fail("failed to bind xHCI IRQ", 0x204);
    }

    let mmio = NonNull::new(exo_abi::XHCI_VA as *mut u8).unwrap();
    let mut host = match USBHost::new_xhci(mmio, &crate::dma::USB_KERNEL) {
        Ok(host) => host,
        Err(_) => fail("CrabUSB xHCI construction failed", 0x205),
    };
    let handler = host.create_event_handler();
    if block_on_usb(host.init(), &handler, resource.intid, &irq_notification).is_err() {
        fail("CrabUSB xHCI initialization failed", 0x206);
    }
    handler.handle_event();
    if host.enable_irq().is_err() {
        fail("CrabUSB failed to enable IRQ", 0x207);
    }

    #[cfg(feature = "scservo")]
    {
        let mut transport = match block_on_usb(
            open_cdc_acm(&mut host, &handler),
            &handler,
            resource.intid,
            &irq_notification,
        ) {
            Ok(transport) => transport,
            Err(stage) => {
                crate::runtime::puts(b"[libos] CDC ACM failure stage=");
                crate::runtime::hex(stage);
                crate::runtime::puts(b"\r\n");
                fail("CDC ACM setup failed", 0x208);
            }
        };
        crate::scservo::run(&mut transport, &handler, resource.intid, &irq_notification)
    }

    #[cfg(all(not(feature = "scservo"), feature = "usb-echo"))]
    {
        if let Err(stage) = block_on_usb(
            usb_echo(&mut host, &handler),
            &handler,
            resource.intid,
            &irq_notification,
        ) {
            crate::runtime::puts(b"[libos] USB echo failure stage=");
            crate::runtime::hex(stage);
            crate::runtime::puts(b"\r\n");
            fail("USB echo stopped", 0x209);
        }
        fail("USB echo returned unexpectedly", 0x20a)
    }

    #[cfg(not(any(feature = "scservo", feature = "usb-echo")))]
    fail("no xHCI application feature selected", 0x20b)
}

/// 枚举并配置一个 CDC ACM 设备，返回可供 SCServo 使用的字节流。
#[cfg(feature = "scservo")]
pub(crate) async fn open_cdc_acm(
    host: &mut USBHost,
    handler: &EventHandler,
) -> Result<CdcAcmTransport, u64> {
    crate::runtime::puts(b"[libos] probing USB devices\r\n");
    let devices = loop {
        let devices = host.probe_devices().await.map_err(|_| 1u64)?;
        if !devices.is_empty() {
            break devices;
        }
        handler.handle_event();
        crate::runtime::delay_ns(10_000_000);
    };

    let mut selected_cdc = None;
    let mut selected_ftdi = None;
    for device in &devices {
        crate::runtime::puts(b"[libos] USB device VID=");
        crate::runtime::hex(device.vendor_id() as u64);
        crate::runtime::puts(b" PID=");
        crate::runtime::hex(device.product_id() as u64);
        crate::runtime::puts(b"\r\n");
        if let ProbedDevice::Device(device) = device {
            if selected_cdc.is_none() && find_cdc_interfaces(device).is_some() {
                selected_cdc = Some(device);
            }
            if selected_ftdi.is_none()
                && device.vendor_id() == FTDI_VENDOR_ID
                && device.product_id() == FTDI_PRODUCT_ID
            {
                selected_ftdi = Some(device);
            }
        }
    }
    if let Some(info) = selected_cdc {
        return configure_cdc_acm(host, info).await;
    }
    if let Some(info) = selected_ftdi {
        return configure_ftdi_scservo(host, info).await;
    }
    Err(2u64)
}

async fn configure_cdc_acm(
    host: &mut USBHost,
    info: &crab_usb::DeviceInfo,
) -> Result<CdcAcmTransport, u64> {
    let (configuration, control_if, data_if, bulk_in, bulk_out) =
        find_cdc_interfaces(info).ok_or(3u64)?;
    let control_number = control_if.interface_number;
    let data_number = data_if.interface_number;
    let in_address = bulk_in.address;
    let out_address = bulk_out.address;

    crate::runtime::puts(b"[libos] CDC ACM control_if=");
    crate::runtime::hex(control_number as u64);
    crate::runtime::puts(b" data_if=");
    crate::runtime::hex(data_number as u64);
    crate::runtime::puts(b" IN=");
    crate::runtime::hex(in_address as u64);
    crate::runtime::puts(b" OUT=");
    crate::runtime::hex(out_address as u64);
    crate::runtime::puts(b"\r\n");

    let mut device = host.open_device(info).await.map_err(|_| 4u64)?;
    device
        .set_configuration(configuration.configuration_value)
        .await
        .map_err(|_| 5u64)?;
    device
        .claim_interface(control_number, control_if.alternate_setting)
        .await
        .map_err(|_| 6u64)?;
    device
        .claim_interface(data_number, data_if.alternate_setting)
        .await
        .map_err(|_| 7u64)?;

    // 1_000_000 baud, 8 data bits, no parity, one stop bit。
    // dwDTERate 是 USB CDC 规定的小端 u32。
    let line_coding = [0x40, 0x42, 0x0f, 0x00, 0x00, 0x00, 0x08];
    cdc_control_out(&mut device, 0x20, 0, control_number as u16, &line_coding)
        .await
        .map_err(|_| 8u64)?;
    cdc_control_out(&mut device, 0x22, 0x0003, control_number as u16, &[])
        .await
        .map_err(|_| 9u64)?;

    let mut endpoints = device.take_endpoints().map_err(|_| 10u64)?;
    let endpoint_in = endpoints.remove(&in_address).ok_or(11u64)?;
    let endpoint_out = endpoints.remove(&out_address).ok_or(12u64)?;
    // CH34x CDC 固件在 SET_LINE_CODING / DTR / RTS 后需要短暂时间切换
    // 串口时钟和半双工方向状态；立即发送首帧可能只有 USB OUT 完成，
    // 但 UART 侧尚未准备好。pyserial 打开设备时也存在等价的驱动稳定期。
    crate::runtime::delay_ns(200_000_000);
    crate::runtime::puts(b"[libos] CDC ACM ready, baudrate=1000000\r\n");
    Ok(CdcAcmTransport {
        endpoint_in,
        endpoint_out,
        ftdi_packet_size: None,
    })
}

#[cfg(feature = "scservo")]
async fn configure_ftdi_scservo(
    host: &mut USBHost,
    info: &crab_usb::DeviceInfo,
) -> Result<CdcAcmTransport, u64> {
    let configuration = info
        .configurations()
        .iter()
        .find(|configuration| configuration.configuration_value == 1)
        .ok_or(20u64)?;
    let interface = configuration
        .interfaces
        .iter()
        .flat_map(|interface| interface.alt_settings.iter())
        .find(|interface| interface.interface_number == 0 && interface.alternate_setting == 0)
        .ok_or(21u64)?;
    let bulk_in = interface
        .endpoints
        .iter()
        .find(|endpoint| {
            endpoint.transfer_type == EndpointType::Bulk && endpoint.direction == Direction::In
        })
        .ok_or(22u64)?;
    let bulk_out = interface
        .endpoints
        .iter()
        .find(|endpoint| {
            endpoint.transfer_type == EndpointType::Bulk && endpoint.direction == Direction::Out
        })
        .ok_or(23u64)?;
    let in_address = bulk_in.address;
    let out_address = bulk_out.address;
    let packet_size = bulk_in.max_packet_size as usize;

    crate::runtime::puts(b"[libos] FTDI serial bridge mode\r\n");
    let mut device = host.open_device(info).await.map_err(|_| 24u64)?;
    device.set_configuration(1).await.map_err(|_| 25u64)?;
    device.claim_interface(0, 0).await.map_err(|_| 26u64)?;
    ftdi_control(&mut device, 0, 0).await?;
    // FTDI基准时钟为3 MHz，除数3选择1 Mbaud。
    ftdi_control(&mut device, 3, 3).await?;
    ftdi_control(&mut device, 4, 8).await?;
    ftdi_control(&mut device, 1, 0x0303).await?;

    let mut endpoints = device.take_endpoints().map_err(|_| 27u64)?;
    let endpoint_in = endpoints.remove(&in_address).ok_or(28u64)?;
    let endpoint_out = endpoints.remove(&out_address).ok_or(29u64)?;
    crate::runtime::delay_ns(200_000_000);
    crate::runtime::puts(b"[libos] FTDI bridge ready, baudrate=1000000\r\n");
    Ok(CdcAcmTransport {
        endpoint_in,
        endpoint_out,
        ftdi_packet_size: Some(packet_size),
    })
}

/// 显式诊断模式：CDC 原样回显，或者兼容 QEMU 的 FTDI 模拟设备。
#[cfg(feature = "usb-echo")]
async fn usb_echo(host: &mut USBHost, handler: &EventHandler) -> Result<(), u64> {
    crate::runtime::puts(b"[libos] usb-echo diagnostic mode\r\n");
    let devices = loop {
        let devices = host.probe_devices().await.map_err(|_| 40u64)?;
        if !devices.is_empty() {
            break devices;
        }
        handler.handle_event();
        crate::runtime::delay_ns(10_000_000);
    };

    for device in &devices {
        crate::runtime::puts(b"[libos] USB device VID=");
        crate::runtime::hex(device.vendor_id() as u64);
        crate::runtime::puts(b" PID=");
        crate::runtime::hex(device.product_id() as u64);
        crate::runtime::puts(b"\r\n");
    }

    for device in &devices {
        if let ProbedDevice::Device(info) = device {
            if find_cdc_interfaces(info).is_some() {
                let mut transport = configure_cdc_acm(host, info).await?;
                let mut input = [0u8; 512];
                loop {
                    let length = transport.read(&mut input).await.map_err(|_| 41u64)?;
                    if length != 0 {
                        transport.write(&input[..length]).await.map_err(|_| 42u64)?;
                    }
                }
            }
        }
    }

    for device in &devices {
        if let ProbedDevice::Device(info) = device {
            if info.vendor_id() == FTDI_VENDOR_ID && info.product_id() == FTDI_PRODUCT_ID {
                return ftdi_device_echo(host, info).await;
            }
        }
    }
    Err(43u64)
}

#[cfg(feature = "usb-echo")]
async fn ftdi_device_echo(host: &mut USBHost, info: &crab_usb::DeviceInfo) -> Result<(), u64> {
    let configuration = info
        .configurations()
        .iter()
        .find(|configuration| configuration.configuration_value == 1)
        .ok_or(20u64)?;
    let interface = configuration
        .interfaces
        .iter()
        .flat_map(|interface| interface.alt_settings.iter())
        .find(|interface| interface.interface_number == 0 && interface.alternate_setting == 0)
        .ok_or(21u64)?;
    let bulk_in = interface
        .endpoints
        .iter()
        .find(|endpoint| {
            endpoint.transfer_type == EndpointType::Bulk && endpoint.direction == Direction::In
        })
        .ok_or(22u64)?;
    let bulk_out = interface
        .endpoints
        .iter()
        .find(|endpoint| {
            endpoint.transfer_type == EndpointType::Bulk && endpoint.direction == Direction::Out
        })
        .ok_or(23u64)?;
    let in_address = bulk_in.address;
    let out_address = bulk_out.address;
    let packet_size = bulk_in.max_packet_size as usize;

    crate::runtime::puts(b"[libos] FTDI compatibility mode\r\n");
    let mut device = host.open_device(info).await.map_err(|_| 24u64)?;
    device.set_configuration(1).await.map_err(|_| 25u64)?;
    device.claim_interface(0, 0).await.map_err(|_| 26u64)?;
    ftdi_control(&mut device, 0, 0).await?;
    ftdi_control(&mut device, 3, 26).await?;
    ftdi_control(&mut device, 4, 8).await?;
    ftdi_control(&mut device, 1, 0x0303).await?;

    let mut endpoints = device.take_endpoints().map_err(|_| 27u64)?;
    let mut endpoint_in = endpoints.remove(&in_address).ok_or(28u64)?;
    let mut endpoint_out = endpoints.remove(&out_address).ok_or(29u64)?;
    crate::runtime::puts(b"[libos] FTDI 0403:6001 ready\r\n");

    let mut input = [0u8; 512];
    loop {
        let completion = endpoint_in
            .wait(TransferRequest::bulk_in(&mut input))
            .await
            .map_err(|_| 30u64)?;
        let length = completion.actual_length.min(input.len());
        let mut payload = Vec::with_capacity(length);
        for packet in input[..length].chunks(packet_size.max(2)) {
            if packet.len() > 2 {
                payload.extend_from_slice(&packet[2..]);
            }
        }
        if !payload.is_empty() {
            endpoint_out
                .wait(TransferRequest::bulk_out(&payload))
                .await
                .map_err(|_| 31u64)?;
        }
    }
}

async fn ftdi_control(device: &mut crab_usb::Device, request: u8, value: u16) -> Result<(), u64> {
    device
        .control_out(
            ControlSetup {
                request_type: RequestType::Vendor,
                recipient: Recipient::Device,
                request: Request::Other(request),
                value,
                index: 0,
            },
            &[],
        )
        .await
        .map(|_| ())
        .map_err(|_| 32 + request as u64)
}

fn find_cdc_interfaces<'a>(
    info: &'a crab_usb::DeviceInfo,
) -> Option<(
    &'a crab_usb::usb_if::descriptor::ConfigurationDescriptor,
    &'a crab_usb::usb_if::descriptor::InterfaceDescriptor,
    &'a crab_usb::usb_if::descriptor::InterfaceDescriptor,
    &'a crab_usb::usb_if::descriptor::EndpointDescriptor,
    &'a crab_usb::usb_if::descriptor::EndpointDescriptor,
)> {
    for configuration in info.configurations() {
        let Some(control) = configuration
            .interfaces
            .iter()
            .flat_map(|interface| interface.alt_settings.iter())
            .find(|interface| {
                interface.class == CDC_CONTROL_CLASS
                    && interface.subclass == CDC_CONTROL_SUBCLASS
                    && interface.protocol == CDC_CONTROL_PROTOCOL
            })
        else {
            continue;
        };
        let Some(data) = configuration
            .interfaces
            .iter()
            .flat_map(|interface| interface.alt_settings.iter())
            .find(|interface| interface.class == CDC_DATA_CLASS)
        else {
            continue;
        };
        let Some(bulk_in) = data.endpoints.iter().find(|endpoint| {
            endpoint.transfer_type == EndpointType::Bulk && endpoint.direction == Direction::In
        }) else {
            continue;
        };
        let Some(bulk_out) = data.endpoints.iter().find(|endpoint| {
            endpoint.transfer_type == EndpointType::Bulk && endpoint.direction == Direction::Out
        }) else {
            continue;
        };
        return Some((configuration, control, data, bulk_in, bulk_out));
    }
    None
}

async fn cdc_control_out(
    device: &mut crab_usb::Device,
    request: u8,
    value: u16,
    interface: u16,
    payload: &[u8],
) -> Result<(), crab_usb::usb_if::err::TransferError> {
    device
        .control_out(
            ControlSetup {
                request_type: RequestType::Class,
                recipient: Recipient::Interface,
                request: Request::Other(request),
                value,
                index: interface,
            },
            payload,
        )
        .await
        .map(|_| ())
}

pub(crate) fn block_on_usb<F: Future>(
    future: F,
    handler: &EventHandler,
    expected_intid: u32,
    notification: &crate::notification::Notification,
) -> F::Output {
    let waker = unsafe { Waker::from_raw(raw_waker()) };
    let mut context = Context::from_waker(&waker);
    let mut future = core::pin::pin!(future);
    loop {
        WOKEN.store(false, Ordering::Release);
        match Future::poll(Pin::as_mut(&mut future), &mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending if WOKEN.swap(false, Ordering::AcqRel) => {
                // 自唤醒 Future 以轮询方式实现有界超时；硬件 completion
                // 仍需在这里从 xHCI Event Ring 取出并唤醒端点请求。
                handler.handle_event();
                core::hint::spin_loop();
                continue;
            }
            Poll::Pending => {
                let badge = notification
                    .wait()
                    .unwrap_or_else(|_| fail("xHCI Notification wait failed", 0x20c));
                if badge & XHCI_IRQ_BADGE != 0 {
                    handler.handle_event();
                    crate::runtime::irq_ack(expected_intid);
                }
            }
        }
    }
}

static WOKEN: AtomicBool = AtomicBool::new(false);

unsafe fn clone_waker(_: *const ()) -> RawWaker {
    raw_waker()
}
unsafe fn wake(_: *const ()) {
    WOKEN.store(true, Ordering::Release);
}
unsafe fn wake_by_ref(_: *const ()) {
    WOKEN.store(true, Ordering::Release);
}
unsafe fn drop_waker(_: *const ()) {}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake, wake_by_ref, drop_waker);

fn raw_waker() -> RawWaker {
    RawWaker::new(core::ptr::null(), &WAKER_VTABLE)
}

fn fail(message: &'static str, code: u64) -> ! {
    crate::runtime::puts(b"[libos] ");
    crate::runtime::puts(message.as_bytes());
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(code)
}
