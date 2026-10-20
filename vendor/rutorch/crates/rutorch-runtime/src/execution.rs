//! 与具体操作系统无关的 Tensor、内存槽和静态执行计划。
//!
//! 本模块只描述连续内存上的视图和加载时已经冻结的槽位；Linux 与 LibOS
//! 必须编译同一份计划。文件读取、页映射和线程创建不属于这里。

use crate::Error;

/// Tensor 的物理布局。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TensorLayout {
    /// 普通连续行优先矩阵。
    RowMajor,
    /// 图像像素连续、通道为最内层维度。
    Nhwc,
    /// 卷积权重的 output/input/height/width 顺序。
    Oihw,
}

/// 最多四维的已验证 shape。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shape {
    dimensions: [usize; 4],
    rank: u8,
}

impl Shape {
    /// 构造 shape，并检查元素数不会溢出。
    pub fn new(dimensions: &[usize]) -> Result<Self, Error> {
        if dimensions.is_empty() || dimensions.len() > 4 {
            return Err(Error::ShapeMismatch);
        }
        let mut output = [1; 4];
        let mut elements = 1usize;
        for (index, dimension) in dimensions.iter().copied().enumerate() {
            if dimension == 0 {
                return Err(Error::ShapeMismatch);
            }
            elements = elements
                .checked_mul(dimension)
                .ok_or(Error::ShapeMismatch)?;
            output[index] = dimension;
        }
        let _ = elements;
        Ok(Self {
            dimensions: output,
            rank: dimensions.len() as u8,
        })
    }

    /// 返回有效维度。
    pub fn dimensions(&self) -> &[usize] {
        &self.dimensions[..usize::from(self.rank)]
    }

    /// 返回总元素数。
    pub fn elements(&self) -> usize {
        self.dimensions().iter().copied().product()
    }
}

/// 只读 Tensor 视图；不拥有底层模型或 arena 内存。
#[derive(Clone, Copy)]
pub struct TensorView<'a> {
    values: &'a [f32],
    shape: Shape,
    layout: TensorLayout,
}

impl<'a> TensorView<'a> {
    /// 在已验证的连续切片上建立视图。
    pub fn new(values: &'a [f32], shape: Shape, layout: TensorLayout) -> Result<Self, Error> {
        if values.len() != shape.elements() {
            return Err(Error::ShapeMismatch);
        }
        Ok(Self {
            values,
            shape,
            layout,
        })
    }

    /// 返回底层连续值。
    pub fn values(&self) -> &'a [f32] {
        self.values
    }

    /// 返回 shape。
    pub const fn shape(&self) -> Shape {
        self.shape
    }

    /// 返回布局。
    pub const fn layout(&self) -> TensorLayout {
        self.layout
    }
}

/// 可写 Tensor 视图；借用期间独占底层 arena 槽位。
pub struct TensorViewMut<'a> {
    values: &'a mut [f32],
    shape: Shape,
    layout: TensorLayout,
}

impl<'a> TensorViewMut<'a> {
    /// 在已验证的连续可写切片上建立视图。
    pub fn new(values: &'a mut [f32], shape: Shape, layout: TensorLayout) -> Result<Self, Error> {
        if values.len() != shape.elements() {
            return Err(Error::ShapeMismatch);
        }
        Ok(Self {
            values,
            shape,
            layout,
        })
    }

    /// 返回可写连续值。
    pub fn values_mut(&mut self) -> &mut [f32] {
        self.values
    }

    /// 返回 shape。
    pub const fn shape(&self) -> Shape {
        self.shape
    }

    /// 返回布局。
    pub const fn layout(&self) -> TensorLayout {
        self.layout
    }
}

/// 静态计划中的整数槽号。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlotId(pub u16);

/// 一个预计算的 f32 arena 槽位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferSlot {
    /// 整数槽号，稳态执行不再使用字符串查找。
    pub id: SlotId,
    /// 从 workspace 起点开始的 f32 偏移。
    pub offset: usize,
    /// 槽位的 f32 元素数。
    pub len: usize,
    /// 最后读取该槽位的计划阶段，用于验证复用边界。
    pub last_use: u16,
}

/// Linux 与 LibOS 共用的只读编译计划。
#[derive(Clone, Copy, Debug)]
pub struct CompiledExecutionPlan {
    /// 计划格式版本。
    pub version: u32,
    /// 规范描述的 SHA-256。
    pub sha256: [u8; 32],
    /// 固定槽位表。
    pub slots: &'static [BufferSlot],
    /// 完整推理所需 f32 workspace。
    pub workspace_floats: usize,
}

const ACT_SLOTS: [BufferSlot; 5] = [
    BufferSlot {
        id: SlotId(0),
        offset: 0,
        len: 246_784,
        last_use: 15,
    },
    BufferSlot {
        id: SlotId(1),
        offset: 246_784,
        len: 246_784,
        last_use: 15,
    },
    BufferSlot {
        id: SlotId(2),
        offset: 493_568,
        len: 51_200,
        last_use: 17,
    },
    BufferSlot {
        id: SlotId(3),
        offset: 544_768,
        len: 51_200,
        last_use: 16,
    },
    BufferSlot {
        id: SlotId(4),
        offset: 595_968,
        len: 16_404_032,
        last_use: 17,
    },
];

/// ACT v2 固定执行计划。Scratch 内部仍按 mark/reset 做确定性子分配，但所有
/// 顶层生命周期和最大容量已经在加载前冻结。
pub static ACT_EXECUTION_PLAN: CompiledExecutionPlan = CompiledExecutionPlan {
    version: 2,
    sha256: [
        0x89, 0x70, 0x15, 0xed, 0x78, 0x9c, 0x00, 0x98, 0x78, 0x46, 0x5c, 0x0f, 0x8e, 0xfb, 0xb1,
        0x76, 0xf0, 0x37, 0xf4, 0x5e, 0x36, 0xea, 0xa9, 0xed, 0xc8, 0xa3, 0xda, 0x36, 0x43, 0xf4,
        0x19, 0x1c,
    ],
    slots: &ACT_SLOTS,
    workspace_floats: 17_000_000,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn act_slots_fit_without_overlap() {
        let mut previous_end = 0;
        for slot in ACT_EXECUTION_PLAN.slots {
            assert!(slot.offset >= previous_end);
            previous_end = slot.offset + slot.len;
        }
        assert_eq!(previous_end, ACT_EXECUTION_PLAN.workspace_floats);
    }

    #[test]
    fn tensor_view_checks_element_count() {
        let shape = Shape::new(&[2, 3]).unwrap();
        assert!(TensorView::new(&[0.0; 6], shape, TensorLayout::RowMajor).is_ok());
        assert!(TensorView::new(&[0.0; 5], shape, TensorLayout::RowMajor).is_err());
    }
}
