//! protect.rs —— MMIO 保护表
//!
//! 物理地址范围 → 谁拥有它。这是外核"准入控制"的核心数据结构。
//!
//! 当 EL0 请求映射某个 MMIO 物理地址时:
//!   ① SVC 陷入 EL1
//!   ② EL1 查这张表: 这个地址是否是已登记设备窗口?
//!      - 归你 → 允许映射, 填充页表, eret 回去
//!      - 不归你 / 无主 → 拒绝, 杀进程
//!
//! DTB 解析完成后, 每个设备的 reg 范围内的地址被 register() 登记,
//! 后续 assign() 分配所有权给某个进程。

const MAX_REGIONS: usize = 128;  // 最多登记 128 个 MMIO 区域

/// 一个 MMIO 物理地址区域
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct MmioRegion {
    pub base: u64,   // 物理基址
    pub size: u64,   // 字节数
    pub owner: u32,  // 所有者进程 ID (0 = 未分配)
}

// 全局 MMIO 保护表
static mut MMIO_TABLE: [MmioRegion; MAX_REGIONS] = [MmioRegion {
    base: 0,
    size: 0,
    owner: 0,
}; MAX_REGIONS];
static mut MMIO_COUNT: usize = 0;

/// 登记一片 MMIO 区域 (来自 DTB 扫描)
/// 先把所有设备的地址范围登记进去, 后续再 assign 给具体进程
pub fn register(base: u64, size: u64) {
    unsafe {
        if MMIO_COUNT < MAX_REGIONS {
            MMIO_TABLE[MMIO_COUNT].base = base;
            MMIO_TABLE[MMIO_COUNT].size = size;
            MMIO_TABLE[MMIO_COUNT].owner = 0;  // 初始无主
            MMIO_COUNT += 1;
        }
    }
}

/// 按物理地址查所有者
/// 遍历整张表, 如果 paddr 落在某个已注册的范围内, 返回其 owner
pub fn lookup(paddr: u64) -> Option<u32> {
    unsafe {
        for i in 0..MMIO_COUNT {
            let r = &MMIO_TABLE[i];
            if paddr >= r.base && paddr < r.base + r.size {
                return Some(r.owner);
            }
        }
    }
    None  // 不在任何已注册 MMIO 范围内 (可能是一般物理内存或未注册的外设)
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
