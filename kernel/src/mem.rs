//! mem.rs —— 物理页分配器
//!
//! 内核最基础的子系统。管理整台机器的空闲 4KB 物理页。
//! 分配器本身不碰硬件, 只管记账——哪些页空闲、哪些已分配。
//!
//! 实现: 静态数组存空闲范围 (FreeRange)。每个范围记录 (基址, 页数)。
//! 分配时从第一个有空闲的范围头部切一页, O(1) 时间。
//! 数据结构来自 main.rs 里遍历 UEFI MemoryMap 得到的 CONVENTIONAL 范围。
//!
//! 当前只分配不回收 (free_page 留空), 后续加合并。

const MAX_RANGES: usize = 64;  // 最多 64 个空闲内存片段

/// 一个连续的空闲物理内存片段
#[derive(Clone, Copy, Default)]
struct FreeRange {
    base: u64,  // 物理基址 (4KB 对齐)
    pages: u64, // 连续空闲 4KB 页数
}

// 静态全局分配池
static mut FREE_RANGES: [FreeRange; MAX_RANGES] = [FreeRange { base: 0, pages: 0 }; MAX_RANGES];
static mut FREE_COUNT: usize = 0;

/// 初始化: 从 UEFI MemoryMap 提取的 (物理基址, 页数) 对塞进分配池
/// 在 main.rs 里 EBS 后调, 遍历 mmap.entries() 只收 CONVENTIONAL 类型的
pub fn init_from_ranges(conventional_ranges: &[(u64, u64)]) {
    unsafe {
        FREE_COUNT = 0;
    }
    for &(base, pages) in conventional_ranges {
        add_range(base, pages);
    }
}

/// 清空分配池。用于 EL1 从 BootInfo 逐项重建 allocator, 避免早期栈上放大数组。
pub fn init_empty() {
    unsafe {
        FREE_COUNT = 0;
    }
}

/// 往分配池追加一个空闲物理范围。
pub fn add_range(base: u64, pages: u64) {
    if pages == 0 {
        return;
    }
    unsafe {
        if FREE_COUNT < MAX_RANGES {
            FREE_RANGES[FREE_COUNT] = FreeRange { base, pages };
            FREE_COUNT += 1;
        }
    }
}

/// 分配一个 4KB 物理页, 返回物理地址
/// 找到第一个 pages > 0 的范围, 拿走它基址那一页, 基址+4096, 页数-1
pub fn alloc_page() -> Option<u64> {
    unsafe {
        for i in 0..FREE_COUNT {
            if FREE_RANGES[i].pages > 0 {
                let addr = FREE_RANGES[i].base;   // 拿走这页
                FREE_RANGES[i].base += 4096;       // 范围基址前进一页
                FREE_RANGES[i].pages -= 1;          // 页数减一
                return Some(addr);
            }
        }
    }
    None  // 没有空闲页了
}

/// 分配连续 N 个 4KB 物理页, 返回首地址
/// 从第一个页数 >= n 的范围头部切 N 页
pub fn alloc_pages(n: u64) -> Option<u64> {
    unsafe {
        for i in 0..FREE_COUNT {
            if FREE_RANGES[i].pages >= n {
                let addr = FREE_RANGES[i].base;
                FREE_RANGES[i].base += n * 4096;  // 基址前进 N 页
                FREE_RANGES[i].pages -= n;          // 页数减 N
                return Some(addr);
            }
        }
    }
    None
}

/// 归还一页 (暂不实现, 后续加相邻范围合并)
pub fn free_page(_paddr: u64) {
    // TODO: 实现合并
}

/// 空闲页总数 (调试用)
pub fn free_pages_total() -> u64 {
    unsafe {
        let mut total = 0u64;
        for i in 0..FREE_COUNT {
            total += FREE_RANGES[i].pages;
        }
        total
    }
}
