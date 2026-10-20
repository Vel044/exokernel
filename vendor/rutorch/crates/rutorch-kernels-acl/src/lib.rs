//! Arm Compute Library 的安全 Rust 边界。
//!
//! 本 crate 不把 ACL 的 C++ 类型暴露给图执行器。卷积 handle 在加载/首次运行
//! 时复制并预备常量权重，后续推理只复制输入、运行已配置算子并取回输出。

#[cfg(rutorch_acl_available)]
use std::ffi::c_void;

/// 固定 shape 的 FP32 NCHW 卷积配置。
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conv2dConfig {
    /// 输入 channel 数。
    pub input_channels: usize,
    /// 输入高度。
    pub input_height: usize,
    /// 输入宽度。
    pub input_width: usize,
    /// 输出 channel 数。
    pub output_channels: usize,
    /// kernel 高度。
    pub kernel_height: usize,
    /// kernel 宽度。
    pub kernel_width: usize,
    /// 输出高度。
    pub output_height: usize,
    /// 输出宽度。
    pub output_width: usize,
    /// `[y, x]` stride。
    pub strides: [usize; 2],
    /// ONNX `[top, left, bottom, right]` padding。
    pub pads: [usize; 4],
    /// `[y, x]` dilation。
    pub dilations: [usize; 2],
    /// ACL scheduler 线程数。
    pub threads: usize,
}

/// 已配置并持有常量权重的 ACL FP32 卷积。
pub struct Conv2d {
    #[cfg(rutorch_acl_available)]
    raw: *mut c_void,
    #[cfg(rutorch_acl_available)]
    input_len: usize,
    #[cfg(rutorch_acl_available)]
    output_len: usize,
}

/// 固定 shape 的 FP32 row-major GEMM 配置。
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GemmConfig {
    /// 左矩阵行数；批量线性层会在上层先折叠 batch。
    pub rows: usize,
    /// 矩阵乘法的 K 维。
    pub inner: usize,
    /// 右矩阵列数。
    pub columns: usize,
    /// ACL scheduler 线程数。
    pub threads: usize,
}

/// 已配置并持有常量右矩阵的 ACL FP32 GEMM。
pub struct Gemm {
    #[cfg(rutorch_acl_available)]
    raw: *mut c_void,
    #[cfg(rutorch_acl_available)]
    input_len: usize,
    #[cfg(rutorch_acl_available)]
    output_len: usize,
}

// handle 只会通过上层 Mutex 顺序调用；ACL 自身用内部 scheduler 并行。
unsafe impl Send for Gemm {}

impl Gemm {
    /// 创建 GEMM，并复制会在多次推理中复用的右矩阵。
    pub fn new(config: GemmConfig, right: &[f32]) -> Result<Self, String> {
        let input_len = config.rows * config.inner;
        let right_len = config.inner * config.columns;
        let output_len = config.rows * config.columns;
        if right.len() != right_len {
            return Err(format!(
                "ACL GEMM 右矩阵长度不匹配：{} != {right_len}",
                right.len()
            ));
        }
        if !is_available() {
            return Err("当前 binary 未链接 AArch64 Linux Arm Compute Library".into());
        }
        #[cfg(rutorch_acl_available)]
        {
            let raw = unsafe { rutorch_acl_gemm_create(&config, right.as_ptr()) };
            if raw.is_null() {
                return Err(last_error());
            }
            Ok(Self {
                raw,
                input_len,
                output_len,
            })
        }
        #[cfg(not(rutorch_acl_available))]
        {
            let _ = (input_len, output_len);
            Err("当前 binary 未链接 AArch64 Linux Arm Compute Library".into())
        }
    }

    /// 在已配置的 ACL GEMM 上运行一个 row-major 输入。
    pub fn run(&mut self, input: &[f32]) -> Result<Vec<f32>, String> {
        #[cfg(not(rutorch_acl_available))]
        {
            let _ = input;
            Err("当前 binary 未链接 AArch64 Linux Arm Compute Library".into())
        }
        #[cfg(rutorch_acl_available)]
        {
            if input.len() != self.input_len {
                return Err("ACL GEMM 输入长度不匹配".into());
            }
            let mut output = vec![0.0; self.output_len];
            let status =
                unsafe { rutorch_acl_gemm_run(self.raw, input.as_ptr(), output.as_mut_ptr()) };
            if status != 0 {
                return Err(last_error());
            }
            Ok(output)
        }
    }
}

impl Drop for Gemm {
    fn drop(&mut self) {
        #[cfg(rutorch_acl_available)]
        unsafe {
            rutorch_acl_gemm_destroy(self.raw);
        }
    }
}

// handle 只会通过上层 Mutex 顺序调用；ACL 自身用内部 scheduler 并行。
unsafe impl Send for Conv2d {}

impl Conv2d {
    /// 创建卷积并复制 OIHW 权重和可选 bias。
    pub fn new(config: Conv2dConfig, weight: &[f32], bias: Option<&[f32]>) -> Result<Self, String> {
        let weight_len = config.output_channels
            * config.input_channels
            * config.kernel_height
            * config.kernel_width;
        if weight.len() != weight_len {
            return Err(format!(
                "ACL Conv 权重长度不匹配：{} != {weight_len}",
                weight.len()
            ));
        }
        if let Some(values) = bias
            && values.len() != config.output_channels
        {
            return Err("ACL Conv bias 长度不匹配".into());
        }
        if !is_available() {
            return Err("当前 binary 未链接 AArch64 Linux Arm Compute Library".into());
        }
        #[cfg(rutorch_acl_available)]
        {
            let input_len = config.input_channels * config.input_height * config.input_width;
            let output_len = config.output_channels * config.output_height * config.output_width;
            let raw = unsafe {
                rutorch_acl_conv2d_create(
                    &config,
                    weight.as_ptr(),
                    bias.map_or(std::ptr::null(), <[f32]>::as_ptr),
                )
            };
            if raw.is_null() {
                return Err(last_error());
            }
            Ok(Self {
                raw,
                input_len,
                output_len,
            })
        }
        #[cfg(not(rutorch_acl_available))]
        Err("当前 binary 未链接 AArch64 Linux Arm Compute Library".into())
    }

    /// 在已配置的 ACL 算子上运行一个 batch=1 NCHW 输入。
    pub fn run(&mut self, input: &[f32]) -> Result<Vec<f32>, String> {
        #[cfg(not(rutorch_acl_available))]
        {
            let _ = input;
            Err("当前 binary 未链接 AArch64 Linux Arm Compute Library".into())
        }
        #[cfg(rutorch_acl_available)]
        {
            if input.len() != self.input_len {
                return Err("ACL Conv 输入长度不匹配".into());
            }
            let mut output = vec![0.0; self.output_len];
            let status =
                unsafe { rutorch_acl_conv2d_run(self.raw, input.as_ptr(), output.as_mut_ptr()) };
            if status != 0 {
                return Err(last_error());
            }
            Ok(output)
        }
    }
}

impl Drop for Conv2d {
    fn drop(&mut self) {
        #[cfg(rutorch_acl_available)]
        unsafe {
            rutorch_acl_conv2d_destroy(self.raw);
        }
    }
}

/// 当前目标是否实际链接了 ACL。
pub const fn is_available() -> bool {
    cfg!(rutorch_acl_available)
}

/// 加载期设置并验证Linux Full ACL主线程和四个OpenMP worker的FPCR。
pub fn configure_fpcr() -> Result<(), String> {
    #[cfg(rutorch_acl_available)]
    {
        // SAFETY: 此函数无指针参数，在计算开始前同步配置同一OpenMP线程池。
        if unsafe { rutorch_acl_configure_fpcr() } == 0 {
            Ok(())
        } else {
            Err("ACL FPCR校验失败".into())
        }
    }
    #[cfg(not(rutorch_acl_available))]
    {
        Err("Full ACL不可用".into())
    }
}

#[cfg(rutorch_acl_available)]
fn last_error() -> String {
    let pointer = unsafe { rutorch_acl_last_error() };
    if pointer.is_null() {
        return "ACL 未提供错误信息".into();
    }
    unsafe { std::ffi::CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(rutorch_acl_available)]
unsafe extern "C" {
    fn rutorch_acl_configure_fpcr() -> i32;
    fn rutorch_acl_conv2d_create(
        config: *const Conv2dConfig,
        weight: *const f32,
        bias: *const f32,
    ) -> *mut c_void;
    fn rutorch_acl_conv2d_run(handle: *mut c_void, input: *const f32, output: *mut f32) -> i32;
    fn rutorch_acl_conv2d_destroy(handle: *mut c_void);
    fn rutorch_acl_last_error() -> *const std::ffi::c_char;
    fn rutorch_acl_gemm_create(config: *const GemmConfig, right: *const f32) -> *mut c_void;
    fn rutorch_acl_gemm_run(handle: *mut c_void, input: *const f32, output: *mut f32) -> i32;
    fn rutorch_acl_gemm_destroy(handle: *mut c_void);
}
