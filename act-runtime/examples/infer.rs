use std::{fs, path::Path, time::Instant};

use act_runtime::{
    ActModel, Observation, ACTION_DIM, ACTION_STEPS, IMAGE_BYTES, IMAGE_HEIGHT, IMAGE_WIDTH,
    WORKSPACE_FLOATS,
};

// 正确性实验采用PyTorch torch.allclose的默认绝对/相对容差。
const ABSOLUTE_TOLERANCE: f32 = 1e-8;
const RELATIVE_TOLERANCE: f32 = 1e-5;

fn main() {
    let directory = std::env::args().nth(1).unwrap_or_else(|| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/act/model").into()
    });
    let cases = std::env::args().nth(2);
    let model_bytes = std::fs::read(format!("{directory}/model.safetensors")).unwrap();
    let stats_bytes = std::fs::read(format!(
        "{directory}/policy_preprocessor_step_3_normalizer_processor.safetensors"
    ))
    .unwrap();
    let model = ActModel::load(&model_bytes, &stats_bytes).unwrap();

    if let Some(cases) = cases {
        run_cases(&model, Path::new(&cases));
        return;
    }

    let mut handeye = vec![0u8; IMAGE_BYTES];
    let mut fixed = vec![0u8; IMAGE_BYTES];
    for y in 0..IMAGE_HEIGHT {
        for x in 0..IMAGE_WIDTH {
            let index = (y * IMAGE_WIDTH + x) * 3;
            handeye[index] = (x & 255) as u8;
            handeye[index + 1] = (y & 255) as u8;
            handeye[index + 2] = ((x + y) & 255) as u8;
            fixed[index] = ((2 * x + y) & 255) as u8;
            fixed[index + 1] = ((x + 2 * y) & 255) as u8;
            fixed[index + 2] = ((3 * x + 5 * y) & 255) as u8;
        }
    }
    let observation = Observation {
        handeye_rgb: &handeye,
        fixed_rgb: &fixed,
        state: [0.0, -30.0, 50.0, 10.0, -1.0, 25.0],
    };
    let mut workspace = vec![0.0f32; WORKSPACE_FLOATS];
    let mut actions = [[0.0f32; ACTION_DIM]; ACTION_STEPS];
    let started = Instant::now();
    model
        .predict(&observation, &mut workspace, &mut actions)
        .unwrap();
    println!("elapsed_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);
    for step in [0usize, 1, 50, 99] {
        println!("action[{step}]={:?}", actions[step]);
    }
    let checksum: f64 = actions.iter().flatten().map(|value| *value as f64).sum();
    println!("checksum={checksum:.9}");
}

/// 对实验02冻结的五组字节执行与EL0完全相同的推理和600元素比较。
fn run_cases(model: &ActModel<'_>, root: &Path) {
    // 68MiB工作区只申请一次，与EL0和seL4的测量方式保持一致。
    let mut workspace = vec![0.0f32; WORKSPACE_FLOATS];
    for case_index in 0..5 {
        let directory = root.join(format!("case-{case_index:03}"));
        let handeye = fs::read(directory.join("handeye.rgb")).unwrap();
        let fixed = fs::read(directory.join("fixed.rgb")).unwrap();
        assert_eq!(handeye.len(), IMAGE_BYTES);
        assert_eq!(fixed.len(), IMAGE_BYTES);
        let state_bytes = fs::read(directory.join("state.f32le")).unwrap();
        let mut state = [0.0f32; ACTION_DIM];
        for (slot, bytes) in state.iter_mut().zip(state_bytes.chunks_exact(4)) {
            *slot = f32::from_le_bytes(bytes.try_into().unwrap());
        }
        let reference_bytes = fs::read(directory.join("pytorch-action.f32le")).unwrap();
        assert_eq!(reference_bytes.len(), ACTION_STEPS * ACTION_DIM * 4);

        let observation = Observation { handeye_rgb: &handeye, fixed_rgb: &fixed, state };
        let mut actions = [[0.0f32; ACTION_DIM]; ACTION_STEPS];
        let started = Instant::now();
        model.predict(&observation, &mut workspace, &mut actions).unwrap();
        // 保存被测Rust后端的全部600个float，供后续CSV逐元素审计。
        let mut action_bytes = Vec::with_capacity(ACTION_STEPS * ACTION_DIM * 4);
        for value in actions.iter().flatten() {
            action_bytes.extend_from_slice(&value.to_le_bytes());
        }
        fs::write(directory.join("rust-action.f32le"), action_bytes).unwrap();
        let mut maximum = 0.0f32;
        let mut total = 0.0f64;
        let mut failed = 0usize;
        for (index, actual) in actions.iter().flatten().enumerate() {
            let offset = index * 4;
            let expected = f32::from_le_bytes(reference_bytes[offset..offset + 4].try_into().unwrap());
            let error = (*actual - expected).abs();
            maximum = maximum.max(error);
            total += error as f64;
            let tolerance = ABSOLUTE_TOLERANCE + RELATIVE_TOLERANCE * expected.abs();
            if error > tolerance { failed += 1; }
        }
        println!(
            "case-{case_index:03} elapsed_ms={:.3} mean_abs_error={:.9} max_abs_error={maximum:.9} failed={failed}",
            started.elapsed().as_secs_f64() * 1000.0,
            total / (ACTION_STEPS * ACTION_DIM) as f64,
        );
        assert_eq!(failed, 0);
    }
}
