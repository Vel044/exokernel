//! 两相机与六轴位置固定观测采集应用。
//!
//! 本场景只读取硬件：它不使能扭矩、不写Goal Position，也不运行ACT。两张
//! UVC MJPEG和同一时刻读取的六轴归一化位置写入用户态可写ext4，供宿主检查
//! 并作为后续纯推理基准的固定输入。

use crab_usb::{EventHandler, ProbedDevice, USBHost};

use crate::drivers::scservo::FeetechMotorsBus;

const HANDEYE_PORT: u8 = 5;
const FIXED_PORT: u8 = 6;
const SCSERVO_PORT: u8 = 7;
const HANDEYE_JPEG_VA: u64 = exo_abi::ACT_INPUT_ARENA_BASE;
const FIXED_JPEG_VA: u64 = HANDEYE_JPEG_VA + 2 * 1024 * 1024;

pub(crate) fn run_with_resources(
    info: &exo_abi::UserBootInfo,
    host: &mut USBHost,
    handler: &EventHandler,
    intid: u32,
    notification: &crate::notification::Notification,
) -> ! {
    let result = crate::runtime::usb_executor::block_on_usb(
        capture(info, host),
        handler,
        intid,
        notification,
    );
    match result {
        Ok(()) => crate::runtime::exit(0),
        Err(code) => {
            crate::runtime::puts(b"[libos] robot observation capture failed=");
            crate::runtime::hex(code);
            crate::runtime::puts(b"\r\n");
            crate::runtime::exit(code)
        }
    }
}

async fn capture(info: &exo_abi::UserBootInfo, host: &mut USBHost) -> Result<(), u64> {
    let devices = loop {
        let devices = host.probe_devices().await.map_err(|_| 0x760u64)?;
        if !devices.is_empty() {
            break devices;
        }
        crate::runtime::delay_ns(10_000_000);
    };
    let handeye = find_device(&devices, HANDEYE_PORT, true).ok_or(0x761u64)?;
    let fixed = find_device(&devices, FIXED_PORT, true).ok_or(0x762u64)?;
    let serial = find_device(&devices, SCSERVO_PORT, false).ok_or(0x763u64)?;

    crate::runtime::puts(b"[libos] configure handeye root-port=5\r\n");
    let mut handeye = crate::drivers::uvc::configure_camera(host, handeye)
        .await
        .map_err(|_| 0x764u64)?;
    crate::runtime::puts(b"[libos] configure fixed root-port=6\r\n");
    let mut fixed = crate::drivers::uvc::configure_camera(host, fixed)
        .await
        .map_err(|_| 0x765u64)?;
    let mut transport = crate::drivers::usb_serial::configure_scservo(host, serial)
        .await
        .map_err(|_| 0x766u64)?;
    let mut bus = FeetechMotorsBus::so101(&mut transport);
    bus.connect().await.map_err(|_| 0x767u64)?;
    bus.read_calibration().await.map_err(|_| 0x768u64)?;

    crate::runtime::puts(b"[libos] capture handeye MJPEG\r\n");
    let handeye_frame = handeye
        .capture_frame_at(HANDEYE_JPEG_VA)
        .await
        .map_err(|_| 0x769u64)?;
    crate::runtime::puts(b"[libos] capture fixed MJPEG\r\n");
    let fixed_frame = fixed
        .capture_frame_at(FIXED_JPEG_VA)
        .await
        .map_err(|_| 0x76au64)?;
    let state = bus.sync_read_positions_f32().await.map_err(|_| 0x76bu64)?;
    print_state(state);

    handeye.stop().await.map_err(|_| 0x76cu64)?;
    fixed.stop().await.map_err(|_| 0x76du64)?;
    let mut volume = crate::fs::ObservationVolume::discover(info)?;
    volume.write_observation(handeye_frame.as_slice(), fixed_frame.as_slice(), state)?;
    crate::runtime::puts(b"[libos] observation ext4 flushed; torque was never enabled\r\n");
    Ok(())
}

fn find_device<'a>(
    devices: &'a [ProbedDevice],
    root_port: u8,
    camera: bool,
) -> Option<&'a crab_usb::DeviceInfo> {
    devices.iter().find_map(|device| {
        if device.root_port_id() != Some(root_port) {
            return None;
        }
        let info = device.as_device_info()?;
        let matches = if camera {
            crate::drivers::uvc::contains_uvc_streaming_interface(info)
        } else {
            crate::drivers::usb_serial::is_cdc_acm(info)
                || crate::drivers::usb_serial::is_ftdi(info)
        };
        matches.then_some(info)
    })
}

fn print_state(state: [f32; 6]) {
    crate::runtime::puts(b"[libos] captured state f32_bits");
    for value in state {
        crate::runtime::puts(b" ");
        crate::runtime::hex(value.to_bits() as u64);
    }
    crate::runtime::puts(b"\r\n");
}
