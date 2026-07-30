//! 普通 Frame 到当前 EL0 VSpace 的映射对象。
//!
//! MappingHandle 精确标识一条映射，避免 unmap 时再次信任 EL0 提供的
//! VA/size。普通 Frame 始终使用 Normal Cacheable 属性。

use crate::{frame, mmu};

pub const MAX_FRAME_MAPPINGS: usize = 128;
const MAX_GENERATION: u32 = 0x7fff_ffff;

#[derive(Clone, Copy)]
struct MappingEntry {
    owner: u32,
    generation: u32,
    frame_handle: u64,
    frame_pa: u64,
    offset_pages: u64,
    pages: u64,
    va: u64,
    rights: u64,
}

impl MappingEntry {
    const EMPTY: Self = Self {
        owner: 0,
        generation: 1,
        frame_handle: 0,
        frame_pa: 0,
        offset_pages: 0,
        pages: 0,
        va: 0,
        rights: 0,
    };
}

static MAPPINGS: crate::sync::SpinLock<[MappingEntry; MAX_FRAME_MAPPINGS]> =
    crate::sync::SpinLock::new([MappingEntry::EMPTY; MAX_FRAME_MAPPINGS]);

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
    if raw_slot == 0 || raw_slot as usize > MAX_FRAME_MAPPINGS || generation == 0 {
        return None;
    }
    Some((raw_slot as usize - 1, generation))
}

fn mapping_flags(rights: u64) -> Option<u64> {
    match rights {
        exo_abi::FRAME_RIGHT_READ => Some(mmu::MMU_USER_RO),
        value if value == exo_abi::FRAME_RIGHT_READ | exo_abi::FRAME_RIGHT_WRITE => {
            Some(mmu::MMU_USER_RW)
        }
        value if value == exo_abi::FRAME_RIGHT_READ | exo_abi::FRAME_RIGHT_EXECUTE => {
            Some(mmu::MMU_USER_RX)
        }
        _ => None,
    }
}

fn va_in_frame_arena(va: u64, pages: u64) -> bool {
    if pages == 0 || (va & (exo_abi::PAGE_SIZE - 1)) != 0 {
        return false;
    }
    let Some(bytes) = pages.checked_mul(exo_abi::PAGE_SIZE) else {
        return false;
    };
    let Some(end) = va.checked_add(bytes) else {
        return false;
    };
    va >= exo_abi::FRAME_ARENA_BASE && end <= exo_abi::FRAME_ARENA_END
}

pub fn map_frame(
    owner: u32,
    frame_handle: u64,
    offset_pages: u64,
    pages: u64,
    va: u64,
    rights: u64,
) -> u64 {
    if owner == 0 || !va_in_frame_arena(va, pages) {
        return exo_abi::SYS_ERR_INVALID;
    }
    let Some(flags) = mapping_flags(rights) else {
        return exo_abi::SYS_ERR_DENIED;
    };
    let Ok((_, frame_pa, frame_pages)) = frame::resolve(owner, frame_handle) else {
        return exo_abi::SYS_ERR_NOT_FOUND;
    };
    let Some(end_page) = offset_pages.checked_add(pages) else {
        return exo_abi::SYS_ERR_INVALID;
    };
    if end_page > frame_pages {
        return exo_abi::SYS_ERR_INVALID;
    }

    // W^X检查与槽位保留必须在同一个锁临界区内，否则两个CPU可以同时
    // 为同一Frame建立RW和RX别名并分别通过检查。
    let (slot, generation) = {
        let mut mappings = MAPPINGS.lock();
        let wants_write = rights & exo_abi::FRAME_RIGHT_WRITE != 0;
        let wants_execute = rights & exo_abi::FRAME_RIGHT_EXECUTE != 0;
        let mut found = None;
        let mut index = 0usize;
        while index < MAX_FRAME_MAPPINGS {
            let mapping = mappings[index];
            if mapping.owner != 0 && mapping.frame_handle == frame_handle {
                let has_write = mapping.rights & exo_abi::FRAME_RIGHT_WRITE != 0;
                let has_execute = mapping.rights & exo_abi::FRAME_RIGHT_EXECUTE != 0;
                if (wants_write && has_execute) || (wants_execute && has_write) {
                    return exo_abi::SYS_ERR_DENIED;
                }
            }
            if mapping.owner == 0 && found.is_none() {
                found = Some(index);
            }
            index += 1;
        }
        let Some(slot) = found else {
            return exo_abi::SYS_ERR_NO_SLOT;
        };
        let generation = mappings[slot].generation;
        mappings[slot].owner = u32::MAX;
        (slot, generation)
    };
    let pa = frame_pa + offset_pages * exo_abi::PAGE_SIZE;
    let root = mmu::active_table();
    if root == 0 {
        MAPPINGS.lock()[slot].owner = 0;
        return exo_abi::SYS_ERR_INVALID;
    }
    if rights & exo_abi::FRAME_RIGHT_EXECUTE != 0 {
        mmu::sync_for_exec(pa, pages);
    }
    match mmu::map_checked(root, va, pa, flags, pages) {
        Ok(()) => {}
        Err(mmu::MapError::Conflict) => {
            MAPPINGS.lock()[slot].owner = 0;
            return exo_abi::SYS_ERR_CONFLICT;
        }
        Err(mmu::MapError::NoMemory) => {
            MAPPINGS.lock()[slot].owner = 0;
            return exo_abi::SYS_ERR_NO_MEMORY;
        }
    }
    if let Err(error) = frame::add_mapping(owner, frame_handle) {
        mmu::unmap(root, va, pages);
        mmu::flush_el1_tlb_range(va, pages);
        MAPPINGS.lock()[slot].owner = 0;
        return error;
    }

    MAPPINGS.lock()[slot] = MappingEntry {
        owner,
        generation,
        frame_handle,
        frame_pa,
        offset_pages,
        pages,
        va,
        rights,
    };
    make_handle(slot, generation)
}

pub fn unmap_handle(owner: u32, mapping_handle: u64) -> u64 {
    let Some((slot, generation)) = decode_handle(mapping_handle) else {
        return exo_abi::SYS_ERR_INVALID;
    };
    let entry = {
        let mut mappings = MAPPINGS.lock();
        let entry = mappings[slot];
        if entry.owner == 0 || entry.owner != owner || entry.generation != generation {
            return exo_abi::SYS_ERR_NOT_FOUND;
        }
        // 暂时保留该槽，阻止另一CPU重复UNMAP或在完成前复用。
        mappings[slot].owner = u32::MAX;
        entry
    };

    let expected_pa = entry.frame_pa + entry.offset_pages * exo_abi::PAGE_SIZE;
    let root = mmu::active_table();
    if !mmu::unmap_matching(root, entry.va, expected_pa, entry.pages) {
        MAPPINGS.lock()[slot].owner = entry.owner;
        return exo_abi::SYS_ERR_CONFLICT;
    }
    mmu::flush_el1_tlb_range(entry.va, entry.pages);
    if frame::remove_mapping(owner, entry.frame_handle).is_err() {
        MAPPINGS.lock()[slot].owner = entry.owner;
        return exo_abi::SYS_ERR_INVALID;
    }

    MAPPINGS.lock()[slot] = MappingEntry {
        generation: next_generation(entry.generation),
        ..MappingEntry::EMPTY
    };
    0
}

/// 退出路径使用：撤销owner的全部Frame映射，但由调用者统一执行TLBI。
pub fn cleanup_owner(owner: u32, root: u64) {
    if owner == 0 {
        return;
    }
    let mut removed = [MappingEntry::EMPTY; MAX_FRAME_MAPPINGS];
    let mut removed_count = 0usize;
    {
        let mut mappings = MAPPINGS.lock();
        let mut slot = 0usize;
        while slot < MAX_FRAME_MAPPINGS {
            let entry = mappings[slot];
            if entry.owner == owner {
                removed[removed_count] = entry;
                removed_count += 1;
                mappings[slot] = MappingEntry {
                    generation: next_generation(entry.generation),
                    ..MappingEntry::EMPTY
                };
            }
            slot += 1;
        }
    }
    // 不持VSpace表锁调用MMU和Frame，避免扩大IRQ关闭临界区。
    for entry in &removed[..removed_count] {
        mmu::unmap(root, entry.va, entry.pages);
        let _ = frame::remove_mapping(owner, entry.frame_handle);
    }
}
