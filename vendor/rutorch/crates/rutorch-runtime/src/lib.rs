//! LeRobot ACT策略的`no_std`用户态推理运行时。
//!
//! 本crate不执行文件I/O、不分配Kernel资源，也不依赖PyTorch。调用者负责
//! 提供只读safetensors字节、两张RGB图像和推理工作区；运行时负责验证权重、
//! 归一化输入并执行ACT网络。这样同一实现可以在macOS测试和EL0 libOS复用。

#![no_std]

#[cfg(all(feature = "platform-linux", feature = "platform-libos"))]
compile_error!("platform-linux 与 platform-libos 不能同时启用");

#[cfg(test)]
extern crate std;

mod engine;
pub mod execution;
#[cfg(feature = "kernel-acl-shared")]
mod prepared;
pub mod safetensors;
mod sha256;

#[cfg(all(feature = "kernel-acl-linux-full", feature = "platform-libos"))]
compile_error!("kernel-acl-linux-full 依赖 Linux/C++ runtime，不能进入 LibOS");

/// Linux full ACL 仅作为诊断/性能上限；ACT canonical shared provider 不依赖它。
#[cfg(feature = "kernel-acl-linux-full")]
pub use rutorch_kernels_acl as linux_full_acl;

pub use engine::{
    ActModel, InferenceStage, JobKernel, ParallelJobsCallback, ParallelRowsCallback, RowKernel,
    install_parallel_jobs, install_parallel_rows,
};
pub use execution::{
    ACT_EXECUTION_PLAN, BufferSlot, CompiledExecutionPlan, Shape, SlotId, TensorLayout, TensorView,
    TensorViewMut,
};

/// 共享内核的运行期计数，用于证明稳态推理没有重新 prepare/pack 或调用旧 raw 路径。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderCounters {
    pub prepare_calls: usize,
    pub run_calls: usize,
    pub raw_run_calls: usize,
}

/// 读取当前进程的共享 provider 计数；portable 构建返回 `None`。
pub fn provider_counters() -> Option<ProviderCounters> {
    #[cfg(feature = "kernel-acl-shared")]
    {
        let counters = act_kernels_acl::counters();
        Some(ProviderCounters {
            prepare_calls: counters.prepare_calls,
            run_calls: counters.run_calls,
            raw_run_calls: counters.raw_run_calls,
        })
    }
    #[cfg(not(feature = "kernel-acl-shared"))]
    {
        None
    }
}

/// 清零共享 provider 计数；必须在并发推理开始前调用。
pub fn reset_provider_counters() {
    #[cfg(feature = "kernel-acl-shared")]
    act_kernels_acl::reset_counters();
}

/// ACT Conv/GEMM 使用的计算 provider。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelProvider {
    /// 现有纯 Rust HWC/NHWC NEON 路径。
    PortableNeon,
    /// EL0 内静态链接的 ACL CPU/NEON 路径。
    SharedAcl,
}

/// 计划中视觉与 Transformer 的固定布局。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Layout {
    /// 相机与 ResNet 中间值使用 NHWC。
    NhwcVision,
    /// Transformer token 使用连续 row-major。
    RowMajorTransformer,
}

/// 加载后可读取的规范计划元数据。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanMetadata {
    pub version: u32,
    pub plan_sha256: [u8; 32],
    pub model_sha256: Option<[u8; 32]>,
    pub provider: KernelProvider,
    pub provider_build_sha256: Option<[u8; 32]>,
    pub layout: Layout,
    pub persistent_bytes: usize,
    pub temporary_bytes: usize,
    pub threads: usize,
}

/// ACT provider 的加载选项。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadOptions {
    /// 选择 Conv/GEMM provider。
    pub provider: KernelProvider,
    /// provider 请求的逻辑线程数；0 表示使用平台默认值。
    pub threads: usize,
}

/// 查询指定 provider 在加载期需要的持久化字节数。
pub fn required_persistent_bytes(
    model: &[u8],
    stats: &[u8],
    options: LoadOptions,
) -> Result<usize, Error> {
    ActModel::required_persistent_bytes(model, stats, options)
}

/// 查询指定 provider 在稳态运行时需要的最大临时字节数。
pub fn required_temporary_bytes(options: LoadOptions) -> Result<usize, Error> {
    ActModel::required_temporary_bytes(options)
}

/// 使用显式 provider 和调用者 arena 加载模型。
pub fn load_with_options<'a>(
    model: &'a [u8],
    stats: &'a [u8],
    persistent: &'a mut [core::mem::MaybeUninit<u8>],
    options: LoadOptions,
) -> Result<ActModel<'a>, Error> {
    ActModel::load_with_options(model, stats, persistent, options)
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            provider: KernelProvider::PortableNeon,
            threads: 1,
        }
    }
}

pub const IMAGE_WIDTH: usize = 640;
pub const IMAGE_HEIGHT: usize = 360;
pub const IMAGE_CHANNELS: usize = 3;
pub const IMAGE_BYTES: usize = IMAGE_WIDTH * IMAGE_HEIGHT * IMAGE_CHANNELS;
pub const STATE_DIM: usize = 6;
pub const ACTION_DIM: usize = 6;
pub const ACTION_STEPS: usize = 100;
/// 两张已经归一化的 HWC FP32 图像所需元素数。
pub const PREPARED_OBSERVATION_FLOATS: usize = 2 * IMAGE_HEIGHT * IMAGE_WIDTH * IMAGE_CHANNELS;
/// 完整ACT前向传播所需的复用工作区，约68MiB。
pub const WORKSPACE_FLOATS: usize = 17_000_000;

/// LeRobot策略的一次观测；图像使用RGB、HWC、每通道`0..=255`布局。
pub struct Observation<'a> {
    pub handeye_rgb: &'a [u8],
    pub fixed_rgb: &'a [u8],
    pub state: [f32; STATE_DIM],
}

/// 已完成图像和 state 归一化的模型核心输入。
pub struct PreparedObservation<'a> {
    pub(crate) handeye: &'a [f32],
    pub(crate) fixed: &'a [f32],
    pub(crate) state: [f32; STATE_DIM],
}

impl<'a> PreparedObservation<'a> {
    /// 借用已归一化NHWC图像和state，不再做预处理；调用者保持缓冲区生命周期。
    pub fn from_parts(
        handeye: &'a [f32],
        fixed: &'a [f32],
        state: [f32; STATE_DIM],
    ) -> Result<Self, Error> {
        let size = IMAGE_HEIGHT * IMAGE_WIDTH * IMAGE_CHANNELS;
        if handeye.len() != size || fixed.len() != size || !state.iter().all(|v| v.is_finite()) {
            return Err(Error::InvalidImage);
        }
        Ok(Self {
            handeye,
            fixed,
            state,
        })
    }
    /// 返回 handeye 的连续 NHWC/HWC FP32 数据。
    pub fn handeye(&self) -> &[f32] {
        self.handeye
    }

    /// 返回 fixed camera 的连续 NHWC/HWC FP32 数据。
    pub fn fixed(&self) -> &[f32] {
        self.fixed
    }

    /// 返回已归一化的机器人状态。
    pub const fn state(&self) -> [f32; STATE_DIM] {
        self.state
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidSafetensors,
    UnsupportedDtype,
    TensorNotFound,
    ShapeMismatch,
    InvalidImage,
    WorkspaceTooSmall,
    /// 当前 ELF 没有链接 freestanding ACL provider。
    AclProviderUnavailable,
    /// provider 的持久化 workspace 不足。
    PersistentWorkspaceTooSmall,
}

/// 在目标 AArch64 LibOS 中执行一组小型 ACL golden，覆盖非整齐 GEMM、偏置、
/// 3x3 padding Conv 和尾行分片。golden 是手工可复算的 FP32 数值，不依赖堆、
/// 文件系统或宿主 Python；完整 ACT 的 PyTorch golden 仍由外部 case 文件比较。
#[cfg(feature = "kernel-acl-shared")]
pub fn acl_operator_golden_smoke() -> Result<(), Error> {
    #[repr(align(64))]
    struct Aligned([u8; 4096]);

    let gemm = act_kernels_acl::GemmConfig {
        rows: 2,
        inner: 3,
        columns: 2,
    };
    let gemm_input = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
    // ACL shim 的 right 契约是 ACT 的 [out, in] row-major。
    let gemm_right = [10.0f32, 20.0, 30.0, 1.0, 2.0, 3.0];
    let gemm_bias = [0.5f32, -0.5];
    let mut gemm_output = [0.0f32; 4];
    let gemm_bytes = act_kernels_acl::gemm_raw_temporary_bytes(&gemm)
        .map_err(|_| Error::AclProviderUnavailable)?;
    let mut gemm_temp = Aligned([0; 4096]);
    // 独立验证容量、shape、对齐错误在FFI边界被拒绝，不进入实际微内核。
    let mut persistent = Aligned([0; 4096]);
    if act_kernels_acl::prepare_gemm(&gemm, &gemm_right, None, &mut persistent.0[1..]).is_ok()
        || act_kernels_acl::prepare_gemm(&gemm, &gemm_right, None, &mut persistent.0[..1]).is_ok()
        || act_kernels_acl::run_gemm_raw(
            &gemm,
            &gemm_input[..5],
            &gemm_right,
            None,
            &mut gemm_output,
            &mut gemm_temp.0,
        )
        .is_ok()
        || act_kernels_acl::run_gemm_raw(
            &gemm,
            &gemm_input,
            &gemm_right,
            None,
            &mut gemm_output,
            &mut gemm_temp.0[..1],
        )
        .is_ok()
    {
        return Err(Error::ShapeMismatch);
    }
    if gemm_bytes > gemm_temp.0.len() {
        return Err(Error::WorkspaceTooSmall);
    }
    act_kernels_acl::run_gemm_raw(
        &gemm,
        &gemm_input,
        &gemm_right,
        Some(&gemm_bias),
        &mut gemm_output,
        &mut gemm_temp.0[..gemm_bytes],
    )
    .map_err(|_| Error::AclProviderUnavailable)?;
    let gemm_expected = [140.5f32, 13.5, 320.5, 31.5];
    for (actual, expected) in gemm_output.into_iter().zip(gemm_expected) {
        if (actual - expected).abs() > 1.0e-4 + 1.0e-5 * expected.abs() {
            return Err(Error::ShapeMismatch);
        }
    }

    let conv = act_kernels_acl::Conv2dConfig {
        input_height: 3,
        input_width: 3,
        input_channels: 1,
        output_channels: 1,
        kernel_height: 3,
        kernel_width: 3,
        output_height: 3,
        output_width: 3,
        strides: [1, 1],
        pads: [1, 1, 1, 1],
        dilations: [1, 1],
    };
    let conv_input = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0];
    let conv_weight = [1.0f32; 9];
    let mut conv_output = [0.0f32; 9];
    let conv_bytes = act_kernels_acl::conv2d_raw_temporary_bytes(&conv)
        .map_err(|_| Error::AclProviderUnavailable)?;
    let mut conv_temp = Aligned([0; 4096]);
    if conv_bytes > conv_temp.0.len() {
        return Err(Error::WorkspaceTooSmall);
    }
    act_kernels_acl::run_conv2d_raw(
        &conv,
        &conv_input,
        &conv_weight,
        None,
        &mut conv_output,
        &mut conv_temp.0[..conv_bytes],
    )
    .map_err(|_| Error::AclProviderUnavailable)?;
    let conv_expected = [12.0f32, 21.0, 16.0, 27.0, 45.0, 33.0, 24.0, 39.0, 28.0];
    for (actual, expected) in conv_output.into_iter().zip(conv_expected) {
        if (actual - expected).abs() > 1.0e-4 + 1.0e-5 * expected.abs() {
            return Err(Error::ShapeMismatch);
        }
    }
    Ok(())
}

/// 训练仓库中保存的输入/输出统计。
///
/// 图像统计由normalizer文件读取；state/action统计也从文件读取，避免重新
/// 训练后忘记同步硬编码常量。
#[derive(Clone, Copy)]
pub struct Normalization {
    pub handeye_mean: [f32; IMAGE_CHANNELS],
    pub handeye_std: [f32; IMAGE_CHANNELS],
    pub fixed_mean: [f32; IMAGE_CHANNELS],
    pub fixed_std: [f32; IMAGE_CHANNELS],
    pub state_mean: [f32; STATE_DIM],
    pub state_std: [f32; STATE_DIM],
    pub action_mean: [f32; ACTION_DIM],
    pub action_std: [f32; ACTION_DIM],
}

impl Normalization {
    /// 从LeRobot preprocessor safetensors读取MEAN_STD参数。
    pub fn from_safetensors(bytes: &[u8]) -> Result<Self, Error> {
        let tensors = safetensors::SafeTensors::parse(bytes)?;
        Ok(Self {
            handeye_mean: read_array(&tensors, "observation.images.handeye.mean")?,
            handeye_std: read_array(&tensors, "observation.images.handeye.std")?,
            fixed_mean: read_array(&tensors, "observation.images.fixed.mean")?,
            fixed_std: read_array(&tensors, "observation.images.fixed.std")?,
            state_mean: read_array(&tensors, "observation.state.mean")?,
            state_std: read_array(&tensors, "observation.state.std")?,
            action_mean: read_array(&tensors, "action.mean")?,
            action_std: read_array(&tensors, "action.std")?,
        })
    }

    #[inline]
    pub fn normalize_state(&self, input: [f32; STATE_DIM]) -> [f32; STATE_DIM] {
        let mut output = [0.0; STATE_DIM];
        let mut index = 0;
        while index < STATE_DIM {
            output[index] = (input[index] - self.state_mean[index]) / self.state_std[index];
            index += 1;
        }
        output
    }

    #[inline]
    pub fn denormalize_action(&self, input: [f32; ACTION_DIM]) -> [f32; ACTION_DIM] {
        let mut output = [0.0; ACTION_DIM];
        let mut index = 0;
        while index < ACTION_DIM {
            output[index] = input[index] * self.action_std[index] + self.action_mean[index];
            index += 1;
        }
        output
    }
}

fn read_array<const N: usize>(
    tensors: &safetensors::SafeTensors<'_>,
    name: &str,
) -> Result<[f32; N], Error> {
    let tensor = tensors.tensor(name)?;
    if tensor.element_count() != N {
        return Err(Error::ShapeMismatch);
    }
    let values = tensor.f32_data()?;
    let mut output = [0.0; N];
    output.copy_from_slice(values);
    Ok(output)
}

/// 检查ACT固定输入契约，避免卷积层在错误图像大小上越界。
pub fn validate_observation(observation: &Observation<'_>) -> Result<(), Error> {
    if observation.handeye_rgb.len() != IMAGE_BYTES
        || observation.fixed_rgb.len() != IMAGE_BYTES
        || !observation.state.iter().all(|value| value.is_finite())
    {
        return Err(Error::InvalidImage);
    }
    Ok(())
}

/// 返回固定输入/模型字节的 SHA-256；用于 Linux 与 LibOS 证据关联。
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    sha256::digest(bytes)
}

#[cfg(test)]
mod input_contract_tests {
    use super::*;
    #[test]
    fn prepared_rejects_wrong_shape_and_nonfinite_state() {
        assert!(PreparedObservation::from_parts(&[], &[], [0.0; STATE_DIM]).is_err());
        let image = std::vec![0.0; IMAGE_HEIGHT * IMAGE_WIDTH * IMAGE_CHANNELS];
        assert!(PreparedObservation::from_parts(&image, &image, [f32::NAN; STATE_DIM]).is_err());
        assert!(PreparedObservation::from_parts(&image, &image, [0.0; STATE_DIM]).is_ok());
    }
}
