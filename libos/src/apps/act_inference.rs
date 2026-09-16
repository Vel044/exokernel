//! ACT真实数据推理正确性实验。
//!
//! 模型和五组冻结输入均由EL0通过只读ext4读取。每组输入与PyTorch参考输出
//! 使用同一原始字节，应用比较完整600个动作值，而不是只检查少数硬编码点。

use act_runtime::{InferenceStage, ACTION_DIM, ACTION_STEPS, IMAGE_BYTES};

const CASE_COUNT: usize = 5;
// 正确性判定与宿主CSV和Linux runner统一，采用torch.allclose默认值。
const ABSOLUTE_TOLERANCE: f32 = 1e-8;
const RELATIVE_TOLERANCE: f32 = 1e-5;
const CASE_HAND_EYE_VA: u64 = exo_abi::ACT_INPUT_ARENA_BASE;
const CASE_FIXED_VA: u64 = CASE_HAND_EYE_VA + 2 * 1024 * 1024;
const CASE_STATE_VA: u64 = CASE_FIXED_VA + 2 * 1024 * 1024;
const CASE_REFERENCE_VA: u64 = CASE_STATE_VA + 1024 * 1024;

pub fn run(info: &exo_abi::UserBootInfo) -> ! {
    crate::runtime::puts(b"[libos] ACT real-data correctness experiment start\r\n");
    crate::runtime::puts(b"[libos] mounting user ext4 filesystem\r\n");
    let filesystem = crate::fs::FileSystem::mount(info).unwrap_or_else(|_| {
        crate::runtime::puts(b"[libos] ext4 mount failed\r\n");
        crate::runtime::exit(0x510)
    });
    let model_file = filesystem
        .read_mapped(
            "/model.safetensors",
            crate::drivers::act::MODEL_FILE_VA,
            crate::drivers::act::MODEL_FILE_MAX_SIZE,
        )
        .unwrap_or_else(|_| {
            crate::runtime::puts(b"[libos] model open/read failed\r\n");
            crate::runtime::exit(0x511)
        });
    let stats_file = filesystem
        .read_mapped(
            "/policy_preprocessor_step_3_normalizer_processor.safetensors",
            crate::drivers::act::STATS_FILE_VA,
            crate::drivers::act::STATS_FILE_MAX_SIZE,
        )
        .unwrap_or_else(|_| {
            crate::runtime::puts(b"[libos] normalizer open/read failed\r\n");
            crate::runtime::exit(0x512)
        });
    crate::runtime::puts(b"[libos] safetensors loaded through ext4\r\n");

    crate::runtime::puts(b"[libos] ACT parsing weights and allocating workspace\r\n");
    let mut policy =
        crate::drivers::act::Policy::from_files(model_file.as_slice(), stats_file.as_slice())
            .unwrap_or_else(|code| {
                crate::runtime::puts(b"[libos] ACT model setup failed=");
                crate::runtime::hex(code);
                crate::runtime::puts(b"\r\n");
                crate::runtime::exit(code)
            });
    crate::runtime::puts(b"[libos] ACT model and workspace ready\r\n");

    // 正确性实验必须执行与性能基准和机器人闭环相同的四核数值路径，否则
    // 只能证明旧单线程实现正确，不能验证当前行并行后端。CPU0上的当前线程
    // 计算第四个分片，CPU1..3上的固定亲和worker分别计算其余三个分片。
    let _parallel_pool = crate::runtime::act_parallel::ActParallelPool::create([
        crate::thread::ThreadConfig::new(1, 32, 32),
        crate::thread::ThreadConfig::new(2, 32, 32),
        crate::thread::ThreadConfig::new(3, 32, 32),
    ])
    .unwrap_or_else(|code| {
        crate::runtime::puts(b"[libos] ACT parallel pool setup failed=");
        crate::runtime::hex(code);
        crate::runtime::puts(b"\r\n");
        crate::runtime::exit(0x513)
    });
    // FPCR属于每CPU上下文；worker会在入口设置自己的FPCR，这里配置负责
    // 第四个分片的CPU0，确保四条执行路径使用一致的flush-to-zero语义。
    crate::runtime::configure_act_fpcr();
    crate::runtime::puts(b"[libos] ACT correctness backend CPU0..3 ready\r\n");

    for case_index in 0..CASE_COUNT {
        run_case(&filesystem, &mut policy, case_index);
    }
    crate::runtime::puts(b"[libos] ACT correctness PASS cases=5 values=3000\r\n");
    crate::runtime::exit(0)
}

fn run_case(
    filesystem: &crate::fs::FileSystem,
    policy: &mut crate::drivers::act::Policy<'_>,
    case_index: usize,
) {
    let mut path = [0u8; 64];
    let handeye_path = case_path(&mut path, case_index, b"handeye.rgb");
    let handeye = filesystem
        .read_mapped(handeye_path, CASE_HAND_EYE_VA, IMAGE_BYTES as u64)
        .unwrap_or_else(|_| fail_case(case_index, b"handeye read", 0x520));
    let fixed_path = case_path(&mut path, case_index, b"fixed.rgb");
    let fixed = filesystem
        .read_mapped(fixed_path, CASE_FIXED_VA, IMAGE_BYTES as u64)
        .unwrap_or_else(|_| fail_case(case_index, b"fixed read", 0x521));
    let state_path = case_path(&mut path, case_index, b"state.f32le");
    let state_file = filesystem
        .read_mapped(state_path, CASE_STATE_VA, 24)
        .unwrap_or_else(|_| fail_case(case_index, b"state read", 0x522));
    let reference_path = case_path(&mut path, case_index, b"pytorch-action.f32le");
    let reference = filesystem
        .read_mapped(
            reference_path,
            CASE_REFERENCE_VA,
            (ACTION_STEPS * ACTION_DIM * 4) as u64,
        )
        .unwrap_or_else(|_| fail_case(case_index, b"reference read", 0x523));

    if handeye.as_slice().len() != IMAGE_BYTES
        || fixed.as_slice().len() != IMAGE_BYTES
        || state_file.as_slice().len() != 24
        || reference.as_slice().len() != ACTION_STEPS * ACTION_DIM * 4
    {
        fail_case(case_index, b"input size", 0x524);
    }
    let mut state = [0.0f32; ACTION_DIM];
    for (index, slot) in state.iter_mut().enumerate() {
        let offset = index * 4;
        *slot = f32::from_le_bytes(
            state_file.as_slice()[offset..offset + 4]
                .try_into()
                .unwrap(),
        );
    }

    crate::runtime::puts(b"[libos] ACT case=");
    print_decimal(case_index as u64);
    crate::runtime::puts(b" forward begin\r\n");
    let mut actions = [[0.0f32; ACTION_DIM]; ACTION_STEPS];
    let started = crate::runtime::counter();
    let mut previous = started;
    policy
        .predict_with_progress(
            handeye.as_slice(),
            fixed.as_slice(),
            state,
            &mut actions,
            |stage| print_progress(stage, started, &mut previous),
        )
        .unwrap_or_else(|code| fail_case(case_index, b"predict", code));

    let result = compare(&actions, reference.as_slice());
    crate::runtime::puts(b"[libos] ACT case=");
    print_decimal(case_index as u64);
    crate::runtime::puts(b" ticks=");
    crate::runtime::hex(crate::runtime::counter().wrapping_sub(started));
    crate::runtime::puts(b" mean_abs_x1e9=");
    print_decimal(result.mean_abs_x1e9);
    crate::runtime::puts(b" max_abs_x1e9=");
    print_decimal(result.max_abs_x1e9);
    crate::runtime::puts(b" max_step=");
    print_decimal(result.max_index as u64 / ACTION_DIM as u64);
    crate::runtime::puts(b" max_joint=");
    print_decimal(result.max_index as u64 % ACTION_DIM as u64);
    crate::runtime::puts(b" failed=");
    print_decimal(result.failed as u64);
    crate::runtime::puts(b"\r\n");
    if result.failed != 0 {
        fail_case(case_index, b"numeric mismatch", 0x525);
    }
}

/// 在固定栈缓冲区生成case路径，避免正确性实验本身依赖字符串堆分配。
fn case_path<'a>(buffer: &'a mut [u8; 64], case_index: usize, name: &[u8]) -> &'a str {
    let prefix = b"/cases/case-00";
    buffer[..prefix.len()].copy_from_slice(prefix);
    buffer[prefix.len()] = b'0' + case_index as u8;
    buffer[prefix.len() + 1] = b'/';
    let end = prefix.len() + 2 + name.len();
    buffer[prefix.len() + 2..end].copy_from_slice(name);
    // SAFETY:prefix、数字、斜杠和固定ASCII文件名均为合法UTF-8。
    unsafe { core::str::from_utf8_unchecked(&buffer[..end]) }
}

struct Comparison {
    mean_abs_x1e9: u64,
    max_abs_x1e9: u64,
    max_index: usize,
    failed: usize,
}

fn compare(actions: &[[f32; ACTION_DIM]; ACTION_STEPS], reference: &[u8]) -> Comparison {
    let mut sum = 0.0f64;
    let mut maximum = 0.0f32;
    let mut max_index = 0usize;
    let mut failed = 0usize;
    for (index, actual) in actions.iter().flatten().enumerate() {
        let offset = index * 4;
        let expected = f32::from_le_bytes(reference[offset..offset + 4].try_into().unwrap());
        let error = (*actual - expected).abs();
        let tolerance = ABSOLUTE_TOLERANCE + RELATIVE_TOLERANCE * expected.abs();
        if !actual.is_finite() || error > tolerance {
            failed += 1;
        }
        if error > maximum {
            maximum = error;
            max_index = index;
        }
        sum += error as f64;
    }
    Comparison {
        mean_abs_x1e9: (sum * 1_000_000_000.0 / (ACTION_STEPS * ACTION_DIM) as f64) as u64,
        max_abs_x1e9: (maximum as f64 * 1_000_000_000.0) as u64,
        max_index,
        failed,
    }
}

fn fail_case(case_index: usize, stage: &[u8], code: u64) -> ! {
    crate::runtime::puts(b"[libos] ACT case=");
    print_decimal(case_index as u64);
    crate::runtime::puts(b" failure stage=");
    crate::runtime::puts(stage);
    crate::runtime::puts(b" code=");
    crate::runtime::hex(code);
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(code)
}

/// 输出阶段名、该阶段耗时和推理累计耗时。日志只出现在阶段边界，不进入
/// 卷积内层循环，因此不会按像素或矩阵元素触发串口I/O。
fn print_progress(stage: InferenceStage, started: u64, previous: &mut u64) {
    let now = crate::runtime::counter();
    crate::runtime::puts(b"[libos] ACT stage complete: ");
    match stage {
        InferenceStage::InputPrepared => crate::runtime::puts(b"input preparation"),
        InferenceStage::CameraNormalized { camera } => {
            print_camera(camera);
            crate::runtime::puts(b" normalization");
        }
        InferenceStage::CameraStem { camera } => {
            print_camera(camera);
            crate::runtime::puts(b" ResNet stem");
        }
        InferenceStage::CameraBlock { camera, block } => {
            print_camera(camera);
            crate::runtime::puts(b" ResNet block=");
            crate::runtime::hex(block as u64);
        }
        InferenceStage::CameraProjected { camera } => {
            print_camera(camera);
            crate::runtime::puts(b" feature projection");
        }
        InferenceStage::EncoderLayer { layer } => {
            crate::runtime::puts(b"Transformer encoder layer=");
            crate::runtime::hex(layer as u64);
        }
        InferenceStage::Decoder => crate::runtime::puts(b"Transformer decoder"),
        InferenceStage::ActionHead => crate::runtime::puts(b"action head"),
    }
    let frequency = crate::runtime::counter_frequency().max(1);
    let stage_ms = now.wrapping_sub(*previous).saturating_mul(1_000) / frequency;
    let total_ms = now.wrapping_sub(started).saturating_mul(1_000) / frequency;
    crate::runtime::puts(b" stage_ms=");
    print_decimal(stage_ms);
    crate::runtime::puts(b" total_ms=");
    print_decimal(total_ms);
    crate::runtime::puts(b"\r\n");
    *previous = now;
}

fn print_camera(camera: u8) {
    let name: &[u8] = if camera == 0 { b"handeye" } else { b"fixed" };
    crate::runtime::puts(name);
}

/// 无堆分配地打印十进制计数值，避免把毫秒误读成十六进制。
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
