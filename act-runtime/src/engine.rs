//! 固定结构ACT推理引擎。
//!
//! 网络结构严格对应目标仓库的`config.json`：共享ResNet18、512维token、
//! 8头注意力、4层Encoder、1层Decoder和100步动作头。训练专用VAE
//! Encoder不参与推理；latent固定为零，与LeRobot `eval()`路径一致。

#![allow(clippy::approx_constant)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::manual_div_ceil)]
#![allow(clippy::needless_range_loop)]

use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::safetensors::SafeTensors;
use crate::{
    validate_observation, Error, KernelProvider, LoadOptions, Normalization, Observation,
    ACTION_DIM, ACTION_STEPS, IMAGE_CHANNELS, IMAGE_HEIGHT, IMAGE_WIDTH, STATE_DIM,
    WORKSPACE_FLOATS,
};

#[cfg(feature = "acl-neon")]
use act_kernels_acl::{self, Conv2dConfig, GemmConfig};

const MODEL_DIM: usize = 512;
const HEADS: usize = 8;
const HEAD_DIM: usize = MODEL_DIM / HEADS;
const FF_DIM: usize = 3200;
const FEATURE_HEIGHT: usize = 12;
const FEATURE_WIDTH: usize = 20;
const IMAGE_TOKENS: usize = FEATURE_HEIGHT * FEATURE_WIDTH;
const ENCODER_TOKENS: usize = 2 + 2 * IMAGE_TOKENS;
const LAYER_NORM_EPS: f32 = 1.0e-5;

/// 对一个互不重叠的输出行区间执行计算的算子微内核。
///
/// `context`只在发起调用的栈帧存活期间有效。并行执行器必须等全部worker
/// 返回后才能从回调返回，不能保存这个指针供以后异步使用。
pub type RowKernel = unsafe fn(context: *mut (), start_row: usize, end_row: usize);

/// 平台提供的同步行并行回调。act-runtime保持`no_std`且不绑定某种线程API；
/// host测试默认不安装，libOS把它连接到Kernel线程和Notification原语。
pub type ParallelRowsCallback = unsafe fn(context: *mut (), rows: usize, kernel: RowKernel);

/// ACL scheduler bridge 使用的单 job 内核。
pub type JobKernel = unsafe extern "C" fn(context: *mut (), job: usize);

/// 平台提供的同步 job 并行回调；返回前必须完成全部 job。
pub type ParallelJobsCallback =
    unsafe extern "C" fn(context: *mut (), jobs: usize, kernel: JobKernel);

static PARALLEL_ROWS_CALLBACK: AtomicUsize = AtomicUsize::new(0);
static PARALLEL_JOBS_CALLBACK: AtomicUsize = AtomicUsize::new(0);

/// 安装或移除平台行并行执行器。
///
/// # Safety
/// 回调必须在进程生命周期内有效、同步等待全部worker完成，并且同一时刻只能
/// 有一个ACT前向传播使用该全局执行器。
pub unsafe fn install_parallel_rows(callback: Option<ParallelRowsCallback>) {
    PARALLEL_ROWS_CALLBACK.store(
        callback.map_or(0, |function| function as usize),
        Ordering::Release,
    );
}

/// 安装或移除 ACL/其他 C++ provider 的 job 调度器。
///
/// # Safety
/// `callback` 必须在后续所有调度调用结束前保持有效；回调必须同步等待
/// 全部 worker 完成，并且不得在同一时刻并发覆盖另一组全局调度状态。
pub unsafe fn install_parallel_jobs(callback: Option<ParallelJobsCallback>) {
    PARALLEL_JOBS_CALLBACK.store(
        callback.map_or(0, |function| function as usize),
        Ordering::Release,
    );
}

/// C++ ACL scheduler 调用的同步 job 入口。
#[no_mangle]
pub unsafe extern "C" fn rutorch_parallel_jobs(context: *mut (), jobs: usize, kernel: JobKernel) {
    let address = PARALLEL_JOBS_CALLBACK.load(Ordering::Acquire);
    if address == 0 || jobs < 2 {
        for job in 0..jobs {
            // SAFETY:调用者保证kernel和context在同步调用期间有效。
            kernel(context, job);
        }
    } else {
        // SAFETY:install_parallel_jobs只接受相同ABI的有效函数地址。
        let callback: ParallelJobsCallback = core::mem::transmute(address);
        callback(context, jobs, kernel);
    }
}

#[inline]
unsafe fn run_rows(context: *mut (), rows: usize, kernel: RowKernel) {
    let address = PARALLEL_ROWS_CALLBACK.load(Ordering::Acquire);
    if address == 0 || rows < 4 {
        kernel(context, 0, rows);
    } else {
        // SAFETY:安装接口只接受相同函数签名，并要求函数地址保持有效。
        let callback: ParallelRowsCallback = core::mem::transmute(address);
        callback(context, rows, kernel);
    }
}

const BACKBONE_BLOCKS: [BlockSpec; 8] = [
    BlockSpec::new("layer1.0", 64, 64, 1, false),
    BlockSpec::new("layer1.1", 64, 64, 1, false),
    BlockSpec::new("layer2.0", 64, 128, 2, true),
    BlockSpec::new("layer2.1", 128, 128, 1, false),
    BlockSpec::new("layer3.0", 128, 256, 2, true),
    BlockSpec::new("layer3.1", 256, 256, 1, false),
    BlockSpec::new("layer4.0", 256, 512, 2, true),
    BlockSpec::new("layer4.1", 512, 512, 1, false),
];

const ENCODER_LAYERS: [&str; 4] = [
    "model.encoder.layers.0",
    "model.encoder.layers.1",
    "model.encoder.layers.2",
    "model.encoder.layers.3",
];

/// 一次ACT前向传播中已经完成的阶段。
///
/// 运行时本身仍然不依赖日志系统；调用者可以选择忽略这些事件，或把它们
/// 转换成串口日志、性能计数器和监控数据。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InferenceStage {
    InputPrepared,
    CameraNormalized { camera: u8 },
    CameraStem { camera: u8 },
    CameraBlock { camera: u8, block: u8 },
    CameraProjected { camera: u8 },
    EncoderLayer { layer: u8 },
    Decoder,
    ActionHead,
}

/// 借用只读权重和归一化参数的ACT实例。
pub struct ActModel<'a> {
    weights: SafeTensors<'a>,
    normalization: Normalization,
    provider: KernelProvider,
    threads: usize,
    // 将调用者的 persistent arena 借用绑定到模型生命周期；portable provider
    // 不使用它，但保留统一接口，避免后续 ACL 句柄悬空。
    _persistent: PhantomData<&'a mut [MaybeUninit<u8>]>,
}

impl<'a> ActModel<'a> {
    pub fn load(model: &'a [u8], stats: &[u8]) -> Result<Self, Error> {
        let weights = SafeTensors::parse(model)?;
        let normalization = Normalization::from_safetensors(stats)?;
        let instance = Self {
            weights,
            normalization,
            provider: KernelProvider::PortableNeon,
            threads: 1,
            _persistent: PhantomData,
        };
        instance.validate_signature()?;
        Ok(instance)
    }

    /// 查询 provider 在模型加载阶段需要的持久化 workspace。
    pub fn required_persistent_bytes(
        model: &[u8],
        stats: &[u8],
        options: LoadOptions,
    ) -> Result<usize, Error> {
        if options.provider == KernelProvider::PortableNeon {
            let _ = (model, stats);
            Ok(0)
        } else {
            #[cfg(feature = "acl-neon")]
            {
                let _ = (model, stats, options);
                // 当前 ACL 整图接线采用一次性 panel 打包；预打包句柄 API
                // 仍可由调用者显式使用，后续加载阶段再切换为固定缓存。
                Ok(0)
            }
            #[cfg(not(feature = "acl-neon"))]
            {
                let _ = (model, stats, options);
                Err(Error::AclProviderUnavailable)
            }
        }
    }

    /// 使用显式 provider 和调用者提供的 persistent arena 加载模型。
    pub fn load_with_options(
        model: &'a [u8],
        stats: &'a [u8],
        _persistent: &'a mut [MaybeUninit<u8>],
        options: LoadOptions,
    ) -> Result<Self, Error> {
        if options.provider == KernelProvider::AclNeon {
            #[cfg(not(feature = "acl-neon"))]
            {
                let _ = (model, stats, _persistent, options);
                return Err(Error::AclProviderUnavailable);
            }
        }
        let mut instance = Self::load(model, stats)?;
        instance.provider = options.provider;
        instance.threads = options.threads.max(1);
        instance._persistent = PhantomData;
        Ok(instance)
    }

    /// 返回加载时冻结的 provider。
    pub fn kernel_provider(&self) -> KernelProvider {
        self.provider
    }

    /// 返回加载时冻结的逻辑线程数。
    pub fn kernel_threads(&self) -> usize {
        self.threads
    }

    pub fn normalization(&self) -> &Normalization {
        &self.normalization
    }

    /// 执行一次双相机ACT推理并输出完整100步动作块。
    pub fn predict(
        &self,
        observation: &Observation<'_>,
        workspace: &mut [f32],
        actions: &mut [[f32; ACTION_DIM]; ACTION_STEPS],
    ) -> Result<(), Error> {
        self.predict_with_progress(observation, workspace, actions, |_| {})
    }

    /// 执行推理，并在每个主要计算阶段完成后调用一次`progress`。
    ///
    /// 回调发生在纯CPU计算边界，不会改变权重、工作区或数值计算顺序。
    pub fn predict_with_progress<F>(
        &self,
        observation: &Observation<'_>,
        workspace: &mut [f32],
        actions: &mut [[f32; ACTION_DIM]; ACTION_STEPS],
        mut progress: F,
    ) -> Result<(), Error>
    where
        F: FnMut(InferenceStage),
    {
        validate_observation(observation)?;
        if workspace.len() < WORKSPACE_FLOATS {
            return Err(Error::WorkspaceTooSmall);
        }

        let mut arena = Arena::new(workspace);
        let encoder = arena.alloc(ENCODER_TOKENS * MODEL_DIM)?;
        let encoder_pos = arena.alloc(ENCODER_TOKENS * MODEL_DIM)?;
        let decoder = arena.alloc(ACTION_STEPS * MODEL_DIM)?;
        let decoder_pos = arena.alloc(ACTION_STEPS * MODEL_DIM)?;

        self.prepare_scalar_tokens(&mut arena, encoder, encoder_pos, observation.state)?;
        copy_weight(
            &self.weights,
            "model.decoder_pos_embed.weight",
            &[ACTION_STEPS, MODEL_DIM],
            &mut arena,
            decoder_pos,
        )?;
        arena.fill(decoder, 0.0);
        progress(InferenceStage::InputPrepared);

        let scratch_mark = arena.mark();
        self.encode_camera(
            &mut arena,
            observation.handeye_rgb,
            &self.normalization.handeye_mean,
            &self.normalization.handeye_std,
            encoder,
            encoder_pos,
            2,
            0,
            &mut progress,
        )?;
        arena.reset(scratch_mark);
        self.encode_camera(
            &mut arena,
            observation.fixed_rgb,
            &self.normalization.fixed_mean,
            &self.normalization.fixed_std,
            encoder,
            encoder_pos,
            2 + IMAGE_TOKENS,
            1,
            &mut progress,
        )?;
        arena.reset(scratch_mark);

        for (layer, prefix) in ENCODER_LAYERS.into_iter().enumerate() {
            self.encoder_layer(&mut arena, encoder, encoder_pos, prefix, scratch_mark)?;
            progress(InferenceStage::EncoderLayer { layer: layer as u8 });
        }
        self.decoder_layer(
            &mut arena,
            decoder,
            decoder_pos,
            encoder,
            encoder_pos,
            scratch_mark,
        )?;
        progress(InferenceStage::Decoder);

        let norm_weight = self.weight("model.decoder.norm.weight", &[MODEL_DIM])?;
        let norm_bias = self.weight("model.decoder.norm.bias", &[MODEL_DIM])?;
        layer_norm_in_place(&mut arena, decoder, ACTION_STEPS, norm_weight, norm_bias);

        let head_weight = self.weight("model.action_head.weight", &[ACTION_DIM, MODEL_DIM])?;
        let head_bias = self.weight("model.action_head.bias", &[ACTION_DIM])?;
        let mut normalized = [[0.0f32; ACTION_DIM]; ACTION_STEPS];
        let decoder_ptr = arena.ptr(decoder);
        self.linear(
            &mut arena,
            decoder_ptr,
            ACTION_STEPS,
            MODEL_DIM,
            head_weight,
            head_bias,
            normalized.as_mut_ptr().cast::<f32>(),
            ACTION_DIM,
        )?;
        for step in 0..ACTION_STEPS {
            actions[step] = self.normalization.denormalize_action(normalized[step]);
        }
        progress(InferenceStage::ActionHead);
        Ok(())
    }

    /// 根据加载时冻结的 provider 执行线性层。ACL 路径使用调用者 arena
    /// 作为一次性打包 workspace；portable 路径保留现有四行 NEON 实现。
    #[allow(clippy::too_many_arguments)]
    fn linear(
        &self,
        arena: &mut Arena<'_>,
        input: *const f32,
        rows: usize,
        input_dim: usize,
        weight: &[f32],
        bias: &[f32],
        output: *mut f32,
        output_dim: usize,
    ) -> Result<(), Error> {
        // ACL 的窄边界只覆盖足够大的矩阵。小矩阵中一次性 panel 打包的固定
        // 成本高于现有四行 NEON；保留 portable 路径是显式的调度选择，不是
        // provider 失败后的静默回退。Conv 仍始终走 ACL 分支。
        let use_acl_gemm = self.provider == KernelProvider::AclNeon
            && rows >= 128
            && (input_dim >= 1024 || output_dim >= 1024);
        if use_acl_gemm {
            #[cfg(feature = "acl-neon")]
            {
                let config = GemmConfig {
                    rows,
                    inner: input_dim,
                    columns: output_dim,
                };
                let bytes = act_kernels_acl::gemm_raw_temporary_bytes(&config)
                    .map_err(|_| Error::AclProviderUnavailable)?;
                let mark = arena.mark();
                let words = bytes.div_ceil(core::mem::size_of::<f32>());
                let temporary = match arena.alloc(words) {
                    Ok(buffer) => buffer,
                    Err(error) => {
                        arena.reset(mark);
                        return Err(error);
                    }
                };
                let temporary_ptr = arena.mut_ptr(temporary).cast::<u8>();
                // SAFETY: shape checks above保证输入/权重/输出覆盖完整矩阵；
                // temporary来自调用者workspace，ACL同步返回后立即复位。
                let result = unsafe {
                    act_kernels_acl::run_gemm_raw(
                        &config,
                        core::slice::from_raw_parts(input, rows * input_dim),
                        weight,
                        Some(bias),
                        core::slice::from_raw_parts_mut(output, rows * output_dim),
                        core::slice::from_raw_parts_mut(temporary_ptr, bytes),
                    )
                };
                arena.reset(mark);
                return result.map_err(|_| Error::AclProviderUnavailable);
            }
            #[cfg(not(feature = "acl-neon"))]
            {
                let _ = (
                    arena, input, rows, input_dim, weight, bias, output, output_dim,
                );
                return Err(Error::AclProviderUnavailable);
            }
        }
        // SAFETY: 调用者已按目标 shape 提供不重叠输出，portable 内核只访问这些范围。
        unsafe { linear_raw(input, rows, input_dim, weight, bias, output, output_dim) };
        Ok(())
    }

    fn validate_signature(&self) -> Result<(), Error> {
        self.weight("model.backbone.conv1.weight", &[64, 3, 7, 7])?;
        self.weight(
            "model.encoder.layers.3.linear1.weight",
            &[FF_DIM, MODEL_DIM],
        )?;
        self.weight(
            "model.decoder.layers.0.multihead_attn.in_proj_weight",
            &[3 * MODEL_DIM, MODEL_DIM],
        )?;
        self.weight("model.action_head.weight", &[ACTION_DIM, MODEL_DIM])?;
        Ok(())
    }

    fn weight(&self, name: &str, shape: &[usize]) -> Result<&'a [f32], Error> {
        let tensor = self.weights.tensor(name)?;
        if tensor.shape() != shape {
            return Err(Error::ShapeMismatch);
        }
        tensor.f32_data()
    }

    fn prepare_scalar_tokens(
        &self,
        arena: &mut Arena<'_>,
        encoder: Buffer,
        encoder_pos: Buffer,
        state: [f32; STATE_DIM],
    ) -> Result<(), Error> {
        let latent_bias = self.weight("model.encoder_latent_input_proj.bias", &[MODEL_DIM])?;
        arena.copy_into(encoder.sub(0, MODEL_DIM), latent_bias);

        let normalized_state = self.normalization.normalize_state(state);
        let state_weight = self.weight(
            "model.encoder_robot_state_input_proj.weight",
            &[MODEL_DIM, STATE_DIM],
        )?;
        let state_bias = self.weight("model.encoder_robot_state_input_proj.bias", &[MODEL_DIM])?;
        let state_output = arena.mut_ptr(encoder.sub(MODEL_DIM, MODEL_DIM));
        self.linear(
            arena,
            normalized_state.as_ptr(),
            1,
            STATE_DIM,
            state_weight,
            state_bias,
            state_output,
            MODEL_DIM,
        )?;

        let scalar_pos =
            self.weight("model.encoder_1d_feature_pos_embed.weight", &[2, MODEL_DIM])?;
        arena.copy_into(encoder_pos.sub(0, 2 * MODEL_DIM), scalar_pos);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_camera<F>(
        &self,
        arena: &mut Arena<'_>,
        rgb: &[u8],
        mean: &[f32; IMAGE_CHANNELS],
        std: &[f32; IMAGE_CHANNELS],
        encoder: Buffer,
        encoder_pos: Buffer,
        token_offset: usize,
        camera: u8,
        progress: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(InferenceStage),
    {
        let normalized = arena.alloc(IMAGE_HEIGHT * IMAGE_WIDTH * IMAGE_CHANNELS)?;
        normalize_rgb_hwc(arena, normalized, rgb, mean, std);
        progress(InferenceStage::CameraNormalized { camera });

        let conv1 = arena.alloc(180 * 320 * 64)?;
        let feature_a = arena.alloc(90 * 160 * 64)?;
        let feature_b = arena.alloc(90 * 160 * 64)?;
        let feature_c = arena.alloc(90 * 160 * 64)?;
        let im2col = arena.alloc(180 * 320 * 3 * 7 * 7)?;

        let conv1_weight = self.weight("model.backbone.conv1.weight", &[64, 3, 7, 7])?;
        conv2d(
            arena,
            normalized,
            IMAGE_HEIGHT,
            IMAGE_WIDTH,
            3,
            conv1_weight,
            64,
            7,
            2,
            3,
            conv1,
            im2col,
            self.provider,
        )?;
        self.batch_norm(arena, conv1, 180 * 320, 64, "model.backbone.bn1", true)?;
        max_pool_3x3(arena, conv1, 180, 320, 64, feature_a);
        progress(InferenceStage::CameraStem { camera });

        let mut current = feature_a;
        let mut current_h = 90usize;
        let mut current_w = 160usize;
        for (block_index, block) in BACKBONE_BLOCKS.into_iter().enumerate() {
            let (next, next_h, next_w) = self.basic_block(
                arena, current, current_h, current_w, feature_a, feature_b, feature_c, im2col,
                block,
            )?;
            current = next;
            current_h = next_h;
            current_w = next_w;
            progress(InferenceStage::CameraBlock {
                camera,
                block: block_index as u8,
            });
        }
        if current_h != FEATURE_HEIGHT || current_w != FEATURE_WIDTH {
            return Err(Error::ShapeMismatch);
        }

        let target = encoder.sub(token_offset * MODEL_DIM, IMAGE_TOKENS * MODEL_DIM);
        let projection_weight = self.weight(
            "model.encoder_img_feat_input_proj.weight",
            &[MODEL_DIM, MODEL_DIM, 1, 1],
        )?;
        let projection_bias =
            self.weight("model.encoder_img_feat_input_proj.bias", &[MODEL_DIM])?;
        let current_ptr = arena.ptr(current);
        let target_ptr = arena.mut_ptr(target);
        self.linear(
            arena,
            current_ptr,
            IMAGE_TOKENS,
            MODEL_DIM,
            projection_weight,
            projection_bias,
            target_ptr,
            MODEL_DIM,
        )?;
        make_2d_position(
            arena,
            encoder_pos.sub(token_offset * MODEL_DIM, IMAGE_TOKENS * MODEL_DIM),
        );
        progress(InferenceStage::CameraProjected { camera });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn basic_block(
        &self,
        arena: &mut Arena<'_>,
        input: Buffer,
        height: usize,
        width: usize,
        a: Buffer,
        b: Buffer,
        c: Buffer,
        im2col: Buffer,
        block: BlockSpec,
    ) -> Result<(Buffer, usize, usize), Error> {
        // `input`在降采样层后会是大缓冲区的前缀切片，长度虽不同但offset
        // 相同；按offset排除才能避免把输入和卷积输出写到同一块内存。
        let mut available = [a, b, c]
            .into_iter()
            .filter(|buffer| buffer.offset != input.offset);
        let temp = available.next().ok_or(Error::WorkspaceTooSmall)?;
        let output = available.next().ok_or(Error::WorkspaceTooSmall)?;
        let output_h = height.div_ceil(block.stride);
        let output_w = width.div_ceil(block.stride);
        let output_len = output_h * output_w * block.out_channels;

        let mut name = Name::new();
        let conv1_name = name.set(&["model.backbone.", block.name, ".conv1.weight"]);
        let conv1_weight =
            self.weight(conv1_name, &[block.out_channels, block.in_channels, 3, 3])?;
        conv2d(
            arena,
            input,
            height,
            width,
            block.in_channels,
            conv1_weight,
            block.out_channels,
            3,
            block.stride,
            1,
            temp.sub(0, output_len),
            im2col,
            self.provider,
        )?;
        let bn1_name = name.set(&["model.backbone.", block.name, ".bn1"]);
        self.batch_norm(
            arena,
            temp.sub(0, output_len),
            output_h * output_w,
            block.out_channels,
            bn1_name,
            true,
        )?;

        let conv2_name = name.set(&["model.backbone.", block.name, ".conv2.weight"]);
        let conv2_weight =
            self.weight(conv2_name, &[block.out_channels, block.out_channels, 3, 3])?;
        conv2d(
            arena,
            temp.sub(0, output_len),
            output_h,
            output_w,
            block.out_channels,
            conv2_weight,
            block.out_channels,
            3,
            1,
            1,
            output.sub(0, output_len),
            im2col,
            self.provider,
        )?;
        let bn2_name = name.set(&["model.backbone.", block.name, ".bn2"]);
        self.batch_norm(
            arena,
            output.sub(0, output_len),
            output_h * output_w,
            block.out_channels,
            bn2_name,
            false,
        )?;

        let shortcut = if block.downsample {
            let shortcut = temp.sub(0, output_len);
            let down_weight_name =
                name.set(&["model.backbone.", block.name, ".downsample.0.weight"]);
            let down_weight = self.weight(
                down_weight_name,
                &[block.out_channels, block.in_channels, 1, 1],
            )?;
            conv2d(
                arena,
                input,
                height,
                width,
                block.in_channels,
                down_weight,
                block.out_channels,
                1,
                block.stride,
                0,
                shortcut,
                im2col,
                self.provider,
            )?;
            let down_bn_name = name.set(&["model.backbone.", block.name, ".downsample.1"]);
            self.batch_norm(
                arena,
                shortcut,
                output_h * output_w,
                block.out_channels,
                down_bn_name,
                false,
            )?;
            shortcut
        } else {
            input.sub(0, output_len)
        };
        add_relu(arena, output.sub(0, output_len), shortcut);
        Ok((output.sub(0, output_len), output_h, output_w))
    }

    fn batch_norm(
        &self,
        arena: &mut Arena<'_>,
        buffer: Buffer,
        pixels: usize,
        channels: usize,
        prefix: &str,
        relu: bool,
    ) -> Result<(), Error> {
        let mut name = Name::new();
        let weight = self.weight(name.set(&[prefix, ".weight"]), &[channels])?;
        let bias = self.weight(name.set(&[prefix, ".bias"]), &[channels])?;
        let mean = self.weight(name.set(&[prefix, ".running_mean"]), &[channels])?;
        let variance = self.weight(name.set(&[prefix, ".running_var"]), &[channels])?;
        frozen_batch_norm(
            arena, buffer, pixels, channels, weight, bias, mean, variance, relu,
        );
        Ok(())
    }

    fn encoder_layer(
        &self,
        arena: &mut Arena<'_>,
        x: Buffer,
        pos: Buffer,
        prefix: &str,
        scratch_mark: usize,
    ) -> Result<(), Error> {
        let attention = self.attention(
            arena,
            x,
            pos,
            x,
            pos,
            x,
            ENCODER_TOKENS,
            ENCODER_TOKENS,
            prefix,
            "self_attn",
        )?;
        let mut name = Name::new();
        let norm1_weight = self.weight(name.set(&[prefix, ".norm1.weight"]), &[MODEL_DIM])?;
        let norm1_bias = self.weight(name.set(&[prefix, ".norm1.bias"]), &[MODEL_DIM])?;
        add_layer_norm(
            arena,
            x,
            attention,
            ENCODER_TOKENS,
            norm1_weight,
            norm1_bias,
        );
        arena.reset(scratch_mark);
        self.feed_forward(arena, x, ENCODER_TOKENS, prefix, "norm2", scratch_mark)
    }

    fn decoder_layer(
        &self,
        arena: &mut Arena<'_>,
        x: Buffer,
        x_pos: Buffer,
        encoder: Buffer,
        encoder_pos: Buffer,
        scratch_mark: usize,
    ) -> Result<(), Error> {
        let prefix = "model.decoder.layers.0";
        let self_attention = self.attention(
            arena,
            x,
            x_pos,
            x,
            x_pos,
            x,
            ACTION_STEPS,
            ACTION_STEPS,
            prefix,
            "self_attn",
        )?;
        let mut name = Name::new();
        let norm1_weight = self.weight(name.set(&[prefix, ".norm1.weight"]), &[MODEL_DIM])?;
        let norm1_bias = self.weight(name.set(&[prefix, ".norm1.bias"]), &[MODEL_DIM])?;
        add_layer_norm(
            arena,
            x,
            self_attention,
            ACTION_STEPS,
            norm1_weight,
            norm1_bias,
        );
        arena.reset(scratch_mark);

        let cross_attention = self.attention(
            arena,
            x,
            x_pos,
            encoder,
            encoder_pos,
            encoder,
            ACTION_STEPS,
            ENCODER_TOKENS,
            prefix,
            "multihead_attn",
        )?;
        let norm2_weight = self.weight(name.set(&[prefix, ".norm2.weight"]), &[MODEL_DIM])?;
        let norm2_bias = self.weight(name.set(&[prefix, ".norm2.bias"]), &[MODEL_DIM])?;
        add_layer_norm(
            arena,
            x,
            cross_attention,
            ACTION_STEPS,
            norm2_weight,
            norm2_bias,
        );
        arena.reset(scratch_mark);
        self.feed_forward(arena, x, ACTION_STEPS, prefix, "norm3", scratch_mark)
    }

    #[allow(clippy::too_many_arguments)]
    fn attention(
        &self,
        arena: &mut Arena<'_>,
        query: Buffer,
        query_pos: Buffer,
        key: Buffer,
        key_pos: Buffer,
        value: Buffer,
        query_rows: usize,
        key_rows: usize,
        prefix: &str,
        module: &str,
    ) -> Result<Buffer, Error> {
        let mut name = Name::new();
        let in_weight_name = name.set(&[prefix, ".", module, ".in_proj_weight"]);
        let in_weight = self.weight(in_weight_name, &[3 * MODEL_DIM, MODEL_DIM])?;
        let in_bias_name = name.set(&[prefix, ".", module, ".in_proj_bias"]);
        let in_bias = self.weight(in_bias_name, &[3 * MODEL_DIM])?;

        let q = arena.alloc(query_rows * MODEL_DIM)?;
        let k = arena.alloc(key_rows * MODEL_DIM)?;
        let v = arena.alloc(key_rows * MODEL_DIM)?;
        let context = arena.alloc(query_rows * MODEL_DIM)?;
        let scores = arena.alloc(key_rows)?;
        unsafe {
            linear_sum_raw(
                arena.ptr(query),
                arena.ptr(query_pos),
                query_rows,
                &in_weight[..MODEL_DIM * MODEL_DIM],
                &in_bias[..MODEL_DIM],
                arena.mut_ptr(q),
            );
            linear_sum_raw(
                arena.ptr(key),
                arena.ptr(key_pos),
                key_rows,
                &in_weight[MODEL_DIM * MODEL_DIM..2 * MODEL_DIM * MODEL_DIM],
                &in_bias[MODEL_DIM..2 * MODEL_DIM],
                arena.mut_ptr(k),
            );
            let value_ptr = arena.ptr(value);
            let value_output = arena.mut_ptr(v);
            self.linear(
                arena,
                value_ptr,
                key_rows,
                MODEL_DIM,
                &in_weight[2 * MODEL_DIM * MODEL_DIM..],
                &in_bias[2 * MODEL_DIM..],
                value_output,
                MODEL_DIM,
            )?;
            attention_context(
                arena.ptr(q),
                arena.ptr(k),
                arena.ptr(v),
                query_rows,
                key_rows,
                arena.mut_ptr(scores),
                arena.mut_ptr(context),
            );
        }
        let output = arena.alloc(query_rows * MODEL_DIM)?;
        let out_weight_name = name.set(&[prefix, ".", module, ".out_proj.weight"]);
        let out_weight = self.weight(out_weight_name, &[MODEL_DIM, MODEL_DIM])?;
        let out_bias_name = name.set(&[prefix, ".", module, ".out_proj.bias"]);
        let out_bias = self.weight(out_bias_name, &[MODEL_DIM])?;
        let context_ptr = arena.ptr(context);
        let output_ptr = arena.mut_ptr(output);
        self.linear(
            arena,
            context_ptr,
            query_rows,
            MODEL_DIM,
            out_weight,
            out_bias,
            output_ptr,
            MODEL_DIM,
        )?;
        Ok(output)
    }

    fn feed_forward(
        &self,
        arena: &mut Arena<'_>,
        x: Buffer,
        rows: usize,
        prefix: &str,
        norm: &str,
        scratch_mark: usize,
    ) -> Result<(), Error> {
        let mut name = Name::new();
        let hidden = arena.alloc(rows * FF_DIM)?;
        let linear1_weight =
            self.weight(name.set(&[prefix, ".linear1.weight"]), &[FF_DIM, MODEL_DIM])?;
        let linear1_bias = self.weight(name.set(&[prefix, ".linear1.bias"]), &[FF_DIM])?;
        let x_ptr = arena.ptr(x);
        let hidden_ptr = arena.mut_ptr(hidden);
        self.linear(
            arena,
            x_ptr,
            rows,
            MODEL_DIM,
            linear1_weight,
            linear1_bias,
            hidden_ptr,
            FF_DIM,
        )?;
        relu_in_place(arena, hidden);
        let output = arena.alloc(rows * MODEL_DIM)?;
        let linear2_weight =
            self.weight(name.set(&[prefix, ".linear2.weight"]), &[MODEL_DIM, FF_DIM])?;
        let linear2_bias = self.weight(name.set(&[prefix, ".linear2.bias"]), &[MODEL_DIM])?;
        let hidden_ptr = arena.ptr(hidden);
        let output_ptr = arena.mut_ptr(output);
        self.linear(
            arena,
            hidden_ptr,
            rows,
            FF_DIM,
            linear2_weight,
            linear2_bias,
            output_ptr,
            MODEL_DIM,
        )?;
        let norm_weight = self.weight(name.set(&[prefix, ".", norm, ".weight"]), &[MODEL_DIM])?;
        let norm_bias = self.weight(name.set(&[prefix, ".", norm, ".bias"]), &[MODEL_DIM])?;
        add_layer_norm(arena, x, output, rows, norm_weight, norm_bias);
        arena.reset(scratch_mark);
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct BlockSpec {
    name: &'static str,
    in_channels: usize,
    out_channels: usize,
    stride: usize,
    downsample: bool,
}

impl BlockSpec {
    const fn new(
        name: &'static str,
        in_channels: usize,
        out_channels: usize,
        stride: usize,
        downsample: bool,
    ) -> Self {
        Self {
            name,
            in_channels,
            out_channels,
            stride,
            downsample,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Buffer {
    offset: usize,
    len: usize,
}

impl Buffer {
    fn sub(self, offset: usize, len: usize) -> Self {
        debug_assert!(offset + len <= self.len);
        Self {
            offset: self.offset + offset,
            len,
        }
    }
}

/// 只在一次predict期间存在的线性工作区分配器。
struct Arena<'a> {
    base: *mut f32,
    len: usize,
    top: usize,
    _borrow: PhantomData<&'a mut [f32]>,
}

impl<'a> Arena<'a> {
    fn new(data: &'a mut [f32]) -> Self {
        Self {
            base: data.as_mut_ptr(),
            len: data.len(),
            top: 0,
            _borrow: PhantomData,
        }
    }

    fn alloc(&mut self, len: usize) -> Result<Buffer, Error> {
        let end = self.top.checked_add(len).ok_or(Error::WorkspaceTooSmall)?;
        if end > self.len {
            return Err(Error::WorkspaceTooSmall);
        }
        let result = Buffer {
            offset: self.top,
            len,
        };
        self.top = end;
        Ok(result)
    }

    fn mark(&self) -> usize {
        self.top
    }

    fn reset(&mut self, mark: usize) {
        debug_assert!(mark <= self.top);
        self.top = mark;
    }

    fn ptr(&self, buffer: Buffer) -> *const f32 {
        debug_assert!(buffer.offset + buffer.len <= self.len);
        unsafe { self.base.add(buffer.offset) }
    }

    fn mut_ptr(&mut self, buffer: Buffer) -> *mut f32 {
        debug_assert!(buffer.offset + buffer.len <= self.len);
        unsafe { self.base.add(buffer.offset) }
    }

    fn fill(&mut self, buffer: Buffer, value: f32) {
        let pointer = self.mut_ptr(buffer);
        for index in 0..buffer.len {
            unsafe { pointer.add(index).write(value) };
        }
    }

    fn copy_into(&mut self, buffer: Buffer, source: &[f32]) {
        debug_assert_eq!(buffer.len, source.len());
        unsafe {
            core::ptr::copy_nonoverlapping(source.as_ptr(), self.mut_ptr(buffer), source.len());
        }
    }
}

struct Name {
    bytes: [u8; 128],
    len: usize,
}

impl Name {
    fn new() -> Self {
        Self {
            bytes: [0; 128],
            len: 0,
        }
    }

    fn set(&mut self, parts: &[&str]) -> &str {
        self.len = 0;
        for part in parts {
            let end = self.len + part.len();
            assert!(end <= self.bytes.len());
            self.bytes[self.len..end].copy_from_slice(part.as_bytes());
            self.len = end;
        }
        // SAFETY:所有输入part都是有效UTF-8，拼接不会改变编码。
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..self.len]) }
    }
}

fn copy_weight(
    weights: &SafeTensors<'_>,
    name: &str,
    shape: &[usize],
    arena: &mut Arena<'_>,
    output: Buffer,
) -> Result<(), Error> {
    let tensor = weights.tensor(name)?;
    if tensor.shape() != shape || tensor.element_count() != output.len {
        return Err(Error::ShapeMismatch);
    }
    arena.copy_into(output, tensor.f32_data()?);
    Ok(())
}

fn normalize_rgb_hwc(
    arena: &mut Arena<'_>,
    output: Buffer,
    rgb: &[u8],
    mean: &[f32; 3],
    std: &[f32; 3],
) {
    let mut context = NormalizeContext {
        target: arena.mut_ptr(output),
        rgb: rgb.as_ptr(),
        mean: *mean,
        std: *std,
    };
    unsafe {
        run_rows(
            (&mut context as *mut NormalizeContext).cast(),
            IMAGE_HEIGHT * IMAGE_WIDTH,
            normalize_rows,
        )
    };
}

struct NormalizeContext {
    target: *mut f32,
    rgb: *const u8,
    mean: [f32; 3],
    std: [f32; 3],
}

unsafe fn normalize_rows(context: *mut (), start_pixel: usize, end_pixel: usize) {
    let context = &*(context.cast::<NormalizeContext>());
    for pixel in start_pixel..end_pixel {
        for channel in 0..3 {
            let index = pixel * 3 + channel;
            let value = *context.rgb.add(index) as f32 * (1.0 / 255.0);
            context
                .target
                .add(index)
                .write((value - context.mean[channel]) / context.std[channel]);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn conv2d(
    arena: &mut Arena<'_>,
    input: Buffer,
    input_h: usize,
    input_w: usize,
    input_c: usize,
    weight: &[f32],
    output_c: usize,
    kernel: usize,
    stride: usize,
    padding: usize,
    output: Buffer,
    im2col: Buffer,
    provider: KernelProvider,
) -> Result<(), Error> {
    let output_h = (input_h + 2 * padding - kernel) / stride + 1;
    let output_w = (input_w + 2 * padding - kernel) / stride + 1;
    let rows = output_h * output_w;
    let columns = input_c * kernel * kernel;
    debug_assert!(im2col.len >= rows * columns);
    debug_assert_eq!(output.len, rows * output_c);
    debug_assert_eq!(weight.len(), output_c * columns);

    if provider == KernelProvider::AclNeon {
        #[cfg(feature = "acl-neon")]
        {
            let config = Conv2dConfig {
                input_height: input_h,
                input_width: input_w,
                input_channels: input_c,
                output_channels: output_c,
                kernel_height: kernel,
                kernel_width: kernel,
                output_height: output_h,
                output_width: output_w,
                strides: [stride, stride],
                pads: [padding, padding, padding, padding],
                dilations: [1, 1],
            };
            let bytes = act_kernels_acl::conv2d_raw_temporary_bytes(&config)
                .map_err(|_| Error::AclProviderUnavailable)?;
            if bytes > im2col.len * core::mem::size_of::<f32>() {
                return Err(Error::WorkspaceTooSmall);
            }
            let input_ptr = arena.ptr(input);
            let output_ptr = arena.mut_ptr(output);
            let temporary_ptr = arena.mut_ptr(im2col).cast::<u8>();
            // SAFETY: Buffer 长度由上层 shape 校验；C shim 不保存这些借用，且
            // 在返回前完成所有 NEON 工作。
            return unsafe {
                act_kernels_acl::run_conv2d_raw(
                    &config,
                    core::slice::from_raw_parts(input_ptr, input_h * input_w * input_c),
                    weight,
                    None,
                    core::slice::from_raw_parts_mut(output_ptr, rows * output_c),
                    core::slice::from_raw_parts_mut(temporary_ptr, bytes),
                )
            }
            .map_err(|_| Error::AclProviderUnavailable);
        }
        #[cfg(not(feature = "acl-neon"))]
        {
            let _ = (
                arena, input, input_h, input_w, input_c, weight, output_c, kernel, stride, padding,
                output, im2col,
            );
            return Err(Error::AclProviderUnavailable);
        }
    }

    let source = arena.ptr(input);
    let col = arena.mut_ptr(im2col);
    let mut context = Im2ColContext {
        source,
        col,
        input_h,
        input_w,
        input_c,
        output_w,
        columns,
        kernel,
        stride,
        padding,
    };
    unsafe {
        run_rows(
            (&mut context as *mut Im2ColContext).cast(),
            rows,
            im2col_rows,
        );
        linear_raw(
            col,
            rows,
            columns,
            weight,
            &[],
            arena.mut_ptr(output),
            output_c,
        );
    }
    Ok(())
}

struct Im2ColContext {
    source: *const f32,
    col: *mut f32,
    input_h: usize,
    input_w: usize,
    input_c: usize,
    output_w: usize,
    columns: usize,
    kernel: usize,
    stride: usize,
    padding: usize,
}

unsafe fn im2col_rows(context: *mut (), start_row: usize, end_row: usize) {
    let context = &*(context.cast::<Im2ColContext>());
    for row in start_row..end_row {
        let oy = row / context.output_w;
        let ox = row % context.output_w;
        let mut column = 0usize;
        for channel in 0..context.input_c {
            for ky in 0..context.kernel {
                let iy = oy * context.stride + ky;
                for kx in 0..context.kernel {
                    let ix = ox * context.stride + kx;
                    let value = if iy < context.padding
                        || ix < context.padding
                        || iy >= context.input_h + context.padding
                        || ix >= context.input_w + context.padding
                    {
                        0.0
                    } else {
                        let source_y = iy - context.padding;
                        let source_x = ix - context.padding;
                        *context.source.add(
                            (source_y * context.input_w + source_x) * context.input_c + channel,
                        )
                    };
                    context.col.add(row * context.columns + column).write(value);
                    column += 1;
                }
            }
        }
    }
}

fn max_pool_3x3(
    arena: &mut Arena<'_>,
    input: Buffer,
    input_h: usize,
    input_w: usize,
    channels: usize,
    output: Buffer,
) {
    let output_h = (input_h + 1) / 2;
    let output_w = (input_w + 1) / 2;
    let source = arena.ptr(input);
    let target = arena.mut_ptr(output);
    let mut context = MaxPoolContext {
        source,
        target,
        input_h,
        input_w,
        output_w,
        channels,
    };
    unsafe {
        run_rows(
            (&mut context as *mut MaxPoolContext).cast(),
            output_h * output_w,
            max_pool_rows,
        )
    };
}

struct MaxPoolContext {
    source: *const f32,
    target: *mut f32,
    input_h: usize,
    input_w: usize,
    output_w: usize,
    channels: usize,
}

unsafe fn max_pool_rows(context: *mut (), start_row: usize, end_row: usize) {
    let context = &*(context.cast::<MaxPoolContext>());
    for row in start_row..end_row {
        let oy = row / context.output_w;
        let ox = row % context.output_w;
        for channel in 0..context.channels {
            let mut maximum = f32::NEG_INFINITY;
            for ky in 0..3 {
                let iy = oy * 2 + ky;
                for kx in 0..3 {
                    let ix = ox * 2 + kx;
                    if iy == 0 || ix == 0 || iy > context.input_h || ix > context.input_w {
                        continue;
                    }
                    let value = *context
                        .source
                        .add(((iy - 1) * context.input_w + (ix - 1)) * context.channels + channel);
                    maximum = maximum.max(value);
                }
            }
            context
                .target
                .add(row * context.channels + channel)
                .write(maximum);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn frozen_batch_norm(
    arena: &mut Arena<'_>,
    buffer: Buffer,
    pixels: usize,
    channels: usize,
    weight: &[f32],
    bias: &[f32],
    mean: &[f32],
    variance: &[f32],
    relu: bool,
) {
    let pointer = arena.mut_ptr(buffer);
    let mut context = BatchNormContext {
        pointer,
        pixels,
        channels,
        weight: weight.as_ptr(),
        bias: bias.as_ptr(),
        mean: mean.as_ptr(),
        variance: variance.as_ptr(),
        relu,
    };
    unsafe {
        run_rows(
            (&mut context as *mut BatchNormContext).cast(),
            channels,
            batch_norm_channels,
        )
    };
}

struct BatchNormContext {
    pointer: *mut f32,
    pixels: usize,
    channels: usize,
    weight: *const f32,
    bias: *const f32,
    mean: *const f32,
    variance: *const f32,
    relu: bool,
}

unsafe fn batch_norm_channels(context: *mut (), start_channel: usize, end_channel: usize) {
    let context = &*(context.cast::<BatchNormContext>());
    for channel in start_channel..end_channel {
        let scale = *context.weight.add(channel) / sqrt(*context.variance.add(channel) + 1.0e-5);
        let offset = *context.bias.add(channel) - *context.mean.add(channel) * scale;
        for pixel in 0..context.pixels {
            let address = context.pointer.add(pixel * context.channels + channel);
            let mut value = unsafe { *address } * scale + offset;
            if context.relu && value < 0.0 {
                value = 0.0;
            }
            address.write(value);
        }
    }
}

fn add_relu(arena: &mut Arena<'_>, output: Buffer, shortcut: Buffer) {
    let out = arena.mut_ptr(output);
    let skip = arena.ptr(shortcut);
    for index in 0..output.len {
        let value = unsafe { *out.add(index) + *skip.add(index) };
        unsafe { out.add(index).write(value.max(0.0)) };
    }
}

fn relu_in_place(arena: &mut Arena<'_>, buffer: Buffer) {
    let pointer = arena.mut_ptr(buffer);
    for index in 0..buffer.len {
        let value = unsafe { *pointer.add(index) };
        if value < 0.0 {
            unsafe { pointer.add(index).write(0.0) };
        }
    }
}

fn make_2d_position(arena: &mut Arena<'_>, output: Buffer) {
    let target = arena.mut_ptr(output);
    const TWO_PI: f32 = 6.283185307179586;
    const FREQUENCY_STEP: f32 = 1.0746078283213174;
    for y in 0..FEATURE_HEIGHT {
        let y_position = (y + 1) as f32 / (FEATURE_HEIGHT as f32 + 1.0e-6) * TWO_PI;
        for x in 0..FEATURE_WIDTH {
            let x_position = (x + 1) as f32 / (FEATURE_WIDTH as f32 + 1.0e-6) * TWO_PI;
            let row = (y * FEATURE_WIDTH + x) * MODEL_DIM;
            let mut frequency = 1.0f32;
            for pair in 0..128 {
                let (sin_y, cos_y) = sin_cos(y_position / frequency);
                let (sin_x, cos_x) = sin_cos(x_position / frequency);
                unsafe {
                    target.add(row + pair * 2).write(sin_y);
                    target.add(row + pair * 2 + 1).write(cos_y);
                    target.add(row + 256 + pair * 2).write(sin_x);
                    target.add(row + 256 + pair * 2 + 1).write(cos_x);
                }
                frequency *= FREQUENCY_STEP;
            }
        }
    }
}

fn add_layer_norm(
    arena: &mut Arena<'_>,
    destination: Buffer,
    addition: Buffer,
    rows: usize,
    weight: &[f32],
    bias: &[f32],
) {
    let dst = arena.mut_ptr(destination);
    let add = arena.ptr(addition);
    for index in 0..destination.len {
        unsafe { dst.add(index).write(*dst.add(index) + *add.add(index)) };
    }
    layer_norm_raw(dst, rows, weight, bias);
}

fn layer_norm_in_place(
    arena: &mut Arena<'_>,
    buffer: Buffer,
    rows: usize,
    weight: &[f32],
    bias: &[f32],
) {
    layer_norm_raw(arena.mut_ptr(buffer), rows, weight, bias);
}

fn layer_norm_raw(pointer: *mut f32, rows: usize, weight: &[f32], bias: &[f32]) {
    for row in 0..rows {
        let base = unsafe { pointer.add(row * MODEL_DIM) };
        let mut mean = 0.0f32;
        for column in 0..MODEL_DIM {
            mean += unsafe { *base.add(column) };
        }
        mean /= MODEL_DIM as f32;
        let mut variance = 0.0f32;
        for column in 0..MODEL_DIM {
            let delta = unsafe { *base.add(column) } - mean;
            variance += delta * delta;
        }
        variance /= MODEL_DIM as f32;
        let inverse_std = 1.0 / sqrt(variance + LAYER_NORM_EPS);
        for column in 0..MODEL_DIM {
            let normalized = (unsafe { *base.add(column) } - mean) * inverse_std;
            unsafe {
                base.add(column)
                    .write(normalized * weight[column] + bias[column])
            };
        }
    }
}

unsafe fn linear_raw(
    input: *const f32,
    rows: usize,
    input_dim: usize,
    weight: &[f32],
    bias: &[f32],
    output: *mut f32,
    output_dim: usize,
) {
    debug_assert_eq!(weight.len(), output_dim * input_dim);
    debug_assert!(bias.is_empty() || bias.len() == output_dim);
    let mut context = LinearContext {
        input,
        input_dim,
        weight: weight.as_ptr(),
        bias: bias.as_ptr(),
        has_bias: !bias.is_empty(),
        output,
        output_dim,
    };
    run_rows(
        (&mut context as *mut LinearContext).cast(),
        rows,
        linear_rows,
    );
}

/// `linear_raw`一次同步调用的只读参数。每个worker只写自己负责的输出行，
/// 因而无需锁；权重和输入在barrier结束前都保持只读。
struct LinearContext {
    input: *const f32,
    input_dim: usize,
    weight: *const f32,
    bias: *const f32,
    has_bias: bool,
    output: *mut f32,
    output_dim: usize,
}

unsafe fn linear_rows(context: *mut (), start_row: usize, end_row: usize) {
    let context = &*(context.cast::<LinearContext>());
    let mut row = start_row;
    // 相邻输出行共享同一组权重。卷积的im2col行和Transformer token行都
    // 连续存放；一次处理4行可让每次权重cache-line/NEON load服务4个点积，
    // 而每行仍使用独立累加器并保持原来的FMA归并次序。
    while row + 4 <= end_row {
        let sources = [
            context.input.add(row * context.input_dim),
            context.input.add((row + 1) * context.input_dim),
            context.input.add((row + 2) * context.input_dim),
            context.input.add((row + 3) * context.input_dim),
        ];
        for channel in 0..context.output_dim {
            let values = dot_rows4(
                sources,
                context.weight.add(channel * context.input_dim),
                context.input_dim,
            );
            let bias = if context.has_bias {
                *context.bias.add(channel)
            } else {
                0.0
            };
            for lane in 0..4 {
                context
                    .output
                    .add((row + lane) * context.output_dim + channel)
                    .write(values[lane] + bias);
            }
        }
        row += 4;
    }
    while row < end_row {
        let source = context.input.add(row * context.input_dim);
        for channel in 0..context.output_dim {
            let value = dot(
                source,
                context.weight.add(channel * context.input_dim),
                context.input_dim,
            ) + if context.has_bias {
                *context.bias.add(channel)
            } else {
                0.0
            };
            context
                .output
                .add(row * context.output_dim + channel)
                .write(value);
        }
        row += 1;
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn dot_rows4(left: [*const f32; 4], right: *const f32, len: usize) -> [f32; 4] {
    use core::arch::aarch64::{
        float32x4_t, vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32,
    };

    let zero = vdupq_n_f32(0.0);
    let mut accumulators: [[float32x4_t; 4]; 4] = [[zero; 4]; 4];
    let mut index = 0usize;
    while index + 16 <= len {
        for part in 0..4 {
            let offset = index + part * 4;
            let weights = vld1q_f32(right.add(offset));
            for lane in 0..4 {
                accumulators[lane][part] = vfmaq_f32(
                    accumulators[lane][part],
                    vld1q_f32(left[lane].add(offset)),
                    weights,
                );
            }
        }
        index += 16;
    }

    let mut result = [0.0f32; 4];
    for lane in 0..4 {
        let low = vaddq_f32(accumulators[lane][0], accumulators[lane][1]);
        let high = vaddq_f32(accumulators[lane][2], accumulators[lane][3]);
        result[lane] = vaddvq_f32(vaddq_f32(low, high));
    }
    while index + 4 <= len {
        let weights = vld1q_f32(right.add(index));
        for lane in 0..4 {
            result[lane] += vaddvq_f32(vfmaq_f32(zero, vld1q_f32(left[lane].add(index)), weights));
        }
        index += 4;
    }
    while index < len {
        let weight = *right.add(index);
        for lane in 0..4 {
            result[lane] += *left[lane].add(index) * weight;
        }
        index += 1;
    }
    result
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn dot_rows4(left: [*const f32; 4], right: *const f32, len: usize) -> [f32; 4] {
    [
        dot(left[0], right, len),
        dot(left[1], right, len),
        dot(left[2], right, len),
        dot(left[3], right, len),
    ]
}

unsafe fn linear_sum_raw(
    input: *const f32,
    position: *const f32,
    rows: usize,
    weight: &[f32],
    bias: &[f32],
    output: *mut f32,
) {
    let mut context = LinearSumContext {
        input,
        position,
        weight: weight.as_ptr(),
        bias: bias.as_ptr(),
        output,
    };
    run_rows(
        (&mut context as *mut LinearSumContext).cast(),
        rows,
        linear_sum_rows,
    );
}

struct LinearSumContext {
    input: *const f32,
    position: *const f32,
    weight: *const f32,
    bias: *const f32,
    output: *mut f32,
}

unsafe fn linear_sum_rows(context: *mut (), start_row: usize, end_row: usize) {
    let context = &*(context.cast::<LinearSumContext>());
    for row in start_row..end_row {
        let source = context.input.add(row * MODEL_DIM);
        let pos = context.position.add(row * MODEL_DIM);
        for channel in 0..MODEL_DIM {
            let weights = context.weight.add(channel * MODEL_DIM);
            let mut sum = *context.bias.add(channel);
            for index in 0..MODEL_DIM {
                sum += (*source.add(index) + *pos.add(index)) * *weights.add(index);
            }
            context.output.add(row * MODEL_DIM + channel).write(sum);
        }
    }
}

unsafe fn attention_context(
    q: *const f32,
    k: *const f32,
    v: *const f32,
    query_rows: usize,
    key_rows: usize,
    scores: *mut f32,
    output: *mut f32,
) {
    core::ptr::write_bytes(output, 0, query_rows * MODEL_DIM);
    for query_row in 0..query_rows {
        for head in 0..HEADS {
            let query = q.add(query_row * MODEL_DIM + head * HEAD_DIM);
            let mut maximum = f32::NEG_INFINITY;
            for key_row in 0..key_rows {
                let key = k.add(key_row * MODEL_DIM + head * HEAD_DIM);
                let score = dot(query, key, HEAD_DIM) * 0.125;
                scores.add(key_row).write(score);
                maximum = maximum.max(score);
            }
            let mut sum = 0.0f32;
            for key_row in 0..key_rows {
                let value = exp_approx(*scores.add(key_row) - maximum);
                scores.add(key_row).write(value);
                sum += value;
            }
            let inverse_sum = 1.0 / sum;
            let target = output.add(query_row * MODEL_DIM + head * HEAD_DIM);
            for key_row in 0..key_rows {
                let probability = *scores.add(key_row) * inverse_sum;
                let value = v.add(key_row * MODEL_DIM + head * HEAD_DIM);
                for dimension in 0..HEAD_DIM {
                    let address = target.add(dimension);
                    address.write(*address + probability * *value.add(dimension));
                }
            }
        }
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn dot(left: *const f32, right: *const f32, len: usize) -> f32 {
    use core::arch::aarch64::{vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32};
    // Cortex-A76的FMA有流水线延迟。单一vector会形成逐条依赖链；四个独立
    // 累加器允许CPU同时发射多条NEON FMA，再在末尾归并，明显提高Conv/Linear
    // 的点积吞吐。每次仍只读取当前行和权重，不改变跨线程写入边界。
    let mut vector0 = vdupq_n_f32(0.0);
    let mut vector1 = vdupq_n_f32(0.0);
    let mut vector2 = vdupq_n_f32(0.0);
    let mut vector3 = vdupq_n_f32(0.0);
    let mut index = 0usize;
    while index + 16 <= len {
        vector0 = vfmaq_f32(
            vector0,
            vld1q_f32(left.add(index)),
            vld1q_f32(right.add(index)),
        );
        vector1 = vfmaq_f32(
            vector1,
            vld1q_f32(left.add(index + 4)),
            vld1q_f32(right.add(index + 4)),
        );
        vector2 = vfmaq_f32(
            vector2,
            vld1q_f32(left.add(index + 8)),
            vld1q_f32(right.add(index + 8)),
        );
        vector3 = vfmaq_f32(
            vector3,
            vld1q_f32(left.add(index + 12)),
            vld1q_f32(right.add(index + 12)),
        );
        index += 16;
    }
    vector0 = core::arch::aarch64::vaddq_f32(vector0, vector1);
    vector2 = core::arch::aarch64::vaddq_f32(vector2, vector3);
    vector0 = core::arch::aarch64::vaddq_f32(vector0, vector2);
    let mut sum = vaddvq_f32(vector0);
    while index + 4 <= len {
        sum += vaddvq_f32(vfmaq_f32(
            vdupq_n_f32(0.0),
            vld1q_f32(left.add(index)),
            vld1q_f32(right.add(index)),
        ));
        index += 4;
    }
    while index < len {
        sum += *left.add(index) * *right.add(index);
        index += 1;
    }
    sum
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn dot(left: *const f32, right: *const f32, len: usize) -> f32 {
    let mut sum = 0.0f32;
    for index in 0..len {
        sum += *left.add(index) * *right.add(index);
    }
    sum
}

fn sqrt(value: f32) -> f32 {
    if value <= 0.0 {
        return 0.0;
    }
    let mut estimate = f32::from_bits((value.to_bits() >> 1) + 0x1fc0_0000);
    for _ in 0..4 {
        estimate = 0.5 * (estimate + value / estimate);
    }
    estimate
}

fn exp_approx(value: f32) -> f32 {
    if value <= -80.0 {
        return 0.0;
    }
    let y = value * 1.4426950408889634;
    let truncated = y as i32;
    let exponent = if y < truncated as f32 {
        truncated - 1
    } else {
        truncated
    };
    let fraction = y - exponent as f32;
    let polynomial = 1.0
        + fraction
            * (0.69314718056
                + fraction
                    * (0.24022650695
                        + fraction
                            * (0.05550410866
                                + fraction * (0.00961812911 + fraction * 0.00133335581))));
    if exponent < -126 {
        0.0
    } else if exponent > 127 {
        f32::INFINITY
    } else {
        f32::from_bits(((exponent + 127) as u32) << 23) * polynomial
    }
}

fn sin_cos(mut value: f32) -> (f32, f32) {
    const PI: f32 = 3.141592653589793;
    const HALF_PI: f32 = 1.5707963267948966;
    const TWO_PI: f32 = 6.283185307179586;
    while value > PI {
        value -= TWO_PI;
    }
    while value < -PI {
        value += TWO_PI;
    }
    let mut cos_sign = 1.0;
    if value > HALF_PI {
        value = PI - value;
        cos_sign = -1.0;
    } else if value < -HALF_PI {
        value = -PI - value;
        cos_sign = -1.0;
    }
    let square = value * value;
    let sine = value
        * (1.0
            + square
                * (-1.0 / 6.0
                    + square * (1.0 / 120.0 + square * (-1.0 / 5040.0 + square / 362880.0))));
    let cosine = 1.0
        + square
            * (-1.0 / 2.0 + square * (1.0 / 24.0 + square * (-1.0 / 720.0 + square / 40320.0)));
    (sine, cosine * cos_sign)
}
