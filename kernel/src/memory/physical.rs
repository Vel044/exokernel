//! mem.rs —— 物理页分配器
//!
//! 内核最基础的子系统。管理整台机器的空闲 4KB 物理页。
//! 分配器本身不碰硬件, 只管记账——哪些页空闲、哪些已分配。
//!
//! 实现: 静态数组存空闲范围 (FreeRange)。每个范围记录 (基址, 页数)。
//! 分配时从第一个有空闲的范围头部切一页, O(1) 时间。
//! 数据结构来自 main.rs 里遍历 UEFI MemoryMap 得到的 CONVENTIONAL 范围。
//!
//! 释放时把范围重新插入数组，并合并相邻或重叠范围。

const MAX_RANGES: usize = 64; // 最多 64 个空闲内存片段

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
                let addr = FREE_RANGES[i].base; // 拿走这页
                FREE_RANGES[i].base += 4096; // 范围基址前进一页
                FREE_RANGES[i].pages -= 1; // 页数减一
                return Some(addr);
            }
        }
    }
    None // 没有空闲页了
}

/// 分配连续 N 个 4KB 物理页, 返回首地址
/// 从第一个页数 >= n 的范围头部切 N 页
pub fn alloc_pages(n: u64) -> Option<u64> {
    unsafe {
        for i in 0..FREE_COUNT {
            if FREE_RANGES[i].pages >= n {
                let addr = FREE_RANGES[i].base;
                FREE_RANGES[i].base += n * 4096; // 基址前进 N 页
                FREE_RANGES[i].pages -= n; // 页数减 N
                return Some(addr);
            }
        }
    }
    None
}

/// 分配连续且按 align_pages 对齐的 N 页。
pub fn alloc_pages_aligned(n: u64, align_pages: u64) -> Option<u64> {
    if n == 0 || align_pages == 0 || !align_pages.is_power_of_two() {
        return None;
    }
    let alignment = align_pages * 4096;
    unsafe {
        for i in 0..FREE_COUNT {
            let range = FREE_RANGES[i];
            let aligned = (range.base + alignment - 1) & !(alignment - 1);
            let prefix_pages = (aligned - range.base) / 4096;
            if prefix_pages + n > range.pages {
                continue;
            }
            let suffix_pages = range.pages - prefix_pages - n;
            if prefix_pages == 0 {
                FREE_RANGES[i].base = aligned + n * 4096;
                FREE_RANGES[i].pages = suffix_pages;
            } else {
                FREE_RANGES[i].pages = prefix_pages;
                if suffix_pages != 0 {
                    if FREE_COUNT >= MAX_RANGES {
                        return None;
                    }
                    FREE_RANGES[FREE_COUNT] = FreeRange {
                        base: aligned + n * 4096,
                        pages: suffix_pages,
                    };
                    FREE_COUNT += 1;
                }
            }
            return Some(aligned);
        }
    }
    None
}

/// 归还连续物理页，并与已有空闲范围合并。
///
/// 调用者必须保证这些页确实由分配器分配、当前没有映射在任何可运行地址空间，
/// 且不会被重复释放。当前内核是单核 bring-up 版本，因此暂时不加锁。
pub fn free_pages(paddr: u64, pages: u64) {
    if pages == 0 {
        return;
    }

    unsafe {
        if FREE_COUNT >= MAX_RANGES {
            panic!("mem: free range table full");
        }

        FREE_RANGES[FREE_COUNT] = FreeRange { base: paddr, pages };
        FREE_COUNT += 1;

        // 范围数量很小，使用 O(n^2) 合并可以保持实现简单且不依赖堆。
        let mut i = 0usize;
        while i < FREE_COUNT {
            let mut j = i + 1;
            while j < FREE_COUNT {
                let a_start = FREE_RANGES[i].base;
                let a_end = a_start + FREE_RANGES[i].pages * 4096;
                let b_start = FREE_RANGES[j].base;
                let b_end = b_start + FREE_RANGES[j].pages * 4096;

                if a_start <= b_end && b_start <= a_end {
                    let start = if a_start < b_start { a_start } else { b_start };
                    let end = if a_end > b_end { a_end } else { b_end };
                    FREE_RANGES[i] = FreeRange {
                        base: start,
                        pages: (end - start) / 4096,
                    };
                    FREE_COUNT -= 1;
                    FREE_RANGES[j] = FREE_RANGES[FREE_COUNT];
                    FREE_RANGES[FREE_COUNT] = FreeRange { base: 0, pages: 0 };
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }
}

pub fn free_page(paddr: u64) {
    free_pages(paddr, 1);
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
