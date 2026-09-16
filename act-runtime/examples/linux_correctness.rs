//! 实验02的AArch64 Linux正确性运行器。
//!
//! 该程序只负责在QEMU Linux中执行与EL0、seL4相同的`act-runtime`前向。
//! 模型、冻结输入和PyTorch参考值来自挂载在`/data`的ext4实验盘；每组完整
//! 600个输出写回该磁盘，宿主随后导出并生成逐元素CSV。

use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::Path,
    time::Instant,
};

use act_runtime::{
    ActModel, Observation, ACTION_DIM, ACTION_STEPS, IMAGE_BYTES, WORKSPACE_FLOATS,
};

const DATA_ROOT: &str = "/data";
const CASE_COUNT: usize = 5;
// 与PyTorch torch.allclose默认值一致，用于跨运行时逐元素比较。
const ABSOLUTE_TOLERANCE: f32 = 1e-8;
const RELATIVE_TOLERANCE: f32 = 1e-5;

fn main() {
    let root = Path::new(DATA_ROOT);
    println!("[linux-rust] loading ACT model");
    let model_bytes = fs::read(root.join("model.safetensors")).expect("read model.safetensors");
    let stats_bytes = fs::read(
        root.join("policy_preprocessor_step_3_normalizer_processor.safetensors"),
    )
    .expect("read normalizer");
    let model = ActModel::load(&model_bytes, &stats_bytes).expect("parse ACT model");

    // 工作区在五次推理之间复用；正确性计时不作为实验03的正式性能结果。
    let mut workspace = vec![0.0f32; WORKSPACE_FLOATS];
    let summary = File::create(root.join("linux-rust-summary.csv")).expect("create summary");
    let mut summary = BufWriter::new(summary);
    writeln!(
        summary,
        "system,case_id,duration_ms,mean_abs_error,max_abs_error,failed_elements,valid"
    )
    .unwrap();

    for case_index in 0..CASE_COUNT {
        run_case(&model, &mut workspace, root, case_index, &mut summary);
    }
    summary.flush().unwrap();
    println!("[linux-rust] PASS: 5 cases / 3000 values");
}

fn run_case(
    model: &ActModel<'_>,
    workspace: &mut [f32],
    root: &Path,
    case_index: usize,
    summary: &mut BufWriter<File>,
) {
    let directory = root.join("cases").join(format!("case-{case_index:03}"));
    let handeye = fs::read(directory.join("handeye.rgb")).expect("read handeye image");
    let fixed = fs::read(directory.join("fixed.rgb")).expect("read fixed image");
    assert_eq!(handeye.len(), IMAGE_BYTES);
    assert_eq!(fixed.len(), IMAGE_BYTES);

    let state_bytes = fs::read(directory.join("state.f32le")).expect("read state");
    assert_eq!(state_bytes.len(), ACTION_DIM * 4);
    let mut state = [0.0f32; ACTION_DIM];
    for (slot, bytes) in state.iter_mut().zip(state_bytes.chunks_exact(4)) {
        *slot = f32::from_le_bytes(bytes.try_into().unwrap());
    }

    let reference = fs::read(directory.join("pytorch-action.f32le")).expect("read reference");
    assert_eq!(reference.len(), ACTION_STEPS * ACTION_DIM * 4);
    let observation = Observation {
        handeye_rgb: &handeye,
        fixed_rgb: &fixed,
        state,
    };
    let mut actions = [[0.0f32; ACTION_DIM]; ACTION_STEPS];

    let started = Instant::now();
    model
        .predict(&observation, workspace, &mut actions)
        .expect("ACT forward");
    let duration_ms = started.elapsed().as_secs_f64() * 1000.0;

    // little-endian二进制保留每一个输出，避免串口格式化损失float32精度。
    let mut output = Vec::with_capacity(ACTION_STEPS * ACTION_DIM * 4);
    let mut error_sum = 0.0f64;
    let mut maximum = 0.0f32;
    let mut failed = 0usize;
    for (index, actual) in actions.iter().flatten().enumerate() {
        output.extend_from_slice(&actual.to_le_bytes());
        let offset = index * 4;
        let expected = f32::from_le_bytes(reference[offset..offset + 4].try_into().unwrap());
        let error = (*actual - expected).abs();
        let tolerance = ABSOLUTE_TOLERANCE + RELATIVE_TOLERANCE * expected.abs();
        error_sum += f64::from(error);
        maximum = maximum.max(error);
        if !actual.is_finite() || error > tolerance {
            failed += 1;
        }
    }
    fs::write(directory.join("linux-rust-action.f32le"), output).expect("write ACT output");

    let mean = error_sum / (ACTION_STEPS * ACTION_DIM) as f64;
    writeln!(
        summary,
        "linux-qemu-tcg,{case_index:03},{duration_ms:.3},{mean:.9},{maximum:.9},{failed},{}",
        failed == 0
    )
    .unwrap();
    summary.flush().unwrap();
    println!(
        "[linux-rust] case-{case_index:03} duration_ms={duration_ms:.3} \
         mean_abs_error={mean:.9} max_abs_error={maximum:.9} failed={failed}"
    );
    assert_eq!(failed, 0, "case-{case_index:03} failed correctness check");
}
