//! protect.rs —— MMIO 保护表
//!
//! 物理地址范围准入表。这是外核保护MMIO的第一层数据结构。
//!
//! 当 EL0 请求映射某个 MMIO 物理地址时:
//!   ① SVC 陷入 EL1
//!   ② EL1 查这张表: 这个地址是否是已登记设备窗口?
//!      - 归你 → 允许映射, 填充页表, eret 回去
//!      - 不归你 / 无主 → 拒绝, 杀进程
//!
//! DTB解析完成后，每个设备的reg范围由register()登记；当前任务的具体
//! 所有权由task中的MMIO grant另行记录，两层校验不能合并。

const MAX_REGIONS: usize = 128; // 最多登记 128 个 MMIO 区域
const MAX_DENIED_REGIONS: usize = 8;

/// 一个 MMIO 物理地址区域
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct MmioRegion {
    pub base: u64, // 物理基址
    pub size: u64, // 字节数
}

// 全局 MMIO 保护表
static mut MMIO_TABLE: [MmioRegion; MAX_REGIONS] = [MmioRegion { base: 0, size: 0 }; MAX_REGIONS];
static mut MMIO_COUNT: usize = 0;
static mut DENIED_TABLE: [MmioRegion; MAX_DENIED_REGIONS] =
    [MmioRegion { base: 0, size: 0 }; MAX_DENIED_REGIONS];
static mut DENIED_COUNT: usize = 0;

/// 登记一片 MMIO 区域 (来自 DTB 扫描)
/// 先把所有设备的地址范围登记进去, 后续再 assign 给具体进程
pub fn register(base: u64, size: u64) {
    if size == 0 {
        return;
    }
    let page_base = base & !0xfff;
    let Some(end) = base.checked_add(size) else {
        return;
    };
    let page_end = (end + 4095) & !4095;
    unsafe {
        if MMIO_COUNT < MAX_REGIONS {
            MMIO_TABLE[MMIO_COUNT].base = page_base;
            MMIO_TABLE[MMIO_COUNT].size = page_end - page_base;
            MMIO_COUNT += 1;
        }
    }
}

/// 登记 EL1 专用 MMIO。即使通用 DTB 扫描随后把它登记为设备窗口，
/// `lookup()` 也会优先拒绝，防止 EL0 映射 GIC 等保护硬件。
pub fn deny(base: u64, size: u64) {
    unsafe {
        if DENIED_COUNT < MAX_DENIED_REGIONS {
            DENIED_TABLE[DENIED_COUNT] = MmioRegion { base, size };
            DENIED_COUNT += 1;
        }
    }
}

/// 检查 [base, base + size) 是否完全落在某个已登记 MMIO 区域内。
pub fn contains_range(base: u64, size: u64) -> bool {
    if size == 0 {
        return false;
    }
    let Some(end) = base.checked_add(size) else {
        return false;
    };

    unsafe {
        for i in 0..DENIED_COUNT {
            let r = &DENIED_TABLE[i];
            let region_end = r.base.saturating_add(r.size);
            if base < region_end && end > r.base {
                return false;
            }
        }
        for i in 0..MMIO_COUNT {
            let r = &MMIO_TABLE[i];
            let Some(region_end) = r.base.checked_add(r.size) else {
                continue;
            };
            if base >= r.base && end <= region_end {
                return true;
            }
        }
    }
    false
}
