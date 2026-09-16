//! USB串口类驱动：CDC ACM与FTDI。
//!
//! 本模块读取USB descriptor、设置configuration/interface和控制请求，最终
//! 只向上层暴露异步字节流。它不知道SCServo策略，也不决定收到数据后是否回显。

use crab_usb::usb_if::{
    descriptor::EndpointType,
    endpoint::TransferRequest,
    err::TransferError,
    host::ControlSetup,
    transfer::{Direction, Recipient, Request, RequestType},
};
#[cfg(any(
    feature = "ide",
    feature = "app-scservo",
    feature = "app-robot-act-once",
    feature = "app-robot-observation",
    feature = "app-robot-action-replay"
))]
use crab_usb::EventHandler;
use crab_usb::{Endpoint, ProbedDevice, USBHost};

const CDC_CONTROL_CLASS: u8 = 0x02;
const CDC_CONTROL_SUBCLASS: u8 = 0x02;
const CDC_CONTROL_PROTOCOL: u8 = 0x01;
const CDC_DATA_CLASS: u8 = 0x0a;
const FTDI_VENDOR_ID: u16 = 0x0403;
const FTDI_PRODUCT_ID: u16 = 0x6001;

/// CDC ACM或FTDI配置完成后的Bulk字节流。
pub(crate) struct UsbSerialTransport {
    /// 设备到Host方向的数据端点，端点地址最高位通常为1。
    endpoint_in: Endpoint,
    /// Host到设备方向的数据端点，端点地址最高位为0。
    endpoint_out: Endpoint,
    /// FTDI每个Bulk IN packet开头有两个modem/status字节。
    ftdi_packet_size: Option<usize>,
}

impl UsbSerialTransport {
    /// 提交一次Bulk IN，并返回设备实际写入的payload长度。
    pub(crate) async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, TransferError> {
        // CDC ACM没有额外状态头，可直接让xHCI DMA写入调用者buffer。
        let Some(packet_size) = self.ftdi_packet_size else {
            return Ok(self
                .endpoint_in
                .wait(TransferRequest::bulk_in(buffer))
                .await?
                .actual_length
                .min(buffer.len()));
        };

        // FTDI状态头要按每个USB packet删除，不能只删整个transfer前两字节。
        let mut usb_packet = [0u8; 512];
        let length = self
            .endpoint_in
            .wait(TransferRequest::bulk_in(&mut usb_packet))
            .await?
            .actual_length
            .min(usb_packet.len());
        // copied只统计已经交给上层的纯串口payload。
        let mut copied = 0;
        for packet in usb_packet[..length].chunks(packet_size.max(2)) {
            // 只有两个状态字节时表示这个packet没有串口数据。
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
        // Bulk OUT不携带FTDI状态头，上层payload可直接提交。
        Ok(self
            .endpoint_out
            .wait(TransferRequest::bulk_out(buffer))
            .await?
            .actual_length
            .min(buffer.len()))
    }

    /// 半双工总线严格先提交Bulk OUT，再提交Bulk IN等待状态包。
    pub(crate) async fn exchange(
        &mut self,
        output: &[u8],
        input: &mut [u8],
    ) -> Result<(usize, usize), TransferError> {
        // OUT完成只表示串口桥收到命令，并不表示舵机状态包已经返回。
        let written = self
            .endpoint_out
            .wait(TransferRequest::bulk_out(output))
            .await?
            .actual_length
            .min(output.len());

        // 两次传输之间不输出UART日志，避免拖慢短响应的读取。
        let read = self.read(input).await?;
        Ok((written, read))
    }
}

/// 为SCServo寻找CDC ACM设备；诊断时也兼容QEMU模拟FTDI。
#[cfg(any(
    feature = "ide",
    feature = "app-scservo",
    feature = "app-robot-act-once",
    feature = "app-robot-observation",
    feature = "app-robot-action-replay"
))]
pub(crate) async fn open_scservo_transport(
    host: &mut USBHost,
    handler: &EventHandler,
) -> Result<UsbSerialTransport, u64> {
    open_scservo_transport_filtered(host, handler, None).await
}

#[cfg(any(
    feature = "ide",
    feature = "app-scservo",
    feature = "app-robot-act-once",
    feature = "app-robot-observation",
    feature = "app-robot-action-replay"
))]
async fn open_scservo_transport_filtered(
    host: &mut USBHost,
    handler: &EventHandler,
    root_port: Option<u8>,
) -> Result<UsbSerialTransport, u64> {
    crate::runtime::puts(b"[libos] probing USB devices\r\n");
    // 允许设备晚于xHCI启动插入，所以没有设备时持续探测。
    let devices = loop {
        let devices = host.probe_devices().await.map_err(|_| 1u64)?;
        if !devices.is_empty() {
            break devices;
        }
        handler.handle_event();
        crate::runtime::delay_ns(10_000_000);
    };

    // 这里只保存descriptor引用；真正open和claim在完成选择后执行一次。
    let mut selected_cdc = None;
    let mut selected_ftdi = None;
    for device in &devices {
        log_device(device);
        if let ProbedDevice::Device(info) = device {
            if root_port.map_or(true, |port| device.root_port_id() == Some(port))
                && selected_cdc.is_none()
                && is_cdc_acm(info)
            {
                selected_cdc = Some(info);
            }
            if root_port.map_or(true, |port| device.root_port_id() == Some(port))
                && selected_ftdi.is_none()
                && is_ftdi(info)
            {
                selected_ftdi = Some(info);
            }
        }
    }

    // 真实串口桥优先按CDC ACM标准类进行配置。
    if let Some(info) = selected_cdc {
        return configure_cdc_acm(host, info).await;
    }
    // QEMU模拟设备退回FTDI vendor request路径，除数3对应1Mbaud。
    if let Some(info) = selected_ftdi {
        return configure_ftdi(host, info, 3, true).await;
    }
    Err(2)
}

/// 使用已经完成枚举的DeviceInfo配置串口。
///
/// 机器人场景一次性取得三个root-port设备后调用此函数，避免再次探测时
/// 由于端口变化标志已经被消费而看不到串口板。
#[cfg(any(feature = "app-robot-act-once", feature = "app-robot-observation"))]
pub(crate) async fn configure_scservo(
    host: &mut USBHost,
    info: &crab_usb::DeviceInfo,
) -> Result<UsbSerialTransport, u64> {
    if is_cdc_acm(info) {
        configure_cdc_acm(host, info).await
    } else if is_ftdi(info) {
        configure_ftdi(host, info, 3, true).await
    } else {
        Err(2)
    }
}

pub(crate) fn log_device(device: &ProbedDevice) {
    // VID/PID来自Device Descriptor，由CrabUSB完成小端解析。
    crate::runtime::puts(b"[libos] USB device VID=");
    crate::runtime::hex(device.vendor_id() as u64);
    crate::runtime::puts(b" PID=");
    crate::runtime::hex(device.product_id() as u64);
    crate::runtime::puts(b"\r\n");
}

pub(crate) fn is_cdc_acm(info: &crab_usb::DeviceInfo) -> bool {
    // 必须同时找到控制接口、数据接口和一对Bulk端点。
    find_cdc_interfaces(info).is_some()
}

pub(crate) fn is_ftdi(info: &crab_usb::DeviceInfo) -> bool {
    // QEMU usb-serial固定模拟FT232BM的0403:6001。
    info.vendor_id() == FTDI_VENDOR_ID && info.product_id() == FTDI_PRODUCT_ID
}

/// 配置CDC ACM为1,000,000 baud、8N1并置位DTR/RTS。
pub(crate) async fn configure_cdc_acm(
    host: &mut USBHost,
    info: &crab_usb::DeviceInfo,
) -> Result<UsbSerialTransport, u64> {
    // descriptor决定端点地址，不能假设所有CDC设备都是0x82/0x02。
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

    // open_device建立CrabUSB设备对象；默认Control Endpoint已经可用。
    let mut device = host.open_device(info).await.map_err(|_| 4u64)?;
    // SET_CONFIGURATION让设备进入descriptor声明的活动配置。
    device
        .set_configuration(configuration.configuration_value)
        .await
        .map_err(|_| 5u64)?;
    // 控制接口承载CDC class request和可选Interrupt IN状态通知。
    device
        .claim_interface(control_number, control_if.alternate_setting)
        .await
        .map_err(|_| 6u64)?;
    // 数据接口承载实际串口Bulk IN/OUT。
    device
        .claim_interface(data_number, data_if.alternate_setting)
        .await
        .map_err(|_| 7u64)?;

    // CDC line coding: 小端u32波特率、1 stop、no parity、8 data bits。
    let line_coding = [0x40, 0x42, 0x0f, 0x00, 0x00, 0x00, 0x08];
    // bRequest=0x20 SET_LINE_CODING，wIndex指定控制接口号。
    cdc_control_out(&mut device, 0x20, 0, control_number as u16, &line_coding)
        .await
        .map_err(|_| 8u64)?;
    // bRequest=0x22 SET_CONTROL_LINE_STATE，bit0 DTR、bit1 RTS。
    cdc_control_out(&mut device, 0x22, 0x0003, control_number as u16, &[])
        .await
        .map_err(|_| 9u64)?;

    // 转移端点所有权，防止同一Endpoint被多个上层对象同时提交请求。
    let mut endpoints = device.take_endpoints().map_err(|_| 10u64)?;
    let endpoint_in = endpoints.remove(&in_address).ok_or(11u64)?;
    let endpoint_out = endpoints.remove(&out_address).ok_or(12u64)?;
    crate::runtime::delay_ns(200_000_000);
    crate::runtime::puts(b"[libos] CDC ACM ready, baudrate=1000000\r\n");
    Ok(UsbSerialTransport {
        endpoint_in,
        endpoint_out,
        ftdi_packet_size: None,
    })
}

/// 配置FTDI。SCServo使用3对应1Mbaud；QEMU回显使用26对应115200。
pub(crate) async fn configure_ftdi(
    host: &mut USBHost,
    info: &crab_usb::DeviceInfo,
    baud_divisor: u16,
    settle: bool,
) -> Result<UsbSerialTransport, u64> {
    // FTDI模拟设备是configuration 1/interface 0，但端点仍动态解析。
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

    let mut device = host.open_device(info).await.map_err(|_| 24u64)?;
    device.set_configuration(1).await.map_err(|_| 25u64)?;
    device.claim_interface(0, 0).await.map_err(|_| 26u64)?;
    // SIO_RESET清空FTDI串口状态。
    ftdi_control(&mut device, 0, 0).await?;
    // SIO_SET_BAUD_RATE，value是FTDI基准时钟除数。
    ftdi_control(&mut device, 3, baud_divisor).await?;
    // SIO_SET_DATA，value=8代表8 data bits、无校验、1 stop。
    ftdi_control(&mut device, 4, 8).await?;
    // SIO_MODEM_CTRL，使DTR和RTS都有效并置位。
    ftdi_control(&mut device, 1, 0x0303).await?;

    let mut endpoints = device.take_endpoints().map_err(|_| 27u64)?;
    let endpoint_in = endpoints.remove(&in_address).ok_or(28u64)?;
    let endpoint_out = endpoints.remove(&out_address).ok_or(29u64)?;
    if settle {
        crate::runtime::delay_ns(200_000_000);
        crate::runtime::puts(b"[libos] FTDI bridge ready, baudrate=1000000\r\n");
    } else {
        crate::runtime::puts(b"[libos] FTDI 0403:6001 ready\r\n");
    }
    Ok(UsbSerialTransport {
        endpoint_in,
        endpoint_out,
        ftdi_packet_size: Some(packet_size),
    })
}

async fn ftdi_control(device: &mut crab_usb::Device, request: u8, value: u16) -> Result<(), u64> {
    // bmRequestType为Host-to-Device、Vendor、Device。
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

fn find_cdc_interfaces(
    info: &crab_usb::DeviceInfo,
) -> Option<(
    &crab_usb::usb_if::descriptor::ConfigurationDescriptor,
    &crab_usb::usb_if::descriptor::InterfaceDescriptor,
    &crab_usb::usb_if::descriptor::InterfaceDescriptor,
    &crab_usb::usb_if::descriptor::EndpointDescriptor,
    &crab_usb::usb_if::descriptor::EndpointDescriptor,
)> {
    // 一个设备可能有多个configuration和alternate setting，逐层扫描。
    for configuration in info.configurations() {
        // CDC ACM控制接口的标准class/subclass/protocol为02/02/01。
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
        // 数据接口class为0x0a，不能依赖它一定紧邻控制接口。
        let Some(data) = configuration
            .interfaces
            .iter()
            .flat_map(|interface| interface.alt_settings.iter())
            .find(|interface| interface.class == CDC_DATA_CLASS)
        else {
            continue;
        };
        // Interrupt endpoint只报告状态，串口payload必须使用Bulk端点。
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
) -> Result<(), TransferError> {
    // CrabUSB会把ControlSetup展开为SETUP/DATA/STATUS阶段的xHCI TRB。
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
