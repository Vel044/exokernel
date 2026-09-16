//! 当前真实观测上的ACT纯推理性能基准。
//!
//! 输入由`robot-observation`场景采集后冻结到只读ext4。这里不初始化PCI、
//! xHCI、UVC或SCServo，计时区间只覆盖ACT前向传播，从而能稳定比较算子优化。

use act_runtime::{InferenceStage, ACTION_DIM, ACTION_STEPS, IMAGE_BYTES};

const HANDEYE_VA: u64 = exo_abi::ACT_INPUT_ARENA_BASE;
const FIXED_VA: u64 = HANDEYE_VA + 2 * 1024 * 1024;
const STATE_VA: u64 = FIXED_VA + 2 * 1024 * 1024;
const WARMUP_RUNS: usize = 5;
const MEASURE_RUNS: usize = 10;
// 仅在专项长稳构建中启用：`ACT_CONTINUOUS_CHECK=1 ./build.sh`。默认基准
// 不增加这100次运行，避免把稳定性检查混入 steady-state 性能样本。
const CONTINUOUS_CHECK: bool = option_env!("ACT_CONTINUOUS_CHECK").is_some();
const CONTINUOUS_RUNS: usize = 100;

pub(crate) fn run(info: &exo_abi::UserBootInfo) -> ! {
    // 启动固定观测的 ACT 性能基准。
    crate::runtime::puts(b"[libos] ACT fixed-observation benchmark start\r\n");

    // 挂载用户态只读 ext4 文件系统。
    let filesystem = crate::fs::FileSystem::mount(info).unwrap_or_else(|_| fail(0x530));

    // 从ext4读取模型文件，并映射到模型专用的EL0 VA。
    let model = filesystem
        .read_mapped(
            "/model.safetensors",
            crate::drivers::act::MODEL_FILE_VA,
            crate::drivers::act::MODEL_FILE_MAX_SIZE,
        )
        .unwrap_or_else(|_| fail(0x531));

    // 读取输入和动作的归一化参数。
    let stats = filesystem
        .read_mapped(
            "/policy_preprocessor_step_3_normalizer_processor.safetensors",
            crate::drivers::act::STATS_FILE_VA,
            crate::drivers::act::STATS_FILE_MAX_SIZE,
        )
        .unwrap_or_else(|_| fail(0x532));

    // 读取冻结的双相机图像和关节状态。
    let handeye = filesystem
        .read_mapped("/observation/handeye.rgb", HANDEYE_VA, IMAGE_BYTES as u64)
        .unwrap_or_else(|_| fail(0x533));
    let fixed = filesystem
        .read_mapped("/observation/fixed.rgb", FIXED_VA, IMAGE_BYTES as u64)
        .unwrap_or_else(|_| fail(0x534));
    let state_file = filesystem
        .read_mapped("/observation/state.f32le", STATE_VA, 24)
        .unwrap_or_else(|_| fail(0x535));

    // 检查输入文件大小是否符合 ACT 固定格式。
    if handeye.as_slice().len() != IMAGE_BYTES
        || fixed.as_slice().len() != IMAGE_BYTES
        || state_file.as_slice().len() != 24
    {
        fail(0x536);
    }

    // 把 little-endian 文件中的六轴状态恢复成 f32 数组。
    let mut state = [0.0f32; ACTION_DIM];
    for (index, value) in state.iter_mut().enumerate() {
        let offset = index * 4;
        *value = f32::from_le_bytes(
            state_file.as_slice()[offset..offset + 4]
                .try_into()
                .unwrap(),
        );
    }
    crate::runtime::puts(b"[libos] fixed RGB/state loaded; model setup begin\r\n");

    // 解析模型权重和归一化参数，创建 ACT 策略对象。
    let mut policy = crate::drivers::act::Policy::from_files(model.as_slice(), stats.as_slice())
        .unwrap_or_else(|_| fail(0x537));

    // 创建 CPU1..3 的并行推理 worker，主线程运行 CPU0 的计算分片。
    let _parallel_pool = crate::runtime::act_parallel::ActParallelPool::create([
        crate::thread::ThreadConfig::new(1, 32, 32),
        crate::thread::ThreadConfig::new(2, 32, 32),
        crate::thread::ThreadConfig::new(3, 32, 32),
    ])
    .unwrap_or_else(|_| fail(0x538));

    #[cfg(feature = "acl-neon")]
    {
        act_runtime::acl_operator_golden_smoke().unwrap_or_else(|_| fail(0x53c));
        crate::runtime::puts(b"[libos] ACL operator golden smoke passed\r\n");
    }

    // 配置当前 CPU 的浮点计算模式。
    crate::runtime::configure_act_fpcr();

    // 先完成固定次数 warm-up，再只统计 steady-state 前向推理；模型解析、
    // Frame 映射、worker 创建和第一次 cache 冷启动均不计入正式样本。
    crate::runtime::puts(b"[libos] ACT steady-state warmup=5 runs=10\r\n");
    let mut actions = [[0.0f32; ACTION_DIM]; ACTION_STEPS];
    for _ in 0..WARMUP_RUNS {
        policy
            .predict(handeye.as_slice(), fixed.as_slice(), state, &mut actions)
            .unwrap_or_else(|_| fail(0x539));
    }
    let allocations_before = crate::runtime::allocation_count();
    let mut samples_ms = [0u64; MEASURE_RUNS];
    let mut model_samples_ms = [0u64; MEASURE_RUNS];
    for (sample_index, sample) in samples_ms.iter_mut().enumerate() {
        let started = crate::runtime::counter();
        let mut model_timer = ModelTimer::new(started);
        policy
            .predict_with_progress(
                handeye.as_slice(),
                fixed.as_slice(),
                state,
                &mut actions,
                |stage| model_timer.observe(stage),
            )
            .unwrap_or_else(|_| fail(0x539));
        let elapsed = crate::runtime::counter().wrapping_sub(started);
        *sample = elapsed.saturating_mul(1_000) / crate::runtime::counter_frequency().max(1);
        model_samples_ms[sample_index] = model_timer.elapsed_ms();
    }
    if CONTINUOUS_CHECK {
        // 长稳检查复用同一输入、输出和 workspace；每次 predict 都重新走完整
        // 图，若 worker 屏障、临时 arena 或 ACL panel 生命周期有问题，这里会
        // 以非零错误码退出，而不是把一次偶然成功当作稳定性证据。
        let continuous_allocations_before = crate::runtime::allocation_count();
        for _ in 0..CONTINUOUS_RUNS {
            policy
                .predict(handeye.as_slice(), fixed.as_slice(), state, &mut actions)
                .unwrap_or_else(|_| fail(0x53a));
        }
        let continuous_allocations_after = crate::runtime::allocation_count();
        if continuous_allocations_after != continuous_allocations_before {
            crate::runtime::puts(b"[libos] ACT continuous check allocation failure\r\n");
            fail(0x53b);
        }
        crate::runtime::puts(b"[libos] ACT continuous check passed runs=100\r\n");
    }
    let allocations_after = crate::runtime::allocation_count();
    crate::runtime::puts(b"[libos] ACT steady allocations=");
    print_decimal((allocations_after - allocations_before) as u64);
    crate::runtime::puts(b"\r\n");
    let mut ordered = samples_ms;
    ordered.sort_unstable();
    let mut ordered_model = model_samples_ms;
    ordered_model.sort_unstable();
    crate::runtime::puts(b"[libos] ACT benchmark complete median_ms=");
    print_decimal(ordered[MEASURE_RUNS / 2]);
    crate::runtime::puts(b" p95_ms=");
    print_decimal(ordered[(MEASURE_RUNS * 95).div_ceil(100) - 1]);
    crate::runtime::puts(b" samples_ms=");
    for sample in samples_ms {
        print_decimal(sample);
        crate::runtime::puts(b",");
    }
    crate::runtime::puts(b" output_bits_first_step=");
    for value in actions[0] {
        crate::runtime::hex(value.to_bits() as u64);
        crate::runtime::puts(b" ");
    }
    crate::runtime::puts(b"\r\n");
    crate::runtime::puts(b"[libos] ACT model-only complete median_ms=");
    print_decimal(ordered_model[MEASURE_RUNS / 2]);
    crate::runtime::puts(b" p95_ms=");
    print_decimal(ordered_model[(MEASURE_RUNS * 95).div_ceil(100) - 1]);
    crate::runtime::puts(b" samples_ms=");
    for sample in model_samples_ms {
        print_decimal(sample);
        crate::runtime::puts(b",");
    }
    crate::runtime::puts(b"\r\n");

    // 输出全部 600 个动作的 IEEE-754 原始位，供宿主保存到 ext4。
    crate::runtime::puts(b"[libos] ACT_ACTION_BITS_BEGIN count=600\r\n");
    for (index, value) in actions.iter().flatten().enumerate() {
        crate::runtime::puts(b"[libos] ACT_ACTION_BITS index=");
        print_decimal(index as u64);
        crate::runtime::puts(b" bits=");
        crate::runtime::hex(value.to_bits() as u64);
        crate::runtime::puts(b"\r\n");
    }
    crate::runtime::puts(b"[libos] ACT_ACTION_BITS_END\r\n");
    crate::runtime::exit(0)
}

fn print_progress(stage: InferenceStage, started: u64, previous: &mut u64) {
    let now = crate::runtime::counter();
    crate::runtime::puts(b"[libos] ACT benchmark stage=");
    match stage {
        InferenceStage::InputPrepared => crate::runtime::puts(b"input"),
        InferenceStage::CameraNormalized { camera } => print_camera(camera, b" normalize"),
        InferenceStage::CameraStem { camera } => print_camera(camera, b" stem"),
        InferenceStage::CameraBlock { camera, block } => {
            print_camera(camera, b" block=");
            crate::runtime::hex(block as u64);
        }
        InferenceStage::CameraProjected { camera } => print_camera(camera, b" projection"),
        InferenceStage::EncoderLayer { layer } => {
            crate::runtime::puts(b"encoder=");
            crate::runtime::hex(layer as u64);
        }
        InferenceStage::Decoder => crate::runtime::puts(b"decoder"),
        InferenceStage::ActionHead => crate::runtime::puts(b"action-head"),
    }
    let frequency = crate::runtime::counter_frequency().max(1);
    crate::runtime::puts(b" stage_ms=");
    print_decimal(now.wrapping_sub(*previous).saturating_mul(1_000) / frequency);
    crate::runtime::puts(b" total_ms=");
    print_decimal(now.wrapping_sub(started).saturating_mul(1_000) / frequency);
    crate::runtime::puts(b"\r\n");
    *previous = now;
}

/// 从阶段回调中提取与 PyTorch `model_ms` 相同边界的 Rust 模型时间。
///
/// `CameraNormalized` 回调发生在一张图像的 HWC→FP32 归一化完成后；从该事件
/// 到下一次归一化事件（或 ActionHead）只包含网络算子。InputPrepared 和两次
/// CameraNormalized 之前的区间属于输入预处理，故不计入 model-only 样本。
struct ModelTimer {
    previous: u64,
    model_ticks: u64,
    in_model: bool,
}

impl ModelTimer {
    fn new(started: u64) -> Self {
        Self {
            previous: started,
            model_ticks: 0,
            in_model: false,
        }
    }

    fn observe(&mut self, stage: InferenceStage) {
        let now = crate::runtime::counter();
        let is_normalized = matches!(stage, InferenceStage::CameraNormalized { .. });
        if self.in_model && !is_normalized {
            self.model_ticks = self
                .model_ticks
                .wrapping_add(now.wrapping_sub(self.previous));
        }
        if is_normalized {
            self.in_model = true;
        }
        self.previous = now;
    }

    fn elapsed_ms(&self) -> u64 {
        self.model_ticks
            .saturating_mul(1_000)
            .checked_div(crate::runtime::counter_frequency().max(1))
            .unwrap_or(0)
    }
}

fn print_camera(camera: u8, suffix: &[u8]) {
    crate::runtime::puts(if camera == 0 { b"handeye" } else { b"fixed" });
    crate::runtime::puts(suffix);
}

fn print_decimal(mut value: u64) {
    let mut buffer = [0u8; 20];
    let mut cursor = buffer.len();
    loop {
        cursor -= 1;
        buffer[cursor] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    crate::runtime::puts(&buffer[cursor..]);
}

fn fail(code: u64) -> ! {
    crate::runtime::puts(b"[libos] ACT fixed benchmark failed code=");
    crate::runtime::hex(code);
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(code)
}
