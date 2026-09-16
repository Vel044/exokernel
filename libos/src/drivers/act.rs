//! ACT策略模型与Frame工作区适配层。
//!
//! safetensors字节由用户态ext4读取到普通Frame，本模块只解析调用者提供的
//! 切片，并在同一Frame arena后半段建立约68MiB工作区。卷积和Attention
//! 运行时不执行文件I/O，也不会反复进入EL1。

use alloc::vec::Vec;

use act_runtime::{
    ActModel, InferenceStage, KernelProvider, LoadOptions, Observation, ACTION_DIM, ACTION_STEPS,
    WORKSPACE_FLOATS,
};
#[cfg(feature = "acl-neon")]
use core::mem::MaybeUninit;

use crate::memory::{Frame, Mapping, Rights};

const MAX_FRAME_PAGES: u64 = 4096;
// 模型文件在普通Frame地址窗口中的起始VA。
pub const MODEL_FILE_VA: u64 = exo_abi::FRAME_ARENA_BASE;
// 模型文件允许占用的最大映射空间。
pub const MODEL_FILE_MAX_SIZE: u64 = 256 * 1024 * 1024;
// 归一化参数文件紧跟模型窗口之后。
pub const STATS_FILE_VA: u64 = MODEL_FILE_VA + MODEL_FILE_MAX_SIZE;
// 归一化参数文件允许占用的最大空间。
pub const STATS_FILE_MAX_SIZE: u64 = 64 * 1024;
// 给stats文件保留1MiB窗口，确保工作区起点仍为页对齐且不与文件重叠。
const WORKSPACE_VA: u64 = STATS_FILE_VA + 1024 * 1024;
const WORKSPACE_BYTES: u64 = (WORKSPACE_FLOATS * core::mem::size_of::<f32>()) as u64;
const PERSISTENT_VA: u64 = WORKSPACE_VA
    + WORKSPACE_BYTES.div_ceil(exo_abi::PAGE_SIZE) * exo_abi::PAGE_SIZE;
/// ACT case、JPEG 和动作文件的共享输入窗口；模型、临时 workspace 与 ACL
/// persistent arena 必须在该地址之前结束。
pub const ACT_INPUT_VA: u64 = exo_abi::ACT_INPUT_ARENA_BASE;

pub struct Policy<'a> {
    // model 必须先于 persistent 字段析构；ACL 句柄中的裸指针只指向后者拥有
    // 的 EL0 映射，不允许从 Policy 单独取出 ActModel。
    model: ActModel<'a>,
    _persistent: Option<MappedWorkspace>,
    workspace: MappedWorkspace,
}

impl<'a> Policy<'a> {
    pub fn from_files(model_bytes: &'a [u8], stats_bytes: &'a [u8]) -> Result<Self, u64> {
        if model_bytes.is_empty()
            || model_bytes.len() as u64 > MODEL_FILE_MAX_SIZE
            || stats_bytes.is_empty()
            || stats_bytes.len() as u64 > STATS_FILE_MAX_SIZE
        {
            return Err(0x501);
        }
        let (model, persistent) = {
            #[cfg(feature = "acl-neon")]
            {
                let options = LoadOptions {
                    provider: KernelProvider::SharedAcl,
                    threads: 4,
                };
                let bytes = act_runtime::required_persistent_bytes(
                    model_bytes,
                    stats_bytes,
                    options,
                )
                .map_err(|_| 0x502u64)?;
                let persistent = MappedWorkspace::allocate(PERSISTENT_VA, bytes as u64, ACT_INPUT_VA)?;
                // SAFETY:persistent 映射由返回的 Policy 独占持有，model 字段先于
                // _persistent 析构；因此人工扩展到模型生命周期的借用不会悬空。
                let storage: &'a mut [MaybeUninit<u8>] = unsafe {
                    core::slice::from_raw_parts_mut(
                        persistent.base.cast::<MaybeUninit<u8>>(),
                        bytes,
                    )
                };
                let model = ActModel::load_with_options(
                    model_bytes, stats_bytes, storage, options,
                )
                .map_err(|_| 0x502u64)?;
                (model, Some(persistent))
            }
            #[cfg(not(feature = "acl-neon"))]
            {
                (
                    ActModel::load(model_bytes, stats_bytes).map_err(|_| 0x502u64)?,
                    None,
                )
            }
        };
        let workspace = MappedWorkspace::allocate(
            WORKSPACE_VA,
            WORKSPACE_BYTES,
            PERSISTENT_VA,
        )?;
        Ok(Self {
            model,
            _persistent: persistent,
            workspace,
        })
    }

    /// 输入两张`640x360 RGB HWC`照片和六个舵机位置，输出未来100步位置。
    pub fn predict(
        &mut self,
        handeye_rgb: &[u8],
        fixed_rgb: &[u8],
        state: [f32; 6],
        actions: &mut [[f32; ACTION_DIM]; ACTION_STEPS],
    ) -> Result<(), u64> {
        let observation = Observation {
            handeye_rgb,
            fixed_rgb,
            state,
        };
        self.model
            .predict(&observation, self.workspace.as_f32_slice(), actions)
            .map_err(|_| 0x503)
    }

    /// 与`predict`计算完全相同，但把主要网络阶段交给应用记录进度。
    pub fn predict_with_progress<F>(
        &mut self,
        handeye_rgb: &[u8],
        fixed_rgb: &[u8],
        state: [f32; 6],
        actions: &mut [[f32; ACTION_DIM]; ACTION_STEPS],
        progress: F,
    ) -> Result<(), u64>
    where
        F: FnMut(InferenceStage),
    {
        let observation = Observation {
            handeye_rgb,
            fixed_rgb,
            state,
        };
        self.model
            .predict_with_progress(
                &observation,
                self.workspace.as_f32_slice(),
                actions,
                progress,
            )
            .map_err(|_| 0x503)
    }
}

/// 多个物理Frame在连续EL0 VA上的组合映射。
struct MappedWorkspace {
    // 保存Frame/Mapping句柄表达资源所有权；推理期间不能提前UNMAP/FREE。
    _frames: Vec<Frame>,
    _mappings: Vec<Mapping>,
    base: *mut u8,
    bytes: usize,
}

impl MappedWorkspace {
    fn allocate(base: u64, bytes: u64, limit: u64) -> Result<Self, u64> {
        let total_pages = bytes.div_ceil(exo_abi::PAGE_SIZE);
        let workspace_end = base
            .checked_add(total_pages * exo_abi::PAGE_SIZE)
            .ok_or(0x504u64)?;
        if workspace_end > limit || limit > exo_abi::FRAME_ARENA_END {
            return Err(0x504);
        }

        let mut frames = Vec::with_capacity(total_pages.div_ceil(MAX_FRAME_PAGES) as usize);
        let mut mappings = Vec::with_capacity(frames.capacity());
        let mut remaining = total_pages;
        let mut va = base;
        while remaining != 0 {
            let pages = remaining.min(MAX_FRAME_PAGES);
            let frame = match Frame::allocate(pages, 1) {
                Ok(frame) => frame,
                Err(error) => {
                    cleanup(frames, mappings);
                    return Err(error);
                }
            };
            let mapping = match frame.map(0, pages, va, Rights::READ_WRITE) {
                Ok(mapping) => mapping,
                Err(error) => {
                    let _ = frame.free();
                    cleanup(frames, mappings);
                    return Err(error);
                }
            };
            frames.push(frame);
            mappings.push(mapping);
            remaining -= pages;
            va += pages * exo_abi::PAGE_SIZE;
        }

        Ok(Self {
            _frames: frames,
            _mappings: mappings,
            base: base as *mut u8,
            bytes: bytes as usize,
        })
    }

    fn as_f32_slice(&mut self) -> &mut [f32] {
        // SAFETY:Mapping 覆盖 bytes 字节连续 EL0 VA，地址页对齐且为当前
        // VSpace 独占 RW Normal Cacheable；这里仅把临时 arena 解释为 f32。
        unsafe {
            core::slice::from_raw_parts_mut(
                self.base.cast::<f32>(),
                self.bytes / core::mem::size_of::<f32>(),
            )
        }
    }
}

fn cleanup(frames: Vec<Frame>, mappings: Vec<Mapping>) {
    for mapping in mappings.into_iter().rev() {
        let _ = mapping.unmap();
    }
    for frame in frames.into_iter().rev() {
        let _ = frame.free();
    }
}
