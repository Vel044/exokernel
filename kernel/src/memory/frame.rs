//! 普通物理内存 Frame 对象。
//!
//! FrameHandle 只用于建立/撤销映射和最终释放；映射建立后，EL0 直接通过
//! 用户 VA 访问内存。普通 Frame 不暴露 PA，也不承担 MMIO 或 DMA 语义。

use crate::mem;

pub const MAX_FRAMES: usize = 64;
pub const MAX_FRAME_PAGES: u64 = 4096;
pub const MAX_ALIGNMENT_PAGES: u64 = 512;

const MAX_GENERATION: u32 = 0x7fff_ffff;

#[derive(Clone, Copy)]
struct FrameEntry {
    owner: u32,
    generation: u32,
    pa: u64,
    pages: u64,
    mapping_count: u32,
}

impl FrameEntry {
    const EMPTY: Self = Self {
        owner: 0,
        generation: 1,
        pa: 0,
        pages: 0,
        mapping_count: 0,
    };
}

static FRAMES: crate::sync::SpinLock<[FrameEntry; MAX_FRAMES]> =
    crate::sync::SpinLock::new([FrameEntry::EMPTY; MAX_FRAMES]);

fn next_generation(generation: u32) -> u32 {
    if generation >= MAX_GENERATION {
        1
    } else {
        generation + 1
    }
}

fn make_handle(slot: usize, generation: u32) -> u64 {
    ((generation as u64) << 32) | (slot as u64 + 1)
}

fn decode_handle(handle: u64) -> Option<(usize, u32)> {
    let raw_slot = handle as u32;
    let generation = (handle >> 32) as u32;
    if raw_slot == 0 || raw_slot as usize > MAX_FRAMES || generation == 0 {
        return None;
    }
    Some((raw_slot as usize - 1, generation))
}

pub fn allocate(owner: u32, pages: u64, align_pages: u64) -> u64 {
    if owner == 0
        || pages == 0
        || pages > MAX_FRAME_PAGES
        || align_pages == 0
        || align_pages > MAX_ALIGNMENT_PAGES
        || !align_pages.is_power_of_two()
    {
        return exo_abi::SYS_ERR_INVALID;
    }

    // 先在Frame表中保留槽位，再分配物理页。owner使用u32::MAX表示
    // 尚未发布给EL0，其他CPU不能把同一槽位重复用于另一个Frame。
    let (slot, generation) = {
        let mut frames = FRAMES.lock();
        let mut found = None;
        let mut index = 0usize;
        while index < MAX_FRAMES {
            if frames[index].owner == 0 {
                found = Some(index);
                break;
            }
            index += 1;
        }
        let Some(slot) = found else {
            return exo_abi::SYS_ERR_NO_SLOT;
        };
        let generation = frames[slot].generation;
        frames[slot].owner = u32::MAX;
        (slot, generation)
    };
    let Some(pa) = mem::alloc_pages_aligned(pages, align_pages) else {
        FRAMES.lock()[slot].owner = 0;
        return exo_abi::SYS_ERR_NO_MEMORY;
    };

    let Some(bytes) = pages.checked_mul(exo_abi::PAGE_SIZE) else {
        mem::free_pages(pa, pages);
        FRAMES.lock()[slot].owner = 0;
        return exo_abi::SYS_ERR_INVALID;
    };
    unsafe {
        core::ptr::write_bytes(pa as *mut u8, 0, bytes as usize);
    }
    FRAMES.lock()[slot] = FrameEntry {
        owner,
        generation,
        pa,
        pages,
        mapping_count: 0,
    };
    make_handle(slot, generation)
}

pub fn free(owner: u32, handle: u64) -> u64 {
    if owner == 0 {
        return exo_abi::SYS_ERR_NOT_FOUND;
    }
    let Some((slot, generation)) = decode_handle(handle) else {
        return exo_abi::SYS_ERR_INVALID;
    };
    let (pa, pages) = {
        let mut frames = FRAMES.lock();
        let entry = frames[slot];
        if entry.owner != owner || entry.generation != generation {
            return exo_abi::SYS_ERR_NOT_FOUND;
        }
        if entry.mapping_count != 0 {
            return exo_abi::SYS_ERR_BUSY;
        }
        frames[slot] = FrameEntry {
            generation: next_generation(entry.generation),
            ..FrameEntry::EMPTY
        };
        (entry.pa, entry.pages)
    };
    mem::free_pages(pa, pages);
    0
}

pub fn resolve(owner: u32, handle: u64) -> Result<(usize, u64, u64), u64> {
    let Some((slot, generation)) = decode_handle(handle) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    let entry = FRAMES.lock()[slot];
    if entry.owner == 0 || entry.owner != owner || entry.generation != generation {
        return Err(exo_abi::SYS_ERR_NOT_FOUND);
    }
    Ok((slot, entry.pa, entry.pages))
}

pub fn add_mapping(owner: u32, handle: u64) -> Result<(), u64> {
    let Some((slot, generation)) = decode_handle(handle) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    let mut frames = FRAMES.lock();
    let entry = &mut frames[slot];
    if entry.owner != owner || entry.generation != generation {
        return Err(exo_abi::SYS_ERR_NOT_FOUND);
    }
    entry.mapping_count = entry
        .mapping_count
        .checked_add(1)
        .ok_or(exo_abi::SYS_ERR_BUSY)?;
    Ok(())
}

pub fn remove_mapping(owner: u32, handle: u64) -> Result<(), u64> {
    let Some((slot, generation)) = decode_handle(handle) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    let mut frames = FRAMES.lock();
    let entry = &mut frames[slot];
    if entry.owner != owner || entry.generation != generation {
        return Err(exo_abi::SYS_ERR_NOT_FOUND);
    }
    if entry.mapping_count == 0 {
        return Err(exo_abi::SYS_ERR_INVALID);
    }
    entry.mapping_count -= 1;
    Ok(())
}

/// 调用前必须已经撤销该 owner 的全部 Mapping，并完成 TLB invalidation。
pub fn cleanup_owner(owner: u32) {
    if owner == 0 {
        return;
    }
    let mut reclaimed = [(0u64, 0u64); MAX_FRAMES];
    let mut reclaimed_count = 0usize;
    {
        let mut frames = FRAMES.lock();
        let mut slot = 0usize;
        while slot < MAX_FRAMES {
            let entry = frames[slot];
            if entry.owner == owner {
                debug_assert_eq!(entry.mapping_count, 0);
                reclaimed[reclaimed_count] = (entry.pa, entry.pages);
                reclaimed_count += 1;
                frames[slot] = FrameEntry {
                    generation: next_generation(entry.generation),
                    ..FrameEntry::EMPTY
                };
            }
            slot += 1;
        }
    }
    // 先释放Frame表锁，再进入Physical锁，保持对象锁顺序单向。
    for &(pa, pages) in &reclaimed[..reclaimed_count] {
        mem::free_pages(pa, pages);
    }
}
