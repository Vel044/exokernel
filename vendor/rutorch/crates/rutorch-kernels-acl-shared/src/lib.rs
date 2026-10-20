//! Exokernel EL0 使用的 ACL CPU/NEON 窄边界。
//!
//! 本 crate 不暴露 Arm Compute Library 的 C++ 类型，也不依赖 `std`、glibc、
//! OpenMP 或动态加载器。ACL 的静态 archive 和第一方 C shim 由最终 AArch64
//! LibOS 构建提供；未启用 `acl-freestanding` 时，所有入口都明确报告不可用。

#![no_std]

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

/// 固定的上游 ACL 版本。
pub const ACL_VERSION: &str = "52.7.0";

/// 共享微内核构建标识；源码树 SHA 由离线构建 manifest 另外记录。
pub const PROVIDER_BUILD_ID: &str = "acl-v52.7.0-a64-sgemm-asimd-8x12-v2";

static PREPARE_CALLS: AtomicUsize = AtomicUsize::new(0);
static RUN_CALLS: AtomicUsize = AtomicUsize::new(0);
static RAW_RUN_CALLS: AtomicUsize = AtomicUsize::new(0);

/// 用于验证稳态推理没有重新打包权重的调用计数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelCounters {
    pub prepare_calls: usize,
    pub run_calls: usize,
    pub raw_run_calls: usize,
}

/// 读取当前进程的 provider 调用计数。
pub fn counters() -> KernelCounters {
    KernelCounters {
        prepare_calls: PREPARE_CALLS.load(Ordering::Relaxed),
        run_calls: RUN_CALLS.load(Ordering::Relaxed),
        raw_run_calls: RAW_RUN_CALLS.load(Ordering::Relaxed),
    }
}

/// 清零 benchmark/test 计数；不得在并发推理期间调用。
pub fn reset_counters() {
    PREPARE_CALLS.store(0, Ordering::Relaxed);
    RUN_CALLS.store(0, Ordering::Relaxed);
    RAW_RUN_CALLS.store(0, Ordering::Relaxed);
}

/// ACL provider 返回的错误类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// 当前构建没有链接 freestanding ACL archive。
    Unavailable,
    /// 配置或输入 shape 不符合 provider 契约。
    InvalidArgument,
    /// 调用者提供的持久化或临时 workspace 太小。
    WorkspaceTooSmall,
    /// C shim 或 ACL 内核报告执行失败。
    ExecutionFailed,
}

/// 固定 FP32 NHWC 卷积配置。
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Conv2dConfig {
    /// 输入高度、宽度和 channel 数。
    pub input_height: usize,
    pub input_width: usize,
    pub input_channels: usize,
    /// 输出 channel 数。
    pub output_channels: usize,
    /// kernel 高度和宽度。
    pub kernel_height: usize,
    pub kernel_width: usize,
    /// 输出高度、宽度。
    pub output_height: usize,
    pub output_width: usize,
    /// `[y, x]` stride。
    pub strides: [usize; 2],
    /// `[top, left, bottom, right]` padding。
    pub pads: [usize; 4],
    /// `[y, x]` dilation。
    pub dilations: [usize; 2],
}

/// 固定 FP32 row-major GEMM 配置。
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GemmConfig {
    /// 左矩阵行数。
    pub rows: usize,
    /// 归约维度。
    pub inner: usize,
    /// 右矩阵列数。
    pub columns: usize,
}

/// 不透明的 ACL 预备句柄。
///
/// 句柄的所有权在 Rust 侧；其内部对象只允许在创建它的 LibOS 线程同步
/// 使用。C++ 侧不得把该指针保存到 Rust 调用边界之外。
pub struct Handle {
    #[cfg_attr(not(feature = "acl-freestanding"), allow(dead_code))]
    raw: *mut c_void,
    #[cfg_attr(not(feature = "acl-freestanding"), allow(dead_code))]
    kind: HandleKind,
    input_len: usize,
    output_len: usize,
    temporary_bytes: usize,
}

impl Handle {
    /// 返回该固定 shape 句柄每次运行至少需要的临时字节数。
    pub const fn temporary_bytes(&self) -> usize {
        self.temporary_bytes
    }
}

#[derive(Clone, Copy)]
#[cfg_attr(not(feature = "acl-freestanding"), allow(dead_code))]
enum HandleKind {
    Conv,
    Gemm,
}

// ACL bare-metal 首版使用单个同步推理线程；四核并行由外层 scheduler bridge
// 控制，句柄本身不跨线程共享。

/// 当前二进制是否包含 freestanding ACL provider。
pub const fn is_available() -> bool {
    cfg!(rutorch_acl_shared_available)
}

/// 查询 ACL Conv2d 所需的持久化 workspace 字节数。
pub fn conv2d_persistent_bytes(config: &Conv2dConfig) -> Result<usize, Error> {
    ensure_config(
        config.input_height,
        config.input_width,
        config.input_channels,
    )?;
    #[cfg(feature = "acl-freestanding")]
    {
        let mut bytes = 0;
        let status = unsafe { rutorch_acl_conv2d_query_persistent(config, &mut bytes) };
        if status == 0 {
            Ok(bytes)
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = config;
        Err(Error::Unavailable)
    }
}

/// 查询 ACL GEMM 所需的持久化 workspace 字节数。
pub fn gemm_persistent_bytes(config: &GemmConfig) -> Result<usize, Error> {
    ensure_config(config.rows, config.inner, config.columns)?;
    #[cfg(feature = "acl-freestanding")]
    {
        let mut bytes = 0;
        let status = unsafe { rutorch_acl_gemm_query_persistent(config, &mut bytes) };
        if status == 0 {
            Ok(bytes)
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = config;
        Err(Error::Unavailable)
    }
}

/// 查询 Conv2d 稳态执行所需的临时 workspace 字节数。
pub fn conv2d_temporary_bytes(config: &Conv2dConfig) -> Result<usize, Error> {
    ensure_config(
        config.output_height,
        config.output_width,
        config.output_channels,
    )?;
    #[cfg(feature = "acl-freestanding")]
    {
        let mut bytes = 0;
        let status = unsafe { rutorch_acl_conv2d_query_temporary(config, &mut bytes) };
        if status == 0 {
            Ok(bytes)
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = config;
        Err(Error::Unavailable)
    }
}

/// 查询 GEMM 稳态执行所需的临时 workspace 字节数。
pub fn gemm_temporary_bytes(config: &GemmConfig) -> Result<usize, Error> {
    ensure_config(config.rows, config.inner, config.columns)?;
    #[cfg(feature = "acl-freestanding")]
    {
        let mut bytes = 0;
        let status = unsafe { rutorch_acl_gemm_query_temporary(config, &mut bytes) };
        if status == 0 {
            Ok(bytes)
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = config;
        Err(Error::Unavailable)
    }
}

/// 查询一次性 GEMM 调用（权重尚未预打包）所需的临时空间。
///
/// ACT 首轮接线使用该入口验证整图；正式模型加载可改用上面的两阶段句柄
/// 接口，把右矩阵放入 persistent arena。两条路径共享同一个 ACL 8x12 内核。
pub fn gemm_raw_temporary_bytes(config: &GemmConfig) -> Result<usize, Error> {
    ensure_config(config.rows, config.inner, config.columns)?;
    #[cfg(feature = "acl-freestanding")]
    {
        let mut bytes = 0;
        let status = unsafe { rutorch_acl_gemm_query_raw_temporary(config, &mut bytes) };
        if status == 0 {
            Ok(bytes)
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = config;
        Err(Error::Unavailable)
    }
}

/// 查询一次性 NHWC Conv2d 调用所需的临时空间。
pub fn conv2d_raw_temporary_bytes(config: &Conv2dConfig) -> Result<usize, Error> {
    ensure_config(
        config.output_height,
        config.output_width,
        config.output_channels,
    )?;
    #[cfg(feature = "acl-freestanding")]
    {
        let mut bytes = 0;
        let status = unsafe { rutorch_acl_conv2d_query_raw_temporary(config, &mut bytes) };
        if status == 0 {
            Ok(bytes)
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = config;
        Err(Error::Unavailable)
    }
}

/// 使用 ACL 微内核执行一次 row-major `input * right + bias`。
pub fn run_gemm_raw(
    config: &GemmConfig,
    input: &[f32],
    right: &[f32],
    bias: Option<&[f32]>,
    output: &mut [f32],
    temporary: &mut [u8],
) -> Result<(), Error> {
    RAW_RUN_CALLS.fetch_add(1, Ordering::Relaxed);
    let input_len = config
        .rows
        .checked_mul(config.inner)
        .ok_or(Error::InvalidArgument)?;
    let output_len = config
        .rows
        .checked_mul(config.columns)
        .ok_or(Error::InvalidArgument)?;
    if input.len() != input_len
        || right.len()
            != config
                .inner
                .checked_mul(config.columns)
                .ok_or(Error::InvalidArgument)?
        || output.len() != output_len
        || bias.is_some_and(|value| value.len() != config.columns)
    {
        return Err(Error::InvalidArgument);
    }
    #[cfg(feature = "acl-freestanding")]
    {
        let status = unsafe {
            rutorch_acl_gemm_run_raw(
                config,
                input.as_ptr(),
                right.as_ptr(),
                bias.map_or(core::ptr::null(), <[f32]>::as_ptr),
                output.as_mut_ptr(),
                temporary.as_mut_ptr().cast(),
                temporary.len(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = (config, input, right, bias, output, temporary);
        Err(Error::Unavailable)
    }
}

/// 使用 ACL 微内核执行一次 FP32 NHWC Conv2d（OIHW 权重）。
pub fn run_conv2d_raw(
    config: &Conv2dConfig,
    input: &[f32],
    weight: &[f32],
    bias: Option<&[f32]>,
    output: &mut [f32],
    temporary: &mut [u8],
) -> Result<(), Error> {
    RAW_RUN_CALLS.fetch_add(1, Ordering::Relaxed);
    let input_len = config
        .input_height
        .checked_mul(config.input_width)
        .and_then(|value| value.checked_mul(config.input_channels))
        .ok_or(Error::InvalidArgument)?;
    let output_len = config
        .output_height
        .checked_mul(config.output_width)
        .and_then(|value| value.checked_mul(config.output_channels))
        .ok_or(Error::InvalidArgument)?;
    let weight_len = config
        .output_channels
        .checked_mul(config.input_channels)
        .and_then(|value| value.checked_mul(config.kernel_height))
        .and_then(|value| value.checked_mul(config.kernel_width))
        .ok_or(Error::InvalidArgument)?;
    if input.len() != input_len
        || weight.len() != weight_len
        || output.len() != output_len
        || bias.is_some_and(|value| value.len() != config.output_channels)
    {
        return Err(Error::InvalidArgument);
    }
    #[cfg(feature = "acl-freestanding")]
    {
        let status = unsafe {
            rutorch_acl_conv2d_run_raw(
                config,
                input.as_ptr(),
                weight.as_ptr(),
                bias.map_or(core::ptr::null(), <[f32]>::as_ptr),
                output.as_mut_ptr(),
                temporary.as_mut_ptr().cast(),
                temporary.len(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = (config, input, weight, bias, output, temporary);
        Err(Error::Unavailable)
    }
}

/// 准备常量权重并创建 Conv2d 句柄。
///
/// `persistent` 的地址、长度和对齐均由调用者提供；C shim 只能在这段内存
/// 中保存预打包权重和 ACL 对象，不能借用 Rust 临时栈或全局堆。
pub fn prepare_conv2d(
    config: &Conv2dConfig,
    weight: &[f32],
    bias: Option<&[f32]>,
    persistent: &mut [u8],
) -> Result<Handle, Error> {
    PREPARE_CALLS.fetch_add(1, Ordering::Relaxed);
    let weight_len = config
        .output_channels
        .checked_mul(config.input_channels)
        .and_then(|value| value.checked_mul(config.kernel_height))
        .and_then(|value| value.checked_mul(config.kernel_width))
        .ok_or(Error::InvalidArgument)?;
    if weight.len() != weight_len || bias.is_some_and(|value| value.len() != config.output_channels)
    {
        return Err(Error::InvalidArgument);
    }
    #[cfg(feature = "acl-freestanding")]
    {
        let mut raw = core::ptr::null_mut();
        let status = unsafe {
            rutorch_acl_conv2d_prepare(
                config,
                weight.as_ptr(),
                bias.map_or(core::ptr::null(), <[f32]>::as_ptr),
                persistent.as_mut_ptr().cast(),
                persistent.len(),
                &mut raw,
            )
        };
        if status == 0 && !raw.is_null() {
            Ok(Handle {
                raw,
                kind: HandleKind::Conv,
                input_len: config.input_height * config.input_width * config.input_channels,
                output_len: config.output_height * config.output_width * config.output_channels,
                temporary_bytes: conv2d_temporary_bytes(config)?,
            })
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = (config, weight, bias, persistent);
        Err(Error::Unavailable)
    }
}

/// 准备常量右矩阵并创建 GEMM 句柄。
pub fn prepare_gemm(
    config: &GemmConfig,
    right: &[f32],
    bias: Option<&[f32]>,
    persistent: &mut [u8],
) -> Result<Handle, Error> {
    PREPARE_CALLS.fetch_add(1, Ordering::Relaxed);
    let right_len = config
        .inner
        .checked_mul(config.columns)
        .ok_or(Error::InvalidArgument)?;
    if right.len() != right_len || bias.is_some_and(|value| value.len() != config.columns) {
        return Err(Error::InvalidArgument);
    }
    #[cfg(feature = "acl-freestanding")]
    {
        let mut raw = core::ptr::null_mut();
        let status = unsafe {
            rutorch_acl_gemm_prepare(
                config,
                right.as_ptr(),
                bias.map_or(core::ptr::null(), <[f32]>::as_ptr),
                persistent.as_mut_ptr().cast(),
                persistent.len(),
                &mut raw,
            )
        };
        if status == 0 && !raw.is_null() {
            Ok(Handle {
                raw,
                kind: HandleKind::Gemm,
                input_len: config.rows * config.inner,
                output_len: config.rows * config.columns,
                temporary_bytes: gemm_temporary_bytes(config)?,
            })
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = (config, right, bias, persistent);
        Err(Error::Unavailable)
    }
}

/// 在已准备的 Conv2d 上同步执行一次。
pub fn run_conv2d(
    handle: &Handle,
    input: &[f32],
    output: &mut [f32],
    temporary: &mut [u8],
) -> Result<(), Error> {
    if !matches!(handle.kind, HandleKind::Conv)
        || input.len() != handle.input_len
        || output.len() != handle.output_len
        || temporary.len() < handle.temporary_bytes
    {
        return Err(Error::InvalidArgument);
    }
    RUN_CALLS.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "acl-freestanding")]
    {
        let status = unsafe {
            rutorch_acl_conv2d_run(
                handle.raw,
                input.as_ptr(),
                output.as_mut_ptr(),
                temporary.as_mut_ptr().cast(),
                temporary.len(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = (handle, input, output, temporary);
        Err(Error::Unavailable)
    }
}

/// 在已准备的 GEMM 上同步执行一次。
pub fn run_gemm(
    handle: &Handle,
    input: &[f32],
    output: &mut [f32],
    temporary: &mut [u8],
) -> Result<(), Error> {
    if !matches!(handle.kind, HandleKind::Gemm)
        || input.len() != handle.input_len
        || output.len() != handle.output_len
        || temporary.len() < handle.temporary_bytes
    {
        return Err(Error::InvalidArgument);
    }
    RUN_CALLS.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "acl-freestanding")]
    {
        let status = unsafe {
            rutorch_acl_gemm_run(
                handle.raw,
                input.as_ptr(),
                output.as_mut_ptr(),
                temporary.as_mut_ptr().cast(),
                temporary.len(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = (handle, input, output, temporary);
        Err(Error::Unavailable)
    }
}

/// 对 `input + addend` 使用同一个已预打包右矩阵；Attention 的 Q/K 投影可由此
/// 消除完整临时加法 Tensor。
pub fn run_gemm_sum(
    handle: &Handle,
    input: &[f32],
    addend: &[f32],
    output: &mut [f32],
    temporary: &mut [u8],
) -> Result<(), Error> {
    if !matches!(handle.kind, HandleKind::Gemm)
        || input.len() != handle.input_len
        || addend.len() != handle.input_len
        || output.len() != handle.output_len
        || temporary.len() < handle.temporary_bytes
    {
        return Err(Error::InvalidArgument);
    }
    RUN_CALLS.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "acl-freestanding")]
    {
        let status = unsafe {
            rutorch_acl_gemm_run_sum(
                handle.raw,
                input.as_ptr(),
                addend.as_ptr(),
                output.as_mut_ptr(),
                temporary.as_mut_ptr().cast(),
                temporary.len(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(Error::ExecutionFailed)
        }
    }
    #[cfg(not(feature = "acl-freestanding"))]
    {
        let _ = (handle, input, addend, output, temporary);
        Err(Error::Unavailable)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        #[cfg(feature = "acl-freestanding")]
        unsafe {
            match self.kind {
                HandleKind::Conv => rutorch_acl_conv2d_destroy(self.raw),
                HandleKind::Gemm => rutorch_acl_gemm_destroy(self.raw),
            }
        }
    }
}

fn ensure_config(a: usize, b: usize, c: usize) -> Result<(), Error> {
    if a == 0 || b == 0 || c == 0 {
        Err(Error::InvalidArgument)
    } else {
        Ok(())
    }
}

#[cfg(feature = "acl-freestanding")]
unsafe extern "C" {
    fn rutorch_acl_conv2d_query_persistent(config: *const Conv2dConfig, bytes: *mut usize) -> i32;
    fn rutorch_acl_conv2d_query_temporary(config: *const Conv2dConfig, bytes: *mut usize) -> i32;
    fn rutorch_acl_conv2d_prepare(
        config: *const Conv2dConfig,
        weight: *const f32,
        bias: *const f32,
        persistent: *mut c_void,
        persistent_bytes: usize,
        handle: *mut *mut c_void,
    ) -> i32;
    fn rutorch_acl_conv2d_run(
        handle: *mut c_void,
        input: *const f32,
        output: *mut f32,
        temporary: *mut c_void,
        temporary_bytes: usize,
    ) -> i32;
    fn rutorch_acl_conv2d_destroy(handle: *mut c_void);
    fn rutorch_acl_gemm_query_persistent(config: *const GemmConfig, bytes: *mut usize) -> i32;
    fn rutorch_acl_gemm_query_temporary(config: *const GemmConfig, bytes: *mut usize) -> i32;
    fn rutorch_acl_gemm_query_raw_temporary(config: *const GemmConfig, bytes: *mut usize) -> i32;
    fn rutorch_acl_gemm_prepare(
        config: *const GemmConfig,
        right: *const f32,
        bias: *const f32,
        persistent: *mut c_void,
        persistent_bytes: usize,
        handle: *mut *mut c_void,
    ) -> i32;
    fn rutorch_acl_gemm_run(
        handle: *mut c_void,
        input: *const f32,
        output: *mut f32,
        temporary: *mut c_void,
        temporary_bytes: usize,
    ) -> i32;
    fn rutorch_acl_gemm_run_sum(
        handle: *mut c_void,
        input: *const f32,
        addend: *const f32,
        output: *mut f32,
        temporary: *mut c_void,
        temporary_bytes: usize,
    ) -> i32;
    fn rutorch_acl_gemm_destroy(handle: *mut c_void);
    fn rutorch_acl_gemm_run_raw(
        config: *const GemmConfig,
        input: *const f32,
        right: *const f32,
        bias: *const f32,
        output: *mut f32,
        temporary: *mut c_void,
        temporary_bytes: usize,
    ) -> i32;
    fn rutorch_acl_conv2d_query_raw_temporary(
        config: *const Conv2dConfig,
        bytes: *mut usize,
    ) -> i32;
    fn rutorch_acl_conv2d_run_raw(
        config: *const Conv2dConfig,
        input: *const f32,
        weight: *const f32,
        bias: *const f32,
        output: *mut f32,
        temporary: *mut c_void,
        temporary_bytes: usize,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_or_overflowed_gemm_shape() {
        let zero = GemmConfig {
            rows: 0,
            inner: 4,
            columns: 4,
        };
        assert_eq!(gemm_raw_temporary_bytes(&zero), Err(Error::InvalidArgument));

        let valid = GemmConfig {
            rows: 2,
            inner: 3,
            columns: 5,
        };
        let input = [0.0f32; 6];
        let right = [0.0f32; 15];
        let mut output = [0.0f32; 10];
        let mut temporary = [0u8; 1];
        assert_eq!(
            run_gemm_raw(
                &valid,
                &input[..5],
                &right,
                None,
                &mut output,
                &mut temporary
            ),
            Err(Error::InvalidArgument)
        );
        assert_eq!(
            run_gemm_raw(&valid, &input, &right, None, &mut output, &mut temporary),
            Err(Error::Unavailable)
        );
    }

    #[test]
    fn checks_conv_shape_and_buffer_contract_before_ffi() {
        let config = Conv2dConfig {
            input_height: 3,
            input_width: 3,
            input_channels: 1,
            output_channels: 2,
            kernel_height: 3,
            kernel_width: 3,
            output_height: 1,
            output_width: 1,
            strides: [1, 1],
            pads: [0, 0, 0, 0],
            dilations: [1, 1],
        };
        let input = [0.0f32; 9];
        let weight = [0.0f32; 18];
        let mut output = [0.0f32; 2];
        let mut temporary = [0u8; 1];
        assert_eq!(
            run_conv2d_raw(
                &config,
                &input,
                &weight[..17],
                None,
                &mut output,
                &mut temporary,
            ),
            Err(Error::InvalidArgument)
        );
        assert_eq!(
            run_conv2d_raw(&config, &input, &weight, None, &mut output, &mut temporary,),
            Err(Error::Unavailable)
        );
    }
}
