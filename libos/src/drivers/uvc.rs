//! USB Video Class (UVC)用户态驱动。
//!
//! 本模块位于CrabUSB之上：CrabUSB负责xHCI命令、TRB、DMA与Event Ring，
//! 本模块负责解析UVC class-specific descriptor、执行Probe/Commit，并把多个
//! 等时USB payload按FID/EOF重组为一帧。EL1只参与启动时MMIO/DMA/IRQ授权；
//! 摄像头开始传输后，视频数据由xHCI直接DMA到EL0已经映射的缓冲区。

use alloc::{vec, vec::Vec};

use crate::memory::{Frame, Mapping, Rights};

use crab_usb::usb_if::{
    descriptor::EndpointType,
    endpoint::{TransferRequest, TransferStatus},
    err::TransferError,
    host::ControlSetup,
    transfer::{Direction, Recipient, Request, RequestType},
};
use crab_usb::{Device, Endpoint, EventHandler, ProbedDevice, USBHost};

const USB_CLASS_VIDEO: u8 = 0x0e;
const UVC_SUBCLASS_CONTROL: u8 = 0x01;
const UVC_SUBCLASS_STREAMING: u8 = 0x02;
const USB_DT_INTERFACE: u8 = 0x04;
const USB_DT_CS_INTERFACE: u8 = 0x24;
const UVC_VC_HEADER: u8 = 0x01;
const UVC_VS_FORMAT_UNCOMPRESSED: u8 = 0x04;
const UVC_VS_FRAME_UNCOMPRESSED: u8 = 0x05;
const UVC_VS_FORMAT_MJPEG: u8 = 0x06;
const UVC_VS_FRAME_MJPEG: u8 = 0x07;
const UVC_SET_CUR: u8 = 0x01;
const UVC_GET_CUR: u8 = 0x81;
const UVC_VS_PROBE_CONTROL: u16 = 0x01;
const UVC_VS_COMMIT_CONTROL: u16 = 0x02;
const UVC_HEADER_FID: u8 = 1 << 0;
const UVC_HEADER_EOF: u8 = 1 << 1;
const UVC_HEADER_ERR: u8 = 1 << 6;
const ISO_PACKETS_PER_TRANSFER: usize = 32;
/// UVC单帧实验使用Frame arena起始地址；该实验不与其他Frame应用同时运行。
const UVC_FRAME_VA: u64 = exo_abi::FRAME_ARENA_BASE;

/// UVC smoke优先选择的ACT输入分辨率。
const TARGET_WIDTH: u16 = 640;
const TARGET_HEIGHT: u16 = 360;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PixelFormat {
    Mjpeg,
    Uncompressed,
}

impl PixelFormat {
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            Self::Mjpeg => b"MJPEG",
            Self::Uncompressed => b"uncompressed",
        }
    }
}

/// 从VS class descriptor提取的一个可协商视频模式。
#[derive(Clone, Copy)]
struct VideoMode {
    format: PixelFormat,
    format_index: u8,
    frame_index: u8,
    width: u16,
    height: u16,
    /// 单位为100ns；333333约等于30fps。
    interval_100ns: u32,
    max_frame_size: u32,
}

/// 一个alternate setting中的视频输入端点。
#[derive(Clone, Copy)]
struct StreamingAlt {
    alternate: u8,
    address: u8,
    transfer_type: EndpointType,
    /// 每个xHCI服务周期最多传输的字节数。
    payload_capacity: usize,
}

/// 完成Probe/Commit和SET_INTERFACE后的摄像头流。
pub(crate) struct UvcStream {
    endpoint: Option<Endpoint>,
    streaming_if: u8,
    transfer_type: EndpointType,
    payload_size: usize,
    max_frame_size: usize,
    pub(crate) format: PixelFormat,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) interval_100ns: u32,
}

/// 一帧已经去除UVC头部的图像数据。
///
/// `backing`保存EL1分配的普通物理页Handle，`mapping`保存这些页在当前
/// VSpace中的映射。二者必须活到调用者完成哈希和磁盘写入之后，设备收到的
/// 数据则通过CrabUSB DMA bounce复制到这块Normal Cacheable Frame。
pub(crate) struct CapturedFrame {
    _backing: Option<Frame>,
    mapping: Option<Mapping>,
    len: usize,
}

/// 采集过程中的临时资源守卫。
///
/// UVC传输可能在EOF前因超时、坏包或xHCI错误返回；Guard保证这些错误路径
/// 也会执行UNMAP/FREE，而不是只依赖任务退出时的Kernel兜底回收。
struct CaptureResources {
    backing: Option<Frame>,
    mapping: Option<Mapping>,
}

impl CaptureResources {
    fn into_frame(mut self, len: usize) -> CapturedFrame {
        CapturedFrame {
            _backing: self.backing.take(),
            mapping: self.mapping.take(),
            len,
        }
    }
}

impl Drop for CaptureResources {
    fn drop(&mut self) {
        if let Some(mapping) = self.mapping.take() {
            let unmapped = mapping.unmap().is_ok();
            if unmapped {
                if let Some(backing) = self.backing.take() {
                    let _ = backing.free();
                }
            }
        }
    }
}

impl CapturedFrame {
    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: mapping由Kernel在调用者指定的Frame arena VA建立为当前VSpace的RW映射；
        // len只会在capacity范围内增长，且self持有Frame和Mapping的生命周期。
        unsafe { core::slice::from_raw_parts(self.mapping.as_ref().unwrap().as_ptr(), self.len) }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// 显式撤销本帧的VA映射并释放backing Frame。
    pub(crate) fn release(self) -> Result<(), u64> {
        let mut this = self;
        let mapping = this.mapping.take().unwrap();
        let backing = this._backing.take().unwrap();
        let result = mapping.unmap();
        if result.is_ok() {
            backing.free()
        } else {
            result
        }
    }
}

impl Drop for CapturedFrame {
    fn drop(&mut self) {
        // 失败路径也必须撤销映射；任务退出的全量回收只是最后一道兜底。
        if let Some(mapping) = self.mapping.take() {
            let unmapped = mapping.unmap().is_ok();
            if unmapped {
                if let Some(backing) = self._backing.take() {
                    let _ = backing.free();
                }
            }
        }
    }
}

impl UvcStream {
    /// 接收并重组一帧；返回值只包含图像payload，不包含UVC payload header。
    pub(crate) async fn capture_frame(&mut self) -> Result<CapturedFrame, UvcError> {
        self.capture_frame_at(UVC_FRAME_VA).await
    }

    /// 在调用者指定的Frame arena VA采集一帧，允许两个摄像头同时保留缓冲。
    pub(crate) async fn capture_frame_at(
        &mut self,
        frame_va: u64,
    ) -> Result<CapturedFrame, UvcError> {
        // 帧缓冲不占用固定4MiB global heap，而是通过Frame系统调用批量申请。
        // 映射完成后，下面组帧只写EL0 VA，不会为每个payload再次进入Kernel。
        let capacity = self.max_frame_size.min(2 * 1024 * 1024).max(64 * 1024);
        let pages = capacity.div_ceil(exo_abi::PAGE_SIZE as usize) as u64;
        let backing = Frame::allocate(pages, 1).map_err(|_| UvcError::NoMemory)?;
        let mapping = match backing.map(0, pages, frame_va, Rights::READ_WRITE) {
            Ok(mapping) => mapping,
            Err(_) => {
                let _ = backing.free();
                return Err(UvcError::NoMemory);
            }
        };
        let capacity = mapping.len();
        let frame_ptr = mapping.as_ptr();
        let resources = CaptureResources {
            backing: Some(backing),
            mapping: Some(mapping),
        };
        // SAFETY: Kernel已经校验Frame所有权、VA arena、空PTE和RW权限；
        // mapping由本函数独占，直到封装进CapturedFrame前不存在别名可变引用。
        let frame = unsafe { core::slice::from_raw_parts_mut(frame_ptr, capacity) };
        let mut frame_len = 0usize;
        let mut active_fid = None;

        // 每轮复用同一普通内存buffer，CrabUSB会为每个请求建立临时DMA
        // bounce。复用可避免长时间流传输导致heap碎片。
        let packet_size = self.payload_size.max(64);
        let packet_count = if self.transfer_type == EndpointType::Isochronous {
            ISO_PACKETS_PER_TRANSFER
        } else {
            1
        };
        let mut transfer_buffer = vec![0u8; packet_size * packet_count];
        let iso_lengths = vec![packet_size; packet_count];

        // 一帧最多允许4096次USB请求，防止损坏设备永远不发送EOF而占住线程。
        for _ in 0..4096 {
            let completion = if self.transfer_type == EndpointType::Isochronous {
                self.endpoint
                    .as_mut()
                    .ok_or(UvcError::Transfer)?
                    .wait(TransferRequest::iso_in(&mut transfer_buffer, &iso_lengths))
                    .await?
            } else {
                self.endpoint
                    .as_mut()
                    .ok_or(UvcError::Transfer)?
                    .wait(TransferRequest::bulk_in(&mut transfer_buffer))
                    .await?
            };

            if completion.status != TransferStatus::Completed {
                return Err(UvcError::Transfer);
            }

            if self.transfer_type == EndpointType::Isochronous {
                // 每个Iso TRB占用固定的requested stride；即使短包，下一包仍从
                // 下一个stride开始，不能用actual_length简单地连续切片。
                for (index, packet) in completion.iso_packets.iter().enumerate() {
                    if packet.status != TransferStatus::Completed {
                        continue;
                    }
                    let start = index * packet_size;
                    let end = start
                        .saturating_add(packet.actual_length)
                        .min(transfer_buffer.len());
                    if self.consume_payload(
                        &transfer_buffer[start..end],
                        frame,
                        &mut frame_len,
                        &mut active_fid,
                    )? {
                        return Ok(resources.into_frame(frame_len));
                    }
                }
            } else {
                let length = completion.actual_length.min(transfer_buffer.len());
                if self.consume_payload(
                    &transfer_buffer[..length],
                    frame,
                    &mut frame_len,
                    &mut active_fid,
                )? {
                    return Ok(resources.into_frame(frame_len));
                }
            }
        }
        Err(UvcError::FrameTimeout)
    }

    /// 停止视频DMA。
    ///
    /// 当前CrabUSB的一个Device对应一个xHCI设备上下文。UVC和CDC ACM
    /// 可能同时属于不同接口，但`SET_INTERFACE`会重新评估这个共享上下文，
    /// 可能使仍在使用的CDC endpoint失效。因此这里采用更小的停止语义：
    /// 释放UVC endpoint，之后不再提交任何视频传输；保留alternate setting
    /// 不会产生DMA，也不会触碰CDC已经建立的endpoint。
    pub(crate) async fn stop(&mut self) -> Result<(), UvcError> {
        // Endpoint持有UVC transfer ring；显式drop后，后续不会再有UVC请求。
        // 不能在这里再次调用SET_INTERFACE，否则会修改共享的xHCI设备上下文。
        drop(self.endpoint.take());
        Ok(())
    }

    fn consume_payload(
        &self,
        packet: &[u8],
        frame: &mut [u8],
        frame_len: &mut usize,
        active_fid: &mut Option<u8>,
    ) -> Result<bool, UvcError> {
        if packet.len() < 2 {
            return Ok(false);
        }
        let header_len = packet[0] as usize;
        let flags = packet[1];
        if header_len < 2 || header_len > packet.len() {
            return Err(UvcError::BadPayloadHeader);
        }
        if flags & UVC_HEADER_ERR != 0 {
            *frame_len = 0;
            *active_fid = None;
            return Ok(false);
        }

        let fid = flags & UVC_HEADER_FID;
        match *active_fid {
            None => *active_fid = Some(fid),
            Some(old) if old != fid => {
                // FID翻转说明上一帧没有可靠EOF。丢弃残帧，从当前payload重来。
                *frame_len = 0;
                *active_fid = Some(fid);
            }
            _ => {}
        }
        let payload = &packet[header_len..];
        let end = frame_len
            .checked_add(payload.len())
            .ok_or(UvcError::FrameTooLarge)?;
        if end > self.max_frame_size.max(1) || end > frame.len() {
            return Err(UvcError::FrameTooLarge);
        }
        frame[*frame_len..end].copy_from_slice(payload);
        *frame_len = end;
        Ok(flags & UVC_HEADER_EOF != 0 && *frame_len != 0)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum UvcError {
    Probe,
    NoCamera,
    NoVideoMode,
    NoStreamingEndpoint,
    Control,
    Transfer,
    BadPayloadHeader,
    FrameTooLarge,
    FrameTimeout,
    NoMemory,
}

impl From<TransferError> for UvcError {
    fn from(_: TransferError) -> Self {
        Self::Transfer
    }
}

/// 探测并配置第一台UVC摄像头。
pub(crate) async fn open_first_camera(
    host: &mut USBHost,
    handler: &EventHandler,
) -> Result<UvcStream, UvcError> {
    crate::runtime::puts(b"[libos] probing UVC devices\r\n");
    let devices = loop {
        let devices = host.probe_devices().await.map_err(|_| UvcError::Probe)?;
        if !devices.is_empty() {
            break devices;
        }
        handler.handle_event();
        crate::runtime::delay_ns(10_000_000);
    };

    let mut selected = None;
    for candidate in &devices {
        log_device(candidate);
        let ProbedDevice::Device(info) = candidate else {
            continue;
        };
        if selected.is_none() && contains_uvc_streaming_interface(info) {
            selected = Some(info);
        }
    }
    let info = selected.ok_or(UvcError::NoCamera)?;
    configure_camera(host, info).await
}

pub(crate) async fn configure_camera(
    host: &mut USBHost,
    info: &crab_usb::DeviceInfo,
) -> Result<UvcStream, UvcError> {
    let configuration = info
        .configurations()
        .iter()
        .find(|config| contains_uvc_raw(&config.raw))
        .ok_or(UvcError::NoCamera)?;
    let raw = &configuration.raw;
    let control_if = find_interface(raw, UVC_SUBCLASS_CONTROL).ok_or(UvcError::NoCamera)?;
    let streaming_if = find_interface(raw, UVC_SUBCLASS_STREAMING).ok_or(UvcError::NoCamera)?;
    let uvc_version = find_uvc_version(raw, control_if).unwrap_or(0x0110);
    let mode = select_mode(raw, streaming_if).ok_or(UvcError::NoVideoMode)?;
    let alts = collect_streaming_alts(configuration, streaming_if);
    if alts.is_empty() {
        return Err(UvcError::NoStreamingEndpoint);
    }

    log_mode(&mode, streaming_if, uvc_version);
    let mut device = host.open_device(info).await.map_err(|_| UvcError::Probe)?;
    device
        .set_configuration(configuration.configuration_value)
        .await
        .map_err(|_| UvcError::Control)?;
    // Alternate 0通常没有数据端点，适合执行带宽协商。SET_INTERFACE和后续
    // class request均通过EP0完成，尚未开始视频DMA。
    device
        .claim_interface(streaming_if, 0)
        .await
        .map_err(|_| UvcError::Control)?;

    let control_size = if uvc_version < 0x0110 {
        26
    } else if uvc_version < 0x0150 {
        34
    } else {
        48
    };
    let mut probe = [0u8; 48];
    put_u16(&mut probe, 0, 1); // bmHint bit0：固定使用请求的frame interval。
    probe[2] = mode.format_index;
    probe[3] = mode.frame_index;
    put_u32(&mut probe, 4, mode.interval_100ns);
    put_u32(&mut probe, 18, mode.max_frame_size);

    uvc_control_out(
        &mut device,
        UVC_SET_CUR,
        UVC_VS_PROBE_CONTROL,
        streaming_if,
        &probe[..control_size],
    )
    .await?;
    uvc_control_in(
        &mut device,
        UVC_GET_CUR,
        UVC_VS_PROBE_CONTROL,
        streaming_if,
        &mut probe[..control_size],
    )
    .await?;
    // 设备可修正格式、间隔、帧上限和带宽，Commit必须回送修正后的结构。
    uvc_control_out(
        &mut device,
        UVC_SET_CUR,
        UVC_VS_COMMIT_CONTROL,
        streaming_if,
        &probe[..control_size],
    )
    .await?;

    let negotiated_frame_size = get_u32(&probe, 18).max(mode.max_frame_size) as usize;
    let negotiated_payload = get_u32(&probe, 22) as usize;
    let selected_alt =
        select_alt(&alts, negotiated_payload).ok_or(UvcError::NoStreamingEndpoint)?;
    log_negotiated(selected_alt, negotiated_frame_size, negotiated_payload);

    // SET_INTERFACE切换到有带宽的alternate。CrabUSB同时创建xHCI endpoint
    // context和transfer ring；此后摄像头可向该IN端点发送视频payload。
    device
        .claim_interface(streaming_if, selected_alt.alternate)
        .await
        .map_err(|_| UvcError::Control)?;
    let mut endpoints = device
        .take_endpoints_for_interface(streaming_if)
        .map_err(|_| UvcError::NoStreamingEndpoint)?;
    let endpoint = endpoints
        .remove(&selected_alt.address)
        .ok_or(UvcError::NoStreamingEndpoint)?;

    Ok(UvcStream {
        endpoint: Some(endpoint),
        streaming_if,
        transfer_type: selected_alt.transfer_type,
        // Iso端点每个服务周期不能超过alternate声明的容量；Bulk没有周期带宽
        // 上限，可以使用设备协商的payload大小提高单次传输效率。
        payload_size: if selected_alt.transfer_type == EndpointType::Isochronous {
            selected_alt.payload_capacity
        } else {
            selected_alt.payload_capacity.max(negotiated_payload)
        },
        max_frame_size: negotiated_frame_size.max(1),
        format: mode.format,
        width: mode.width,
        height: mode.height,
        interval_100ns: get_u32(&probe, 4),
    })
}

pub(crate) fn contains_uvc_streaming_interface(info: &crab_usb::DeviceInfo) -> bool {
    info.configurations()
        .iter()
        .any(|config| contains_uvc_raw(&config.raw))
}

fn log_device(device: &ProbedDevice) {
    crate::runtime::puts(b"[libos] USB device VID=");
    crate::runtime::hex(device.vendor_id() as u64);
    crate::runtime::puts(b" PID=");
    crate::runtime::hex(device.product_id() as u64);
    crate::runtime::puts(b"\r\n");
}

fn contains_uvc_raw(raw: &[u8]) -> bool {
    find_interface(raw, UVC_SUBCLASS_STREAMING).is_some()
}

fn find_interface(raw: &[u8], subclass: u8) -> Option<u8> {
    descriptor_iter(raw).find_map(|descriptor| {
        (descriptor.len() >= 9
            && descriptor[1] == USB_DT_INTERFACE
            && descriptor[5] == USB_CLASS_VIDEO
            && descriptor[6] == subclass)
            .then_some(descriptor[2])
    })
}

fn find_uvc_version(raw: &[u8], control_if: u8) -> Option<u16> {
    let mut current_interface = None;
    for descriptor in descriptor_iter(raw) {
        if descriptor[1] == USB_DT_INTERFACE && descriptor.len() >= 9 {
            current_interface = Some(descriptor[2]);
        } else if current_interface == Some(control_if)
            && descriptor[1] == USB_DT_CS_INTERFACE
            && descriptor.len() >= 5
            && descriptor[2] == UVC_VC_HEADER
        {
            return Some(u16::from_le_bytes([descriptor[3], descriptor[4]]));
        }
    }
    None
}

fn select_mode(raw: &[u8], streaming_if: u8) -> Option<VideoMode> {
    let mut current_interface = None;
    let mut current_format = None;
    let mut modes = Vec::new();
    for descriptor in descriptor_iter(raw) {
        if descriptor[1] == USB_DT_INTERFACE && descriptor.len() >= 9 {
            current_interface = Some((descriptor[2], descriptor[3]));
            continue;
        }
        if current_interface != Some((streaming_if, 0))
            || descriptor[1] != USB_DT_CS_INTERFACE
            || descriptor.len() < 4
        {
            continue;
        }
        match descriptor[2] {
            UVC_VS_FORMAT_MJPEG if descriptor.len() >= 5 => {
                current_format = Some((PixelFormat::Mjpeg, descriptor[3]));
            }
            UVC_VS_FORMAT_UNCOMPRESSED if descriptor.len() >= 5 => {
                current_format = Some((PixelFormat::Uncompressed, descriptor[3]));
            }
            UVC_VS_FRAME_MJPEG | UVC_VS_FRAME_UNCOMPRESSED if descriptor.len() >= 26 => {
                let Some((format, format_index)) = current_format else {
                    continue;
                };
                let expected = match descriptor[2] {
                    UVC_VS_FRAME_MJPEG => PixelFormat::Mjpeg,
                    _ => PixelFormat::Uncompressed,
                };
                if format != expected {
                    continue;
                }
                modes.push(VideoMode {
                    format,
                    format_index,
                    frame_index: descriptor[3],
                    width: get_u16(descriptor, 5),
                    height: get_u16(descriptor, 7),
                    max_frame_size: get_u32(descriptor, 17),
                    interval_100ns: select_interval(descriptor),
                });
            }
            _ => {}
        }
    }

    // 首选ACT需要的640x360 MJPEG；否则选择像素数最接近且优先压缩的模式。
    modes.into_iter().min_by_key(|mode| {
        let exact_penalty = if mode.width == TARGET_WIDTH && mode.height == TARGET_HEIGHT {
            0u64
        } else {
            1u64 << 40
        };
        let format_penalty = if mode.format == PixelFormat::Mjpeg {
            0
        } else {
            1u64 << 39
        };
        let pixels = u64::from(mode.width) * u64::from(mode.height);
        let target = u64::from(TARGET_WIDTH) * u64::from(TARGET_HEIGHT);
        exact_penalty + format_penalty + pixels.abs_diff(target)
    })
}

fn select_interval(descriptor: &[u8]) -> u32 {
    let default = get_u32(descriptor, 21);
    let interval_type = descriptor[25];
    if interval_type == 0 || descriptor.len() < 30 {
        return default.max(333_333);
    }
    // 优先选择最接近30fps的离散间隔，以便后续直接接ACT采样流程。
    (0..interval_type as usize)
        .filter_map(|index| {
            let offset = 26 + index * 4;
            (offset + 4 <= descriptor.len()).then(|| get_u32(descriptor, offset))
        })
        .min_by_key(|interval| interval.abs_diff(333_333))
        .unwrap_or(default.max(333_333))
}

fn collect_streaming_alts(
    configuration: &crab_usb::usb_if::descriptor::ConfigurationDescriptor,
    streaming_if: u8,
) -> Vec<StreamingAlt> {
    let mut alts = Vec::new();
    for interface in &configuration.interfaces {
        if interface.interface_number != streaming_if {
            continue;
        }
        for alt in &interface.alt_settings {
            for endpoint in &alt.endpoints {
                if endpoint.direction == Direction::In
                    && matches!(
                        endpoint.transfer_type,
                        EndpointType::Isochronous | EndpointType::Bulk
                    )
                {
                    alts.push(StreamingAlt {
                        alternate: alt.alternate_setting,
                        address: endpoint.address,
                        transfer_type: endpoint.transfer_type,
                        payload_capacity: endpoint.max_packet_size as usize
                            * endpoint.packets_per_microframe.max(1),
                    });
                }
            }
        }
    }
    alts
}

fn select_alt(alts: &[StreamingAlt], requested: usize) -> Option<StreamingAlt> {
    alts.iter()
        .copied()
        .filter(|alt| alt.payload_capacity >= requested.max(1))
        .min_by_key(|alt| alt.payload_capacity)
        .or_else(|| alts.iter().copied().max_by_key(|alt| alt.payload_capacity))
}

async fn uvc_control_out(
    device: &mut Device,
    request: u8,
    selector: u16,
    interface: u8,
    data: &[u8],
) -> Result<(), UvcError> {
    let length = device
        .control_out(
            ControlSetup {
                request_type: RequestType::Class,
                recipient: Recipient::Interface,
                request: Request::Other(request),
                value: selector << 8,
                index: interface as u16,
            },
            data,
        )
        .await
        .map_err(|_| UvcError::Control)?;
    (length == data.len())
        .then_some(())
        .ok_or(UvcError::Control)
}

async fn uvc_control_in(
    device: &mut Device,
    request: u8,
    selector: u16,
    interface: u8,
    data: &mut [u8],
) -> Result<(), UvcError> {
    let length = device
        .control_in(
            ControlSetup {
                request_type: RequestType::Class,
                recipient: Recipient::Interface,
                request: Request::Other(request),
                value: selector << 8,
                index: interface as u16,
            },
            data,
        )
        .await
        .map_err(|_| UvcError::Control)?;
    (length == data.len())
        .then_some(())
        .ok_or(UvcError::Control)
}

fn descriptor_iter(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut offset = 0usize;
    core::iter::from_fn(move || {
        if offset + 2 > raw.len() {
            return None;
        }
        let length = raw[offset] as usize;
        if length < 2 || offset + length > raw.len() {
            offset = raw.len();
            return None;
        }
        let descriptor = &raw[offset..offset + length];
        offset += length;
        Some(descriptor)
    })
}

fn get_u16(data: &[u8], offset: usize) -> u16 {
    data.get(offset..offset + 2)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_le_bytes)
        .unwrap_or(0)
}

fn get_u32(data: &[u8], offset: usize) -> u32 {
    data.get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0)
}

fn put_u16(data: &mut [u8], offset: usize, value: u16) {
    if let Some(target) = data.get_mut(offset..offset + 2) {
        target.copy_from_slice(&value.to_le_bytes());
    }
}

fn put_u32(data: &mut [u8], offset: usize, value: u32) {
    if let Some(target) = data.get_mut(offset..offset + 4) {
        target.copy_from_slice(&value.to_le_bytes());
    }
}

fn log_mode(mode: &VideoMode, interface: u8, version: u16) {
    crate::runtime::puts(b"[libos] UVC mode format=");
    crate::runtime::puts(mode.format.name());
    crate::runtime::puts(b" width=");
    crate::runtime::hex(mode.width as u64);
    crate::runtime::puts(b" height=");
    crate::runtime::hex(mode.height as u64);
    crate::runtime::puts(b" interval_100ns=");
    crate::runtime::hex(mode.interval_100ns as u64);
    crate::runtime::puts(b" interface=");
    crate::runtime::hex(interface as u64);
    crate::runtime::puts(b" version=");
    crate::runtime::hex(version as u64);
    crate::runtime::puts(b"\r\n");
}

fn log_negotiated(alt: StreamingAlt, frame_size: usize, payload: usize) {
    crate::runtime::puts(b"[libos] UVC commit alt=");
    crate::runtime::hex(alt.alternate as u64);
    crate::runtime::puts(b" endpoint=");
    crate::runtime::hex(alt.address as u64);
    crate::runtime::puts(b" frame_max=");
    crate::runtime::hex(frame_size as u64);
    crate::runtime::puts(b" payload=");
    crate::runtime::hex(payload as u64);
    crate::runtime::puts(b" capacity=");
    crate::runtime::hex(alt.payload_capacity as u64);
    crate::runtime::puts(b"\r\n");
}
