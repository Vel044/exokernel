//! ACT 固定图的 ACL 句柄计划。
//!
//! 所有描述符均在编译期固定；加载阶段验证 Safetensors shape，并把常量右矩阵
//! 打包到调用者的 persistent arena。稳态推理只按权重地址和整数 shape 查找，
//! 不解析名字、不重新 packing，也不访问全局堆。

use core::marker::PhantomData;
use core::mem::MaybeUninit;

use act_kernels_acl::{self, Conv2dConfig, GemmConfig, Handle};

use crate::Error;
use crate::safetensors::SafeTensors;

const ALIGNMENT: usize = 64;
const GEMM_COUNT: usize = 37;
const CONV_COUNT: usize = 20;

#[derive(Clone, Copy)]
enum WeightShape {
    Matrix,
    Qkv,
    ImageProjection,
}

#[derive(Clone, Copy)]
struct GemmSpec {
    weight: &'static str,
    bias: &'static str,
    rows: usize,
    inner: usize,
    columns: usize,
    part: usize,
    shape: WeightShape,
}

impl GemmSpec {
    const fn matrix(
        weight: &'static str,
        bias: &'static str,
        rows: usize,
        inner: usize,
        columns: usize,
    ) -> Self {
        Self {
            weight,
            bias,
            rows,
            inner,
            columns,
            part: 0,
            shape: WeightShape::Matrix,
        }
    }

    const fn qkv(weight: &'static str, bias: &'static str, rows: usize, part: usize) -> Self {
        Self {
            weight,
            bias,
            rows,
            inner: 512,
            columns: 512,
            part,
            shape: WeightShape::Qkv,
        }
    }

    const fn image_projection() -> Self {
        Self {
            weight: "model.encoder_img_feat_input_proj.weight",
            bias: "model.encoder_img_feat_input_proj.bias",
            rows: 240,
            inner: 512,
            columns: 512,
            part: 0,
            shape: WeightShape::ImageProjection,
        }
    }

    const fn config(self) -> GemmConfig {
        GemmConfig {
            rows: self.rows,
            inner: self.inner,
            columns: self.columns,
        }
    }
}

const GEMM_SPECS: [GemmSpec; GEMM_COUNT] = [
    GemmSpec::matrix(
        "model.encoder_robot_state_input_proj.weight",
        "model.encoder_robot_state_input_proj.bias",
        1,
        6,
        512,
    ),
    GemmSpec::image_projection(),
    GemmSpec::qkv(
        "model.encoder.layers.0.self_attn.in_proj_weight",
        "model.encoder.layers.0.self_attn.in_proj_bias",
        482,
        0,
    ),
    GemmSpec::qkv(
        "model.encoder.layers.0.self_attn.in_proj_weight",
        "model.encoder.layers.0.self_attn.in_proj_bias",
        482,
        1,
    ),
    GemmSpec::qkv(
        "model.encoder.layers.0.self_attn.in_proj_weight",
        "model.encoder.layers.0.self_attn.in_proj_bias",
        482,
        2,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.0.self_attn.out_proj.weight",
        "model.encoder.layers.0.self_attn.out_proj.bias",
        482,
        512,
        512,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.0.linear1.weight",
        "model.encoder.layers.0.linear1.bias",
        482,
        512,
        3200,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.0.linear2.weight",
        "model.encoder.layers.0.linear2.bias",
        482,
        3200,
        512,
    ),
    GemmSpec::qkv(
        "model.encoder.layers.1.self_attn.in_proj_weight",
        "model.encoder.layers.1.self_attn.in_proj_bias",
        482,
        0,
    ),
    GemmSpec::qkv(
        "model.encoder.layers.1.self_attn.in_proj_weight",
        "model.encoder.layers.1.self_attn.in_proj_bias",
        482,
        1,
    ),
    GemmSpec::qkv(
        "model.encoder.layers.1.self_attn.in_proj_weight",
        "model.encoder.layers.1.self_attn.in_proj_bias",
        482,
        2,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.1.self_attn.out_proj.weight",
        "model.encoder.layers.1.self_attn.out_proj.bias",
        482,
        512,
        512,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.1.linear1.weight",
        "model.encoder.layers.1.linear1.bias",
        482,
        512,
        3200,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.1.linear2.weight",
        "model.encoder.layers.1.linear2.bias",
        482,
        3200,
        512,
    ),
    GemmSpec::qkv(
        "model.encoder.layers.2.self_attn.in_proj_weight",
        "model.encoder.layers.2.self_attn.in_proj_bias",
        482,
        0,
    ),
    GemmSpec::qkv(
        "model.encoder.layers.2.self_attn.in_proj_weight",
        "model.encoder.layers.2.self_attn.in_proj_bias",
        482,
        1,
    ),
    GemmSpec::qkv(
        "model.encoder.layers.2.self_attn.in_proj_weight",
        "model.encoder.layers.2.self_attn.in_proj_bias",
        482,
        2,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.2.self_attn.out_proj.weight",
        "model.encoder.layers.2.self_attn.out_proj.bias",
        482,
        512,
        512,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.2.linear1.weight",
        "model.encoder.layers.2.linear1.bias",
        482,
        512,
        3200,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.2.linear2.weight",
        "model.encoder.layers.2.linear2.bias",
        482,
        3200,
        512,
    ),
    GemmSpec::qkv(
        "model.encoder.layers.3.self_attn.in_proj_weight",
        "model.encoder.layers.3.self_attn.in_proj_bias",
        482,
        0,
    ),
    GemmSpec::qkv(
        "model.encoder.layers.3.self_attn.in_proj_weight",
        "model.encoder.layers.3.self_attn.in_proj_bias",
        482,
        1,
    ),
    GemmSpec::qkv(
        "model.encoder.layers.3.self_attn.in_proj_weight",
        "model.encoder.layers.3.self_attn.in_proj_bias",
        482,
        2,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.3.self_attn.out_proj.weight",
        "model.encoder.layers.3.self_attn.out_proj.bias",
        482,
        512,
        512,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.3.linear1.weight",
        "model.encoder.layers.3.linear1.bias",
        482,
        512,
        3200,
    ),
    GemmSpec::matrix(
        "model.encoder.layers.3.linear2.weight",
        "model.encoder.layers.3.linear2.bias",
        482,
        3200,
        512,
    ),
    GemmSpec::qkv(
        "model.decoder.layers.0.self_attn.in_proj_weight",
        "model.decoder.layers.0.self_attn.in_proj_bias",
        100,
        0,
    ),
    GemmSpec::qkv(
        "model.decoder.layers.0.self_attn.in_proj_weight",
        "model.decoder.layers.0.self_attn.in_proj_bias",
        100,
        1,
    ),
    GemmSpec::qkv(
        "model.decoder.layers.0.self_attn.in_proj_weight",
        "model.decoder.layers.0.self_attn.in_proj_bias",
        100,
        2,
    ),
    GemmSpec::matrix(
        "model.decoder.layers.0.self_attn.out_proj.weight",
        "model.decoder.layers.0.self_attn.out_proj.bias",
        100,
        512,
        512,
    ),
    GemmSpec::qkv(
        "model.decoder.layers.0.multihead_attn.in_proj_weight",
        "model.decoder.layers.0.multihead_attn.in_proj_bias",
        100,
        0,
    ),
    GemmSpec::qkv(
        "model.decoder.layers.0.multihead_attn.in_proj_weight",
        "model.decoder.layers.0.multihead_attn.in_proj_bias",
        482,
        1,
    ),
    GemmSpec::qkv(
        "model.decoder.layers.0.multihead_attn.in_proj_weight",
        "model.decoder.layers.0.multihead_attn.in_proj_bias",
        482,
        2,
    ),
    GemmSpec::matrix(
        "model.decoder.layers.0.multihead_attn.out_proj.weight",
        "model.decoder.layers.0.multihead_attn.out_proj.bias",
        100,
        512,
        512,
    ),
    GemmSpec::matrix(
        "model.decoder.layers.0.linear1.weight",
        "model.decoder.layers.0.linear1.bias",
        100,
        512,
        3200,
    ),
    GemmSpec::matrix(
        "model.decoder.layers.0.linear2.weight",
        "model.decoder.layers.0.linear2.bias",
        100,
        3200,
        512,
    ),
    GemmSpec::matrix(
        "model.action_head.weight",
        "model.action_head.bias",
        100,
        512,
        6,
    ),
];

#[derive(Clone, Copy)]
struct ConvSpec {
    weight: &'static str,
    input_height: usize,
    input_width: usize,
    input_channels: usize,
    output_channels: usize,
    kernel: usize,
    stride: usize,
    padding: usize,
}

impl ConvSpec {
    const fn config(self) -> Conv2dConfig {
        let output_height = (self.input_height + 2 * self.padding - self.kernel) / self.stride + 1;
        let output_width = (self.input_width + 2 * self.padding - self.kernel) / self.stride + 1;
        Conv2dConfig {
            input_height: self.input_height,
            input_width: self.input_width,
            input_channels: self.input_channels,
            output_channels: self.output_channels,
            kernel_height: self.kernel,
            kernel_width: self.kernel,
            output_height,
            output_width,
            strides: [self.stride, self.stride],
            pads: [self.padding, self.padding, self.padding, self.padding],
            dilations: [1, 1],
        }
    }
}

const CONV_SPECS: [ConvSpec; CONV_COUNT] = [
    ConvSpec {
        weight: "model.backbone.conv1.weight",
        input_height: 360,
        input_width: 640,
        input_channels: 3,
        output_channels: 64,
        kernel: 7,
        stride: 2,
        padding: 3,
    },
    ConvSpec {
        weight: "model.backbone.layer1.0.conv1.weight",
        input_height: 90,
        input_width: 160,
        input_channels: 64,
        output_channels: 64,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer1.0.conv2.weight",
        input_height: 90,
        input_width: 160,
        input_channels: 64,
        output_channels: 64,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer1.1.conv1.weight",
        input_height: 90,
        input_width: 160,
        input_channels: 64,
        output_channels: 64,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer1.1.conv2.weight",
        input_height: 90,
        input_width: 160,
        input_channels: 64,
        output_channels: 64,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer2.0.conv1.weight",
        input_height: 90,
        input_width: 160,
        input_channels: 64,
        output_channels: 128,
        kernel: 3,
        stride: 2,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer2.0.conv2.weight",
        input_height: 45,
        input_width: 80,
        input_channels: 128,
        output_channels: 128,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer2.0.downsample.0.weight",
        input_height: 90,
        input_width: 160,
        input_channels: 64,
        output_channels: 128,
        kernel: 1,
        stride: 2,
        padding: 0,
    },
    ConvSpec {
        weight: "model.backbone.layer2.1.conv1.weight",
        input_height: 45,
        input_width: 80,
        input_channels: 128,
        output_channels: 128,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer2.1.conv2.weight",
        input_height: 45,
        input_width: 80,
        input_channels: 128,
        output_channels: 128,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer3.0.conv1.weight",
        input_height: 45,
        input_width: 80,
        input_channels: 128,
        output_channels: 256,
        kernel: 3,
        stride: 2,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer3.0.conv2.weight",
        input_height: 23,
        input_width: 40,
        input_channels: 256,
        output_channels: 256,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer3.0.downsample.0.weight",
        input_height: 45,
        input_width: 80,
        input_channels: 128,
        output_channels: 256,
        kernel: 1,
        stride: 2,
        padding: 0,
    },
    ConvSpec {
        weight: "model.backbone.layer3.1.conv1.weight",
        input_height: 23,
        input_width: 40,
        input_channels: 256,
        output_channels: 256,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer3.1.conv2.weight",
        input_height: 23,
        input_width: 40,
        input_channels: 256,
        output_channels: 256,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer4.0.conv1.weight",
        input_height: 23,
        input_width: 40,
        input_channels: 256,
        output_channels: 512,
        kernel: 3,
        stride: 2,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer4.0.conv2.weight",
        input_height: 12,
        input_width: 20,
        input_channels: 512,
        output_channels: 512,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer4.0.downsample.0.weight",
        input_height: 23,
        input_width: 40,
        input_channels: 256,
        output_channels: 512,
        kernel: 1,
        stride: 2,
        padding: 0,
    },
    ConvSpec {
        weight: "model.backbone.layer4.1.conv1.weight",
        input_height: 12,
        input_width: 20,
        input_channels: 512,
        output_channels: 512,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
    ConvSpec {
        weight: "model.backbone.layer4.1.conv2.weight",
        input_height: 12,
        input_width: 20,
        input_channels: 512,
        output_channels: 512,
        kernel: 3,
        stride: 1,
        padding: 1,
    },
];

struct PreparedGemm {
    weight_address: usize,
    config: GemmConfig,
    handle: Handle,
}

struct PreparedConv {
    weight_address: usize,
    config: Conv2dConfig,
    handle: Handle,
}

/// 固定数量的 ACT 预备句柄；数组本身位于 ActModel，权重数据位于 persistent arena。
pub struct PreparedKernels<'a> {
    gemm: [Option<PreparedGemm>; GEMM_COUNT],
    conv: [Option<PreparedConv>; CONV_COUNT],
    temporary_bytes: usize,
    persistent_bytes: usize,
    _lifetime: PhantomData<&'a mut [MaybeUninit<u8>]>,
}

impl<'a> PreparedKernels<'a> {
    pub fn empty() -> Self {
        Self {
            gemm: core::array::from_fn(|_| None),
            conv: core::array::from_fn(|_| None),
            temporary_bytes: 0,
            persistent_bytes: 0,
            _lifetime: PhantomData,
        }
    }

    pub fn prepare(
        weights: &SafeTensors<'a>,
        persistent: &'a mut [MaybeUninit<u8>],
    ) -> Result<Self, Error> {
        let required = required_persistent_bytes(weights)?;
        if persistent.len() < required {
            return Err(Error::PersistentWorkspaceTooSmall);
        }
        let mut output = Self::empty();
        output.persistent_bytes = required;
        let mut arena = PersistentArena::new(persistent)?;

        for (index, spec) in GEMM_SPECS.into_iter().enumerate() {
            let (weight, bias) = gemm_parameters(weights, spec)?;
            let config = spec.config();
            let bytes =
                act_kernels_acl::gemm_persistent_bytes(&config).map_err(map_provider_error)?;
            let storage = arena.allocate(bytes)?;
            let handle = act_kernels_acl::prepare_gemm(&config, weight, Some(bias), storage)
                .map_err(map_provider_error)?;
            output.temporary_bytes = output.temporary_bytes.max(handle.temporary_bytes());
            output.gemm[index] = Some(PreparedGemm {
                weight_address: weight.as_ptr() as usize,
                config,
                handle,
            });
        }

        for (index, spec) in CONV_SPECS.into_iter().enumerate() {
            let config = spec.config();
            let weight = tensor_with_shape(
                weights,
                spec.weight,
                &[
                    spec.output_channels,
                    spec.input_channels,
                    spec.kernel,
                    spec.kernel,
                ],
            )?;
            let bytes =
                act_kernels_acl::conv2d_persistent_bytes(&config).map_err(map_provider_error)?;
            let storage = arena.allocate(bytes)?;
            let handle = act_kernels_acl::prepare_conv2d(&config, weight, None, storage)
                .map_err(map_provider_error)?;
            output.temporary_bytes = output.temporary_bytes.max(handle.temporary_bytes());
            output.conv[index] = Some(PreparedConv {
                weight_address: weight.as_ptr() as usize,
                config,
                handle,
            });
        }
        Ok(output)
    }

    pub fn gemm(
        &self,
        weight: &[f32],
        rows: usize,
        inner: usize,
        columns: usize,
    ) -> Result<&Handle, Error> {
        self.gemm
            .iter()
            .flatten()
            .find(|entry| {
                entry.weight_address == weight.as_ptr() as usize
                    && entry.config.rows == rows
                    && entry.config.inner == inner
                    && entry.config.columns == columns
            })
            .map(|entry| &entry.handle)
            .ok_or(Error::ShapeMismatch)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn conv(
        &self,
        weight: &[f32],
        input_height: usize,
        input_width: usize,
        input_channels: usize,
        output_channels: usize,
        kernel: usize,
        stride: usize,
        padding: usize,
    ) -> Result<&Handle, Error> {
        self.conv
            .iter()
            .flatten()
            .find(|entry| {
                let config = entry.config;
                entry.weight_address == weight.as_ptr() as usize
                    && config.input_height == input_height
                    && config.input_width == input_width
                    && config.input_channels == input_channels
                    && config.output_channels == output_channels
                    && config.kernel_height == kernel
                    && config.strides == [stride, stride]
                    && config.pads == [padding, padding, padding, padding]
            })
            .map(|entry| &entry.handle)
            .ok_or(Error::ShapeMismatch)
    }

    pub const fn temporary_bytes(&self) -> usize {
        self.temporary_bytes
    }

    pub const fn persistent_bytes(&self) -> usize {
        self.persistent_bytes
    }
}

pub fn required_persistent_bytes(weights: &SafeTensors<'_>) -> Result<usize, Error> {
    let mut total = ALIGNMENT - 1;
    for spec in GEMM_SPECS {
        let _ = gemm_parameters(weights, spec)?;
        let bytes =
            act_kernels_acl::gemm_persistent_bytes(&spec.config()).map_err(map_provider_error)?;
        total = total
            .checked_add(align_up(bytes, ALIGNMENT)?)
            .ok_or(Error::PersistentWorkspaceTooSmall)?;
    }
    for spec in CONV_SPECS {
        let _ = tensor_with_shape(
            weights,
            spec.weight,
            &[
                spec.output_channels,
                spec.input_channels,
                spec.kernel,
                spec.kernel,
            ],
        )?;
        let bytes =
            act_kernels_acl::conv2d_persistent_bytes(&spec.config()).map_err(map_provider_error)?;
        total = total
            .checked_add(align_up(bytes, ALIGNMENT)?)
            .ok_or(Error::PersistentWorkspaceTooSmall)?;
    }
    Ok(total)
}

pub fn required_temporary_bytes() -> Result<usize, Error> {
    let mut maximum = 0usize;
    for spec in GEMM_SPECS {
        maximum = maximum.max(
            act_kernels_acl::gemm_temporary_bytes(&spec.config()).map_err(map_provider_error)?,
        );
    }
    for spec in CONV_SPECS {
        maximum = maximum.max(
            act_kernels_acl::conv2d_temporary_bytes(&spec.config()).map_err(map_provider_error)?,
        );
    }
    Ok(maximum)
}

fn gemm_parameters<'a>(
    weights: &SafeTensors<'a>,
    spec: GemmSpec,
) -> Result<(&'a [f32], &'a [f32]), Error> {
    let (full_weight_shape, full_bias_shape, weight_offset, bias_offset) = match spec.shape {
        WeightShape::Matrix => (
            [spec.columns, spec.inner, 1, 1],
            [spec.columns, 1, 1, 1],
            0,
            0,
        ),
        WeightShape::Qkv => (
            [3 * spec.columns, spec.inner, 1, 1],
            [3 * spec.columns, 1, 1, 1],
            spec.part * spec.columns * spec.inner,
            spec.part * spec.columns,
        ),
        WeightShape::ImageProjection => (
            [spec.columns, spec.inner, 1, 1],
            [spec.columns, 1, 1, 1],
            0,
            0,
        ),
    };
    let weight_rank = if matches!(spec.shape, WeightShape::ImageProjection) {
        4
    } else {
        2
    };
    let full_weight = tensor_with_shape(weights, spec.weight, &full_weight_shape[..weight_rank])?;
    let full_bias = tensor_with_shape(weights, spec.bias, &full_bias_shape[..1])?;
    let weight_len = spec.columns * spec.inner;
    Ok((
        &full_weight[weight_offset..weight_offset + weight_len],
        &full_bias[bias_offset..bias_offset + spec.columns],
    ))
}

fn tensor_with_shape<'a>(
    weights: &SafeTensors<'a>,
    name: &str,
    shape: &[usize],
) -> Result<&'a [f32], Error> {
    let tensor = weights.tensor(name)?;
    if tensor.shape() != shape {
        return Err(Error::ShapeMismatch);
    }
    tensor.f32_data()
}

struct PersistentArena {
    base: *mut u8,
    len: usize,
    offset: usize,
}

impl PersistentArena {
    fn new(storage: &mut [MaybeUninit<u8>]) -> Result<Self, Error> {
        let address = storage.as_mut_ptr() as usize;
        let aligned = align_up(address, ALIGNMENT)?;
        let prefix = aligned - address;
        if prefix > storage.len() {
            return Err(Error::PersistentWorkspaceTooSmall);
        }
        Ok(Self {
            base: aligned as *mut u8,
            len: storage.len() - prefix,
            offset: 0,
        })
    }

    fn allocate(&mut self, bytes: usize) -> Result<&mut [u8], Error> {
        let length = align_up(bytes, ALIGNMENT)?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or(Error::PersistentWorkspaceTooSmall)?;
        if end > self.len {
            return Err(Error::PersistentWorkspaceTooSmall);
        }
        // SAFETY:base 已对齐且指向调用者独占的 persistent slice；每次分配的
        // 区间不重叠。Handle 只保存当前区间地址，调用者保证其寿命覆盖模型。
        let result = unsafe { core::slice::from_raw_parts_mut(self.base.add(self.offset), bytes) };
        self.offset = end;
        Ok(result)
    }
}

fn align_up(value: usize, alignment: usize) -> Result<usize, Error> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(Error::PersistentWorkspaceTooSmall)
}

fn map_provider_error(error: act_kernels_acl::Error) -> Error {
    match error {
        act_kernels_acl::Error::Unavailable => Error::AclProviderUnavailable,
        act_kernels_acl::Error::WorkspaceTooSmall => Error::PersistentWorkspaceTooSmall,
        act_kernels_acl::Error::InvalidArgument | act_kernels_acl::Error::ExecutionFailed => {
            Error::ShapeMismatch
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_has_unique_runtime_keys() {
        for (index, left) in GEMM_SPECS.iter().enumerate() {
            for right in &GEMM_SPECS[index + 1..] {
                assert!(
                    left.weight != right.weight
                        || left.rows != right.rows
                        || left.part != right.part,
                    "重复 GEMM key: {}",
                    left.weight
                );
            }
        }
        assert_eq!(GEMM_SPECS.len(), GEMM_COUNT);
        assert_eq!(CONV_SPECS.len(), CONV_COUNT);
    }
}
