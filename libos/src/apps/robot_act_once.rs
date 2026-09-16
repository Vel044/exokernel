//! QEMU单次ACT推理运动闭环。
//!
//! 本模块只编排应用策略：先固定三个USB root-port角色，采集两张图，读取
//! 六轴状态，调用ACT一次，再以30Hz执行100步动作。PCI、xHCI、UVC、JPEG、
//! CDC ACM和SCServo的寄存器/协议细节分别留在drivers中。

use core::sync::atomic::{AtomicU64, Ordering};

use act_runtime::{InferenceStage, ACTION_DIM, ACTION_STEPS};
use crab_usb::{EventHandler, ProbedDevice, USBHost};

use crate::{
    drivers::{act::Policy, scservo::FeetechMotorsBus},
    runtime::usb_executor::block_on_usb,
};

// QEMU qemu-xhci 先编号4个USB3 root port，再编号USB2 companion port。
// run.sh中的connector 1/2/3因此会被CrabUSB报告为root port 5/6/7。
// 这里匹配的是xHCI Slot Context里的Root Hub Port Number，不是QEMU命令行的
// `port=`连接器编号；两者混用会导致设备已枚举却始终选择不到。
const HANDEYE_PORT: u8 = 5;
const FIXED_PORT: u8 = 6;
const SCSERVO_PORT: u8 = 7;

// 模型占用Frame arena前半段；实时图像放在后半段，避免覆盖权重和工作区。
const HANDEYE_JPEG_VA: u64 = exo_abi::ACT_INPUT_ARENA_BASE;
const FIXED_JPEG_VA: u64 = HANDEYE_JPEG_VA + 2 * 1024 * 1024;
const HANDEYE_RGB_VA: u64 = FIXED_JPEG_VA + 2 * 1024 * 1024;
const FIXED_RGB_VA: u64 = HANDEYE_RGB_VA + 2 * 1024 * 1024;

const INFERENCE_IDLE: u64 = 0;
const INFERENCE_RUNNING: u64 = 1;
const INFERENCE_COMPLETE: u64 = 2;
const INFERENCE_FAILED: u64 = 3;

// CPU3写入状态，CPU2读取。Release/Acquire同时发布actions数组中的推理结果。
static INFERENCE_STATE: AtomicU64 = AtomicU64::new(INFERENCE_IDLE);
static INFERENCE_ERROR: AtomicU64 = AtomicU64::new(0);
static INFERENCE_STAGE: AtomicU64 = AtomicU64::new(0);

/// 只在一次闭环调用期间存在的跨核推理参数。
///
/// 所有指针都指向同一VSpace：Policy和actions位于CPU2用户栈，RGB位于普通
/// Frame映射。CPU2在INFERENCE_COMPLETE之前不读写这些对象，且等待CPU3退出
/// 后才允许本结构离开作用域。
#[repr(C)]
struct InferenceJob {
    policy: *mut (),
    handeye_rgb: *const u8,
    fixed_rgb: *const u8,
    image_len: usize,
    state: [f32; 6],
    actions: *mut [[f32; ACTION_DIM]; ACTION_STEPS],
}

/// CPU3上的纯计算入口；不拥有USB资源，也不执行舵机写入。
extern "C" fn inference_worker(job_ptr: u64, _thread: u64, _ipc: u64) -> ! {
    // CPU3是第四个ACT计算lane；其FPCR与CPU0..2 worker分别配置。
    crate::runtime::configure_act_fpcr();
    // SAFETY:CPU2在创建线程前完整初始化InferenceJob，并在看到Release发布的
    // COMPLETE/FAILED前保持job、Policy、RGB Frame和actions全部存活。CPU3是这些
    // 可变对象在推理期间的唯一访问者。
    let job = unsafe { &mut *(job_ptr as *mut InferenceJob) };
    let policy = unsafe { &mut *(job.policy as *mut Policy<'static>) };
    let handeye = unsafe { core::slice::from_raw_parts(job.handeye_rgb, job.image_len) };
    let fixed = unsafe { core::slice::from_raw_parts(job.fixed_rgb, job.image_len) };
    let actions = unsafe { &mut *job.actions };
    let result = policy.predict_with_progress(handeye, fixed, job.state, actions, |stage| {
        INFERENCE_STAGE.store(stage_number(stage), Ordering::Release)
    });
    match result {
        Ok(()) => INFERENCE_STATE.store(INFERENCE_COMPLETE, Ordering::Release),
        Err(code) => {
            INFERENCE_ERROR.store(code, Ordering::Release);
            INFERENCE_STATE.store(INFERENCE_FAILED, Ordering::Release);
        }
    }
    crate::thread::exit(0)
}

/// 真实USB资源入口由usb_task调用；普通run只保留给编译期场景分发。
pub(crate) fn run_with_resources(
    info: &exo_abi::UserBootInfo,
    host: &mut USBHost,
    handler: &EventHandler,
    intid: u32,
    notification: &crate::notification::Notification,
) -> ! {
    let filesystem =
        crate::fs::FileSystem::mount(info).unwrap_or_else(|_| fail("ACT ext4 mount failed", 0x700));
    let model = filesystem
        .read_mapped(
            "/model.safetensors",
            crate::drivers::act::MODEL_FILE_VA,
            crate::drivers::act::MODEL_FILE_MAX_SIZE,
        )
        .unwrap_or_else(|_| fail("ACT model read failed", 0x701));
    let stats = filesystem
        .read_mapped(
            "/policy_preprocessor_step_3_normalizer_processor.safetensors",
            crate::drivers::act::STATS_FILE_VA,
            crate::drivers::act::STATS_FILE_MAX_SIZE,
        )
        .unwrap_or_else(|_| fail("ACT normalizer read failed", 0x702));
    crate::runtime::puts(b"[libos] ACT model loaded through ext4\r\n");
    let mut policy = Policy::from_files(model.as_slice(), stats.as_slice())
        .unwrap_or_else(|code| fail_with_code("ACT model setup failed", code));

    let result = block_on_usb(robot_flow(&mut policy, host), handler, intid, notification);
    match result {
        Ok(()) => crate::runtime::exit(0),
        Err(code) => fail_with_code("robot ACT one-shot failed", code),
    }
}

/// 设备发现只执行一次；后续配置全部基于同一批DeviceInfo。
async fn robot_flow(policy: &mut Policy<'_>, host: &mut USBHost) -> Result<(), u64> {
    let devices = loop {
        let devices = host.probe_devices().await.map_err(|_| 0x710u64)?;
        if !devices.is_empty() {
            break devices;
        }
        crate::runtime::delay_ns(10_000_000);
    };
    for device in &devices {
        log_probed_device(device);
    }
    let handeye = find_device(&devices, HANDEYE_PORT, true).ok_or(0x711u64)?;
    let fixed = find_device(&devices, FIXED_PORT, true).ok_or(0x712u64)?;
    let serial = find_device(&devices, SCSERVO_PORT, false).ok_or(0x713u64)?;

    // 先建立低带宽CDC链路并确认真实舵机响应，再把两台UVC切到高带宽
    // isochronous alternate setting。这样首个SCServo Bulk OUT/IN事务不会与
    // 两路摄像头端点初始化同时竞争QEMU usb-host和xHCI Event Ring。
    crate::runtime::puts(b"[libos] opening SCServo xHCI root-port=0x7\r\n");
    let mut transport = crate::drivers::usb_serial::configure_scservo(host, serial)
        .await
        .map_err(|_| 0x716u64)?;
    let mut bus = FeetechMotorsBus::so101(&mut transport);
    bus.connect().await.map_err(|_| 0x720u64)?;
    bus.read_calibration().await.map_err(|_| 0x721u64)?;
    crate::runtime::puts(b"[libos] SCServo link verified before UVC streaming\r\n");

    crate::runtime::puts(b"[libos] opening handeye xHCI root-port=0x5\r\n");
    let mut handeye = crate::drivers::uvc::configure_camera(host, handeye)
        .await
        .map_err(|_| 0x714u64)?;
    crate::runtime::puts(b"[libos] opening fixed xHCI root-port=0x6\r\n");
    let mut fixed = match crate::drivers::uvc::configure_camera(host, fixed).await {
        Ok(stream) => stream,
        Err(_) => {
            let _ = handeye.stop().await;
            return Err(0x715);
        }
    };
    // 所有设备已经通过同一次open_device建立。UVC停止只释放视频endpoint，
    // CDC ACM继续复用原来的Device和Bulk endpoint，避免重复打开设备。
    let result = control_flow(policy, &mut bus, &mut handeye, &mut fixed).await;
    if result.is_err() {
        // control_flow在已获得串口后负责关闭扭矩；这里补做视频流清理。
        let _ = handeye.stop().await;
        let _ = fixed.stop().await;
    }
    result
}

/// 输出本轮枚举结果，区分“QEMU已透传设备”和“应用角色匹配成功”。
fn log_probed_device(device: &ProbedDevice) {
    crate::runtime::puts(b"[libos] probed USB root-port=");
    crate::runtime::hex(device.root_port_id().unwrap_or(0) as u64);
    crate::runtime::puts(b" VID=");
    crate::runtime::hex(device.vendor_id() as u64);
    crate::runtime::puts(b" PID=");
    crate::runtime::hex(device.product_id() as u64);
    crate::runtime::puts(b" role=");
    let role = match device.as_device_info() {
        Some(info) if crate::drivers::uvc::contains_uvc_streaming_interface(info) => {
            b"uvc" as &[u8]
        }
        Some(info)
            if crate::drivers::usb_serial::is_cdc_acm(info)
                || crate::drivers::usb_serial::is_ftdi(info) =>
        {
            b"serial"
        }
        _ => b"other",
    };
    crate::runtime::puts(role);
    crate::runtime::puts(b"\r\n");
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

async fn control_flow(
    policy: &mut Policy<'_>,
    bus: &mut FeetechMotorsBus<'_>,
    handeye: &mut crate::drivers::uvc::UvcStream,
    fixed: &mut crate::drivers::uvc::UvcStream,
) -> Result<(), u64> {
    let result = control_flow_inner(policy, bus, handeye, fixed).await;
    if result.is_err() {
        // 任何一个采集、推理或动作错误都不能让部分舵机保持扭矩。
        let _ = bus.disable_torque(None).await;
    }
    result
}

/// 已经拥有串口总线后的实际闭环；外层负责统一错误清理。
async fn control_flow_inner(
    policy: &mut Policy<'_>,
    bus: &mut FeetechMotorsBus<'_>,
    handeye: &mut crate::drivers::uvc::UvcStream,
    fixed: &mut crate::drivers::uvc::UvcStream,
) -> Result<(), u64> {
    // robot_flow已经在开启UVC等时流之前完成连接和校准读取；这里直接进入
    // 观测姿态保持、图像采集、ACT推理和动作执行，避免重复发送六轮PING。
    let before = bus.sync_read_positions_f32().await.map_err(|_| 0x722u64)?;
    print_positions(b"[libos] initial positions", before);
    bus.sync_write_positions_f32(before)
        .await
        .map_err(|_| 0x723u64)?;
    bus.enable_torque(None).await.map_err(|_| 0x724u64)?;
    let torque = bus.sync_read_torque_enabled().await.map_err(|_| 0x729u64)?;
    print_torque(b"[libos] torque readback", torque);
    if torque.iter().any(|value| *value != 1) {
        return Err(0x72a);
    }
    crate::runtime::puts(b"[libos] torque enabled on all six servos\r\n");

    crate::runtime::puts(b"[libos] capture handeye MJPEG\r\n");
    let handeye_jpeg = handeye
        .capture_frame_at(HANDEYE_JPEG_VA)
        .await
        .map_err(|_| 0x725u64)?;
    crate::runtime::puts(b"[libos] capture fixed MJPEG\r\n");
    let fixed_jpeg = fixed
        .capture_frame_at(FIXED_JPEG_VA)
        .await
        .map_err(|_| 0x726u64)?;
    crate::runtime::puts(b"[libos] decode handeye/fixed JPEG 640x360 RGB\r\n");
    let handeye_rgb = crate::drivers::jpeg::decode_mjpeg(handeye_jpeg.as_slice(), HANDEYE_RGB_VA)?;
    let fixed_rgb = crate::drivers::jpeg::decode_mjpeg(fixed_jpeg.as_slice(), FIXED_RGB_VA)?;
    let _ = handeye_jpeg.release();
    let _ = fixed_jpeg.release();
    handeye.stop().await.map_err(|_| 0x727u64)?;
    fixed.stop().await.map_err(|_| 0x728u64)?;

    let state = bus.sync_read_positions_f32().await.map_err(|_| 0x72fu64)?;
    print_positions(b"[libos] ACT input state", state);
    // 推理在QEMU TCG中可能持续数分钟。此时保持Torque_Enable=1和刚才写入的
    // 当前Goal Position，机械臂只维持观测姿态，不执行ACT输出。若关闭扭矩，
    // 重力会让关节漂移并使“同一观测对应同一状态”的安全前提失效。
    crate::runtime::puts(b"[libos] torque held at input pose during inference\r\n");
    let _parallel_pool = crate::runtime::act_parallel::ActParallelPool::create([
        crate::thread::ThreadConfig::new(0, 16, 16),
        crate::thread::ThreadConfig::new(1, 16, 16),
        // CPU2同时推进USB Event Ring，worker与USB线程同优先级轮转。
        crate::thread::ThreadConfig::new(2, 48, 48),
    ])
    .map_err(|_| 0x746u64)?;
    crate::runtime::puts(
        b"[libos] ACT forward begin with CPU0..3 row-parallel backend; USB idle during inference\r\n",
    );
    let started = crate::runtime::counter();
    let mut actions = [[0.0f32; ACTION_DIM]; ACTION_STEPS];
    let mut job = InferenceJob {
        policy: (policy as *mut Policy<'_>).cast::<()>(),
        handeye_rgb: handeye_rgb.as_slice().as_ptr(),
        fixed_rgb: fixed_rgb.as_slice().as_ptr(),
        image_len: crate::drivers::jpeg::RGB_BYTES,
        state,
        actions: &mut actions,
    };
    INFERENCE_ERROR.store(0, Ordering::Release);
    INFERENCE_STAGE.store(0, Ordering::Release);
    INFERENCE_STATE.store(INFERENCE_RUNNING, Ordering::Release);
    let inference_thread = crate::thread::Thread::spawn(
        inference_worker,
        (&mut job as *mut InferenceJob) as u64,
        crate::thread::ThreadConfig::new(3, 16, 16),
    )
    .map_err(|_| 0x735u64)?;

    let mut reported_stage = 0u64;
    loop {
        match INFERENCE_STATE.load(Ordering::Acquire) {
            INFERENCE_COMPLETE => break,
            INFERENCE_FAILED => return Err(INFERENCE_ERROR.load(Ordering::Acquire)),
            INFERENCE_RUNNING => {}
            _ => return Err(0x736),
        }
        // 推理期间没有待完成的USB传输，因此无需主动PING。此前每500ms发送一次
        // 保活会在TCG的长推理中累计约170次物理usb-host事务；任一次macOS
        // libusb瞬断都会让QEMU重新枚举全部设备，甚至触发host-libusb断言。
        // 这里只轮询跨核推理状态，推理结束后再恢复SCServo Bulk传输。
        let stage = INFERENCE_STAGE.load(Ordering::Acquire);
        if stage != 0 && stage != reported_stage {
            reported_stage = stage;
            crate::runtime::puts(b"[libos] ACT progress stage=");
            crate::runtime::hex(stage);
            crate::runtime::puts(b"\r\n");
        }
        crate::runtime::delay_ns(100_000_000);
    }
    // COMPLETE的Acquire保证CPU3对actions的全部写入已经对CPU2可见。
    core::mem::forget(inference_thread);
    crate::runtime::puts(b"[libos] ACT forward complete ticks=");
    crate::runtime::hex(crate::runtime::counter().wrapping_sub(started));
    crate::runtime::puts(b"\r\n");

    let after_inference = bus.sync_read_positions_f32().await.map_err(|_| 0x730u64)?;
    print_positions(b"[libos] positions after inference", after_inference);
    let mut drift = [0.0f32; 6];
    for joint in 0..6 {
        drift[joint] = (after_inference[joint] - state[joint]).abs();
    }
    print_positions(b"[libos] inference pose drift", drift);
    if drift.iter().any(|value| *value > 2.0) {
        return Err(0x741);
    }
    // 动作执行前再次读回Torque_Enable，证明推理期间保持力没有意外丢失。
    let torque = bus.sync_read_torque_enabled().await.map_err(|_| 0x73fu64)?;
    print_torque(b"[libos] torque held before action", torque);
    if torque.iter().any(|value| *value != 1) {
        return Err(0x740);
    }
    execute_actions(bus, actions).await?;
    let final_position = bus.sync_read_positions_f32().await.map_err(|_| 0x732u64)?;
    print_positions(b"[libos] final positions", final_position);
    bus.disable_torque(None).await.map_err(|_| 0x733u64)?;
    let torque = bus.sync_read_torque_enabled().await.map_err(|_| 0x738u64)?;
    print_torque(b"[libos] torque disabled readback", torque);
    if torque.iter().any(|value| *value != 0) {
        return Err(0x739);
    }
    crate::runtime::puts(b"[libos] torque disabled; robot ACT one-shot complete\r\n");
    let _ = handeye_rgb.release();
    let _ = fixed_rgb.release();
    Ok(())
}

pub(crate) async fn execute_actions(
    bus: &mut FeetechMotorsBus<'_>,
    actions: [[f32; ACTION_DIM]; ACTION_STEPS],
) -> Result<(), u64> {
    let frequency = crate::runtime::counter_frequency().max(1);
    let period = (frequency / 30).max(1);
    let mut deadline = crate::runtime::counter();
    let initial = bus.sync_read_positions_f32().await.map_err(|_| 0x730u64)?;
    // `commanded`保存上一轮已经写入Goal Position的连续值。若每次都从尚未来得及
    // 变化的Present Position重新加0.1，量化后会反复写同一个寄存器计数，舵机不动。
    let mut commanded = initial;
    for (step, action) in actions.into_iter().enumerate() {
        let current = bus.sync_read_positions_f32().await.map_err(|_| 0x730u64)?;
        let mut target = commanded;
        for joint in 0..6 {
            if !action[joint].is_finite() {
                return Err(0x731);
            }
            // 第一层限制保持LeRobot的max_relative_target=0.1语义，但基于上一条命令
            // 累计前进；第二层限制命令最多领先机械实测位置2.0，避免电机堵转时目标
            // 仍无限累积。两个限制共同保证“能形成位移”与“不会突然跳变”。
            let next = commanded[joint] + (action[joint] - commanded[joint]).clamp(-0.1, 0.1);
            target[joint] = current[joint] + (next - current[joint]).clamp(-2.0, 2.0);
        }
        commanded = target;
        if step == 0 {
            print_positions(b"[libos] ACT first raw target", action);
            print_positions(b"[libos] ACT first limited target", target);
        }
        bus.sync_write_positions_f32(target)
            .await
            .map_err(|_| 0x732u64)?;
        if step == 0 || step + 1 == ACTION_STEPS {
            // 读回Goal Position只验证控制表写入，不把它误当成机械已到位；
            // Present Position仍在循环开头读取，用来做下一步限幅。
            let goal = bus
                .sync_read_goal_positions_f32()
                .await
                .map_err(|_| 0x734u64)?;
            print_positions(b"[libos] ACT goal readback", goal);
        }
        if step % 10 == 0 || step + 1 == ACTION_STEPS {
            crate::runtime::puts(b"[libos] action step=");
            crate::runtime::hex((step + 1) as u64);
            crate::runtime::puts(b"/100\r\n");
            print_positions(b"[libos] commanded positions", commanded);
            print_positions(b"[libos] measured positions", current);
        }
        deadline = deadline.wrapping_add(period);
        while crate::runtime::counter().wrapping_sub(deadline) > u64::MAX / 2 {
            core::hint::spin_loop();
        }
    }
    // 最后一条Goal写入后给舵机一秒跟随时间，再以Present Position验收真实机械
    // 位移。这里只认传感器读回，不把Bulk OUT完成或Goal寄存器变化当成“已运动”。
    crate::runtime::delay_ns(1_000_000_000);
    let measured = bus.sync_read_positions_f32().await.map_err(|_| 0x747u64)?;
    let mut displacement = [0.0f32; 6];
    for joint in 0..6 {
        displacement[joint] = (measured[joint] - initial[joint]).abs();
    }
    print_positions(b"[libos] measured action displacement", displacement);
    if displacement.iter().all(|value| *value < 0.5) {
        return Err(0x748);
    }
    Ok(())
}

/// 把带字段的推理阶段压成稳定整数，供CPU3发布、CPU2日志展示。
fn stage_number(stage: InferenceStage) -> u64 {
    match stage {
        InferenceStage::InputPrepared => 1,
        InferenceStage::CameraNormalized { camera } => 10 + camera as u64,
        InferenceStage::CameraStem { camera } => 20 + camera as u64,
        InferenceStage::CameraBlock { camera, block } => 100 + camera as u64 * 16 + block as u64,
        InferenceStage::CameraProjected { camera } => 30 + camera as u64,
        InferenceStage::EncoderLayer { layer } => 200 + layer as u64,
        InferenceStage::Decoder => 300,
        InferenceStage::ActionHead => 400,
    }
}

fn print_positions(prefix: &[u8], positions: [f32; 6]) {
    crate::runtime::puts(prefix);
    for position in positions {
        crate::runtime::puts(b" ");
        crate::runtime::hex(position.to_bits() as u64);
    }
    crate::runtime::puts(b"\r\n");
}

fn print_torque(prefix: &[u8], values: [u8; 6]) {
    crate::runtime::puts(prefix);
    for value in values {
        crate::runtime::puts(b" ");
        crate::runtime::hex(value as u64);
    }
    crate::runtime::puts(b"\r\n");
}

fn fail(message: &'static str, code: u64) -> ! {
    fail_with_code(message, code)
}

fn fail_with_code(message: &'static str, code: u64) -> ! {
    crate::runtime::puts(b"[libos] ");
    crate::runtime::puts(message.as_bytes());
    crate::runtime::puts(b" code=");
    crate::runtime::hex(code);
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(code)
}
