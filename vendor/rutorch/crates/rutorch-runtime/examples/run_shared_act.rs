//! 在 Linux AArch64 guest 中运行共享 RuTorch ACT，并同时留下可审计的正确性与
//! 稳态计时证据。模型、normalizer 和五组输入均来自 `data/act` 冻结目录。

use std::{
    env, fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use rutorch_runtime::{
    ACTION_DIM, ACTION_STEPS, IMAGE_BYTES, JobKernel, KernelProvider, LoadOptions, Observation,
    PREPARED_OBSERVATION_FLOATS, WORKSPACE_FLOATS, install_parallel_jobs, load_with_options,
    provider_counters, required_persistent_bytes, required_temporary_bytes,
    reset_provider_counters,
};

const ATOL: f32 = 1e-4;
const RTOL: f32 = 1e-5;

fn main() {
    configure_fpcr();
    // 三个变量同时存在才进入协议导出模式；旧位置参数调用保留原有行为。
    let export = match (
        env::var("RUTORCH_RUN_ID"),
        env::var("RUTORCH_REQUEST_SHA256"),
        env::var("RUTORCH_RESULT_DIR"),
    ) {
        (Ok(run_id), Ok(request_sha256), Ok(result_dir)) => {
            assert_eq!(
                (ACTION_STEPS, ACTION_DIM),
                (100, 6),
                "冻结契约动作尺寸不一致"
            );
            assert!(!run_id.is_empty(), "run_id 不能为空");
            assert!(
                request_sha256.len() == 64
                    && request_sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "request_sha256 必须为64位小写hex"
            );
            let result_dir = PathBuf::from(result_dir);
            assert!(result_dir.is_absolute(), "结果目录必须为绝对路径");
            fs::create_dir_all(&result_dir).unwrap();
            Some(ExportRequest {
                run_id,
                request_sha256,
                result_dir,
            })
        }
        _ => None,
    };
    let model_dir = env::args()
        .nth(1)
        .unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/act/model").into());
    let cases_dir = env::args().nth(2).unwrap_or_else(|| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/act/correctness").into()
    });
    let threads = env::var("RUTORCH_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1usize);
    let warmup = env::var("RUTORCH_WARMUP")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3usize);
    let runs = env::var("RUTORCH_RUNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5usize);
    let case_limit = env::var("RUTORCH_CASE_LIMIT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5usize)
        .clamp(1, 5);
    // 协议验收固定五组、无预热、一次后续检查，不允许环境变量改变工作量。
    let (warmup, runs, case_limit) = if export.is_some() {
        match env::var("RUTORCH_RUN_PHASE").as_deref() {
            Ok("benchmark") => (5, 10, 5),
            _ => (0, 1, 5),
        }
    } else {
        (warmup, runs, case_limit)
    };
    let provider = match env::var("RUTORCH_PROVIDER").as_deref() {
        Ok("portable") => KernelProvider::PortableNeon,
        Ok("shared-acl") | Ok("acl") | Err(_) => KernelProvider::SharedAcl,
        Ok(value) => panic!("RUTORCH_PROVIDER 必须是 portable 或 shared-acl，收到 {value}"),
    };

    if provider == KernelProvider::SharedAcl && threads > 1 {
        // Linux guest 的平台层只负责同步等待；计算上下文和 job 分片仍由
        // canonical C shim/执行计划决定。每个 job 使用独占 temporary lane。
        unsafe { install_parallel_jobs(Some(parallel_jobs)) };
    }

    let model_bytes = fs::read(Path::new(&model_dir).join("model.safetensors")).unwrap();
    let stats_bytes = fs::read(
        Path::new(&model_dir).join("policy_preprocessor_step_3_normalizer_processor.safetensors"),
    )
    .unwrap();
    let options = LoadOptions { provider, threads };
    let persistent_bytes = required_persistent_bytes(&model_bytes, &stats_bytes, options).unwrap();
    let temporary_bytes = required_temporary_bytes(options).unwrap();
    let mut persistent_storage = vec![0u64; persistent_bytes.div_ceil(8)];
    // u64 向量保证 8 字节对齐；共享 ACL shim 会在 arena 内部再次向上对齐到64字节。
    let persistent = unsafe {
        core::slice::from_raw_parts_mut(
            persistent_storage
                .as_mut_ptr()
                .cast::<core::mem::MaybeUninit<u8>>(),
            persistent_storage.len() * 8,
        )
    };
    let load_started = Instant::now();
    let model = load_with_options(&model_bytes, &stats_bytes, persistent, options).unwrap();
    let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;
    let metadata = model.metadata();
    println!(
        "provider={provider:?} threads={} model_sha256={} plan_sha256={} provider_build_sha256={:?} persistent_bytes={} temporary_bytes={} load_ms={load_ms:.3}",
        metadata.threads,
        hex(&metadata.model_sha256.unwrap()),
        hex(&metadata.plan_sha256),
        metadata.provider_build_sha256.map(|value| hex(&value)),
        metadata.persistent_bytes,
        metadata.temporary_bytes,
    );
    assert!(
        temporary_bytes <= metadata.temporary_bytes || provider == KernelProvider::PortableNeon
    );

    let mut prepared_input = vec![0.0f32; PREPARED_OBSERVATION_FLOATS];
    let mut workspace = vec![0.0f32; WORKSPACE_FLOATS];
    let mut actions = [[0.0f32; ACTION_DIM]; ACTION_STEPS];
    reset_provider_counters();

    for case_id in 0..case_limit {
        let case_dir = Path::new(&cases_dir).join(format!("case-{case_id:03}"));
        let handeye = fs::read(case_dir.join("handeye.rgb")).unwrap();
        let fixed = fs::read(case_dir.join("fixed.rgb")).unwrap();
        let state_bytes = fs::read(case_dir.join("state.f32le")).unwrap();
        let mut state = [0.0f32; ACTION_DIM];
        for (slot, raw) in state.iter_mut().zip(state_bytes.chunks_exact(4)) {
            *slot = f32::from_le_bytes(raw.try_into().unwrap());
        }
        assert_eq!(handeye.len(), IMAGE_BYTES);
        assert_eq!(fixed.len(), IMAGE_BYTES);
        let observation = Observation {
            handeye_rgb: &handeye,
            fixed_rgb: &fixed,
            state,
        };
        let preprocessing_started = Instant::now();
        let prepared = model
            .prepare_observation(&observation, &mut prepared_input)
            .unwrap();
        let preprocess_ms = preprocessing_started.elapsed().as_secs_f64() * 1000.0;
        if case_id == 0
            && let Some(export) = &export
        {
            for (name, values) in [
                ("handeye", prepared.handeye()),
                ("fixed", prepared.fixed()),
                ("state", &prepared.state()[..]),
            ] {
                let raw: Vec<u8> = values
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect();
                fs::write(
                    export.result_dir.join(format!("prepared-{name}.f32le")),
                    raw,
                )
                .unwrap();
            }
        }
        let started = Instant::now();
        model
            .predict_normalized_prepared(&prepared, &mut workspace, &mut actions)
            .unwrap();
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let postprocess_started = Instant::now();
        model.denormalize_actions(&mut actions);
        let postprocess_ms = postprocess_started.elapsed().as_secs_f64() * 1000.0;
        if let Some(export) = &export {
            // 写入刚刚得到的真实 actions；必须早于现有 assert，失败也可独立审计。
            let file = fs::File::create(export.result_dir.join(format!("case-{case_id:03}.f32le")))
                .unwrap();
            let mut writer = BufWriter::new(file);
            for action in actions.iter().flatten() {
                writer.write_all(&action.to_le_bytes()).unwrap();
            }
            writer.flush().unwrap();
            writer.get_ref().sync_all().unwrap();
        }
        let expected = fs::read(case_dir.join("pytorch-action.f32le")).unwrap();
        let mut failures = 0usize;
        let mut max_abs = 0.0f32;
        for (index, actual) in actions.iter().flatten().enumerate() {
            let offset = index * 4;
            let want = f32::from_le_bytes(expected[offset..offset + 4].try_into().unwrap());
            let error = (*actual - want).abs();
            max_abs = max_abs.max(error);
            if !actual.is_finite() || error > ATOL + RTOL * want.abs() {
                failures += 1;
            }
        }
        println!(
            "case-{case_id:03} correctness={} elapsed_ms={elapsed_ms:.3} preprocess_ms={preprocess_ms:.3} postprocess_ms={postprocess_ms:.3} max_abs_error={max_abs:.9} failures={failures}",
            if failures == 0 { "PASS" } else { "FAIL" }
        );
        assert_eq!(failures, 0, "case-{case_id:03} correctness failed");
    }

    let case_dir = Path::new(&cases_dir).join("case-000");
    let handeye = fs::read(case_dir.join("handeye.rgb")).unwrap();
    let fixed = fs::read(case_dir.join("fixed.rgb")).unwrap();
    let state_bytes = fs::read(case_dir.join("state.f32le")).unwrap();
    let mut state = [0.0f32; ACTION_DIM];
    for (slot, raw) in state.iter_mut().zip(state_bytes.chunks_exact(4)) {
        *slot = f32::from_le_bytes(raw.try_into().unwrap());
    }
    let observation = Observation {
        handeye_rgb: &handeye,
        fixed_rgb: &fixed,
        state,
    };
    let prepared = model
        .prepare_observation(&observation, &mut prepared_input)
        .unwrap();
    if env::var_os("RUTORCH_PROFILE_STAGES").is_some() {
        let mut previous = Instant::now();
        model
            .predict_prepared_with_progress(&prepared, &mut workspace, &mut actions, |stage| {
                let now = Instant::now();
                println!(
                    "stage={stage:?} delta_ms={:.3}",
                    now.duration_since(previous).as_secs_f64() * 1000.0
                );
                previous = now;
            })
            .unwrap();
    }
    for _ in 0..warmup {
        model
            .predict_normalized_prepared(&prepared, &mut workspace, &mut actions)
            .unwrap();
    }
    let before_runs = provider_counters();
    let mut timings = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        model
            .predict_normalized_prepared(&prepared, &mut workspace, &mut actions)
            .unwrap();
        timings.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let after_runs = provider_counters();
    if let Some(export) = &export {
        fs::write(export.result_dir.join("timings.json"), format!("{{\"boundary\":\"normalized-network\",\"warmup\":{warmup},\"runs\":{runs},\"model_samples_ms\":{timings:?}}}\n")).unwrap();
    }
    timings.sort_by(f64::total_cmp);
    let median = (timings[(timings.len() - 1) / 2] + timings[timings.len() / 2]) / 2.0;
    let p95 = timings[((timings.len() * 95).saturating_sub(1) / 100).min(timings.len() - 1)];
    println!(
        "steady_state runs={runs} warmup={warmup} median_ms={median:.3} p95_ms={p95:.3} counters_before={before_runs:?} counters_after={after_runs:?}"
    );
    if let (Some(before), Some(after)) = (before_runs, after_runs) {
        assert_eq!(
            before.prepare_calls, after.prepare_calls,
            "steady state re-prepared kernels"
        );
        assert_eq!(
            before.raw_run_calls, after.raw_run_calls,
            "steady state entered raw path"
        );
    }
    println!(
        "PASS: {case_limit} cases / {} values",
        case_limit * ACTION_STEPS * ACTION_DIM
    );
    if let Some(export) = &export {
        // 五组 correctness 与后续 provider 检查全部通过才原子发布完成清单。
        let cases = (0..5).map(|case_id| format!(
            "{{\"id\":\"case-{case_id:03}\",\"path\":\"case-{case_id:03}.f32le\",\"dtype\":\"f32le\",\"shape\":[100,6],\"values\":600}}"
        )).collect::<Vec<_>>().join(",");
        let result = format!(
            "{{\"schema_version\":1,\"run_id\":{},\"request_sha256\":{},\"complete\":true,\"cases\":[{cases}]}}\n",
            json_string(&export.run_id),
            json_string(&export.request_sha256)
        );
        let temporary = export.result_dir.join(".result.json.tmp");
        let mut file = fs::File::create(&temporary).unwrap();
        file.write_all(result.as_bytes()).unwrap();
        file.sync_all().unwrap();
        fs::rename(temporary, export.result_dir.join("result.json")).unwrap();
    }
}

// 只保存本次导出身份与结果目录；模型读取和计算策略仍属于原有运行时。
struct ExportRequest {
    run_id: String,
    request_sha256: String,
    result_dir: PathBuf,
}

fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            character if character <= '\u{1f}' => {
                output.push_str(&format!("\\u{:04x}", character as u32))
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

struct SendContext {
    context: usize,
    kernel: JobKernel,
}

// C shim 保证 context 指向当前同步 predict 栈帧，job 输出区互不重叠；这里
// 只把整数地址跨线程传递，线程在 scope 结束前必定 join。
unsafe impl Send for SendContext {}

unsafe extern "C" fn parallel_jobs(context: *mut (), jobs: usize, kernel: JobKernel) {
    let context = SendContext {
        context: context as usize,
        kernel,
    };
    std::thread::scope(|scope| {
        for job in 1..jobs {
            let context = SendContext {
                context: context.context,
                kernel: context.kernel,
            };
            scope.spawn(move || {
                configure_fpcr();
                // SAFETY: callback只在scope内同步执行，调用者保证 context 的布局
                // 和每个 job 的 scratch/output 分片有效。
                unsafe { (context.kernel)(context.context as *mut (), job) };
            });
        }
        // 当前调用线程承担 job 0，减少一次线程切换和同步等待。
        unsafe { (context.kernel)(context.context as *mut (), 0) };
    });
}

/// 主线程和计算worker统一FPCR；校验发生于每个新线程入口，计时内不输出日志。
fn configure_fpcr() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let mut value: u64;
        core::arch::asm!("mrs {v}, fpcr", v = out(reg) value, options(nomem, nostack));
        value &= !((1 << 24) | (1 << 19) | (1 << 25) | (3 << 22));
        core::arch::asm!("msr fpcr, {v}", "isb", v = in(reg) value, options(nomem, nostack));
        core::arch::asm!("mrs {v}, fpcr", v = out(reg) value, options(nomem, nostack));
        assert_eq!(value & ((1 << 24) | (1 << 19) | (1 << 25) | (3 << 22)), 0);
    }
}
