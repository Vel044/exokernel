//! VSpace 与普通 Frame Mapping 对象。
//!
//! 一个 VSpace 是一套独立的 EL0 页表，也是第一阶段的资源隔离域。Kernel
//! 不把页表根物理地址或 ASID 交给 EL0；用户只保存 `VSpaceHandle`，所有句柄
//! 都通过 slot + generation 检查所有权和生命周期。
//!
//! 本模块同时保存 Mapping 表。MMIO、DMA 和普通 Frame 仍然是三种不同资源：
//! 这里仅管理普通、可缓存的 Frame 映射，设备和 DMA 继续走各自的系统调用。

use crate::{frame, mmu};
use core::sync::atomic::{AtomicU64, Ordering};

pub const MAX_FRAME_MAPPINGS: usize = 128;
pub const MAX_VSPACES: usize = 8;
const MAX_GENERATION: u32 = 0x7fff_ffff;

const VSPACE_FREE: u8 = 0;
const VSPACE_ACTIVE: u8 = 1;
const VSPACE_DESTROYING: u8 = 2;

#[derive(Clone, Copy)]
struct VSpaceEntry {
    generation: u32,
    state: u8,
    root_pa: u64,
    asid: u16,
    creator_owner: u32,
    active_thread_count: u16,
    mapping_count: u16,
}

impl VSpaceEntry {
    const EMPTY: Self = Self {
        generation: 1,
        state: VSPACE_FREE,
        root_pa: 0,
        asid: 0,
        creator_owner: 0,
        active_thread_count: 0,
        mapping_count: 0,
    };
}

#[derive(Clone, Copy)]
struct MappingEntry {
    owner: u32,
    generation: u32,
    frame_handle: u64,
    frame_pa: u64,
    target_vspace: u64,
    target_root: u64,
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
        target_vspace: 0,
        target_root: 0,
        offset_pages: 0,
        pages: 0,
        va: 0,
        rights: 0,
    };
}

static VSPACES: crate::sync::SpinLock<[VSpaceEntry; MAX_VSPACES]> =
    crate::sync::SpinLock::new([VSpaceEntry::EMPTY; MAX_VSPACES]);
static MAPPINGS: crate::sync::SpinLock<[MappingEntry; MAX_FRAME_MAPPINGS]> =
    crate::sync::SpinLock::new([MappingEntry::EMPTY; MAX_FRAME_MAPPINGS]);

// 映射事务锁把“检查目标PTE、登记Frame/VSpace引用、写入PTE、发布MappingHandle”
// 串成一个原子操作。否则四核同时执行FRAME_MAP_TO时，两个CPU都可能先看到空PTE，
// 随后一个覆盖另一个；它也防止VSPACE_DESTROY在映射尚未发布时释放页表。
static MAP_OPERATION_LOCK: crate::sync::SpinLock<()> = crate::sync::SpinLock::new(());

// 每个CPU各自记录当前 TTBR0 对应的句柄。调度器切换线程时通过这个数组判断
// 是否真的需要写 TTBR0；同一 VSpace 内的线程不会重复执行地址空间切换。
static CURRENT_VSPACES: [AtomicU64; exo_abi::MAX_CPUS] =
    [const { AtomicU64::new(0) }; exo_abi::MAX_CPUS];

// 新建 VSpace 需要保留 EL1 设备映射。它们不是用户授权资源，只是让切换页表
// 后 Kernel 仍能访问自己的 UART/GIC；EL0 不会拿到这些地址的映射权限。
static DEVICE_BLOCKS: crate::sync::SpinLock<([u64; 3], usize)> =
    crate::sync::SpinLock::new(([0; 3], 0));

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
    if raw_slot == 0 || raw_slot as usize > MAX_VSPACES || generation == 0 {
        return None;
    }
    Some((raw_slot as usize - 1, generation))
}

fn mapping_handle(slot: usize, generation: u32) -> u64 {
    ((generation as u64) << 32) | (slot as u64 + 1)
}

fn decode_mapping(handle: u64) -> Option<(usize, u32)> {
    let raw_slot = handle as u32;
    let generation = (handle >> 32) as u32;
    if raw_slot == 0 || raw_slot as usize > MAX_FRAME_MAPPINGS || generation == 0 {
        return None;
    }
    Some((raw_slot as usize - 1, generation))
}

/// 登记启动页表为句柄1，并把 Kernel 需要的设备块复制给后续 VSpace。
pub fn initialize(initial_root: u64, uart_pa: u64, gicd_pa: u64, gicc_pa: u64) {
    let mut blocks = [0u64; 3];
    let mut count = 0usize;
    for pa in [uart_pa, gicd_pa, gicc_pa] {
        let block = pa & !0x1f_ffff;
        if block != 0 && !blocks[..count].contains(&block) {
            blocks[count] = block;
            count += 1;
        }
    }
    *DEVICE_BLOCKS.lock() = (blocks, count);

    let mut spaces = VSPACES.lock();
    spaces[0] = VSpaceEntry {
        generation: spaces[0].generation,
        state: VSPACE_ACTIVE,
        root_pa: initial_root,
        asid: 1,
        creator_owner: 1,
        active_thread_count: 1,
        mapping_count: 0,
    };
    for current in &CURRENT_VSPACES {
        current.store(make_handle(0, spaces[0].generation), Ordering::Release);
    }
}

/// 当前CPU正在运行的 VSpace 句柄。
pub fn current_handle() -> u64 {
    let cpu = crate::arch::aarch64::cpu::id().min(exo_abi::MAX_CPUS - 1);
    CURRENT_VSPACES[cpu].load(Ordering::Acquire)
}

/// 解析句柄并确认它属于指定创建者。
pub fn resolve_for_owner(owner: u32, handle: u64) -> Result<(u64, u16), u64> {
    let Some((slot, generation)) = decode_handle(handle) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    let spaces = VSPACES.lock();
    let entry = spaces[slot];
    if entry.state != VSPACE_ACTIVE || entry.generation != generation {
        return Err(exo_abi::SYS_ERR_NOT_FOUND);
    }
    if entry.creator_owner != owner {
        return Err(exo_abi::SYS_ERR_DENIED);
    }
    Ok((entry.root_pa, entry.asid))
}

/// 获取目标 VSpace 的 Kernel 资源 owner。第一版没有 Capability Transfer，
/// 因此目标空间仍由创建它的 libOS 负责管理。
pub fn resource_owner(owner: u32, handle: u64) -> Result<u32, u64> {
    let Some((slot, generation)) = decode_handle(handle) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    let spaces = VSPACES.lock();
    let entry = spaces[slot];
    if entry.state != VSPACE_ACTIVE || entry.generation != generation {
        return Err(exo_abi::SYS_ERR_NOT_FOUND);
    }
    if entry.creator_owner != owner {
        return Err(exo_abi::SYS_ERR_DENIED);
    }
    Ok(entry.creator_owner)
}

/// 创建空的用户地址空间：共享 EL1 identity map，用户窗口全部为空。
pub fn create(owner: u32) -> u64 {
    if owner == 0 {
        return exo_abi::SYS_ERR_DENIED;
    }
    let (slot, generation) = {
        let mut spaces = VSPACES.lock();
        let Some(slot) = (1..MAX_VSPACES).find(|&slot| spaces[slot].state == VSPACE_FREE) else {
            return exo_abi::SYS_ERR_NO_SLOT;
        };
        let generation = spaces[slot].generation;
        spaces[slot].state = VSPACE_DESTROYING;
        (slot, generation)
    };

    let Some(root) = mmu::try_create_table() else {
        VSPACES.lock()[slot].state = VSPACE_FREE;
        return exo_abi::SYS_ERR_NO_MEMORY;
    };
    // 用户窗口被跳过，保证新地址空间不会继承创建者的代码、heap、MMIO。
    // 运行时创建不能使用启动阶段会panic的map_range_2m，因此这里逐个使用
    // 可失败的block映射；任意一级页表分配失败都释放这棵私有页表树。
    let mut va = 0u64;
    let end = 0x1_8000_0000u64;
    while va < end {
        if !(va >= exo_abi::USER_BASE && va < exo_abi::USER_WINDOW_END)
            && !mmu::try_map_block_2m(root, va, va, mmu::MMU_KERNEL)
        {
            mmu::destroy_table(root);
            VSPACES.lock()[slot].state = VSPACE_FREE;
            return exo_abi::SYS_ERR_NO_MEMORY;
        }
        va += 0x20_0000;
    }
    let (blocks, count) = *DEVICE_BLOCKS.lock();
    for block in blocks[..count].iter().copied() {
        mmu::map_block_2m(root, block, block, mmu::MMU_DEV);
    }
    mmu::flush_el1_tlb();

    let entry = &mut VSPACES.lock()[slot];
    entry.state = VSPACE_ACTIVE;
    entry.root_pa = root;
    // v1使用固定槽位派生ASID，槽位不会在旧空间销毁前复用。
    entry.asid = (slot + 1) as u16;
    entry.creator_owner = owner;
    entry.active_thread_count = 0;
    entry.mapping_count = 0;
    make_handle(slot, generation)
}

pub fn add_thread(handle: u64) -> Result<(), u64> {
    let Some((slot, generation)) = decode_handle(handle) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    let mut spaces = VSPACES.lock();
    let entry = &mut spaces[slot];
    if entry.state != VSPACE_ACTIVE || entry.generation != generation {
        return Err(exo_abi::SYS_ERR_NOT_FOUND);
    }
    entry.active_thread_count = entry
        .active_thread_count
        .checked_add(1)
        .ok_or(exo_abi::SYS_ERR_BUSY)?;
    Ok(())
}

pub fn remove_thread(handle: u64) {
    if let Some((slot, generation)) = decode_handle(handle) {
        let mut spaces = VSPACES.lock();
        let entry = &mut spaces[slot];
        if entry.generation == generation && entry.active_thread_count != 0 {
            entry.active_thread_count -= 1;
        }
    }
}

pub fn add_mapping(handle: u64) -> Result<(), u64> {
    let Some((slot, generation)) = decode_handle(handle) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    let mut spaces = VSPACES.lock();
    let entry = &mut spaces[slot];
    if entry.state != VSPACE_ACTIVE || entry.generation != generation {
        return Err(exo_abi::SYS_ERR_NOT_FOUND);
    }
    entry.mapping_count = entry
        .mapping_count
        .checked_add(1)
        .ok_or(exo_abi::SYS_ERR_BUSY)?;
    Ok(())
}

pub fn remove_mapping(handle: u64) {
    if let Some((slot, generation)) = decode_handle(handle) {
        let mut spaces = VSPACES.lock();
        let entry = &mut spaces[slot];
        if entry.generation == generation && entry.mapping_count != 0 {
            entry.mapping_count -= 1;
        }
    }
}

/// 只有没有线程和 Mapping 的空间才能被销毁，避免强制回收另一个线程正在用的页表。
pub fn destroy(owner: u32, handle: u64) -> u64 {
    let Some((slot, generation)) = decode_handle(handle) else {
        return exo_abi::SYS_ERR_INVALID;
    };
    if slot == 0 {
        return exo_abi::SYS_ERR_DENIED;
    }
    let _operation = MAP_OPERATION_LOCK.lock();
    let root = {
        let mut spaces = VSPACES.lock();
        let entry = spaces[slot];
        if entry.state != VSPACE_ACTIVE || entry.generation != generation {
            return exo_abi::SYS_ERR_NOT_FOUND;
        }
        if entry.creator_owner != owner {
            return exo_abi::SYS_ERR_DENIED;
        }
        if entry.active_thread_count != 0 || entry.mapping_count != 0 {
            return exo_abi::SYS_ERR_BUSY;
        }
        spaces[slot].state = VSPACE_DESTROYING;
        entry.root_pa
    };
    mmu::destroy_table(root);
    mmu::flush_el1_tlb();
    let mut spaces = VSPACES.lock();
    spaces[slot] = VSpaceEntry {
        generation: next_generation(generation),
        ..VSpaceEntry::EMPTY
    };
    0
}

/// 调度器在切换线程时调用。切换不同 VSpace 才写 TTBR0/ASID。
pub fn switch_to(handle: u64) -> Result<(), u64> {
    let (root, asid) = resolve_for_owner(crate::task::current_owner(), handle)?;
    let cpu = crate::arch::aarch64::cpu::id().min(exo_abi::MAX_CPUS - 1);
    if CURRENT_VSPACES[cpu].load(Ordering::Acquire) != handle {
        mmu::switch_to(root, asid);
        CURRENT_VSPACES[cpu].store(handle, Ordering::Release);
    }
    Ok(())
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

/// 固定的 EL0 自建 heap 窗口。EL0 在启动时用 SYS_FRAME_MAP 把普通 Frame
/// 映射到这里，作为全局分配器的 backing 内存。该窗口仍受 range_available
/// 保留，避免 MMIO/DMA 误占；mmu::map_checked 则防止重复覆盖。
fn va_in_heap_window(va: u64, pages: u64) -> bool {
    if pages == 0 || (va & (exo_abi::PAGE_SIZE - 1)) != 0 {
        return false;
    }
    let Some(bytes) = pages.checked_mul(exo_abi::PAGE_SIZE) else {
        return false;
    };
    let Some(end) = va.checked_add(bytes) else {
        return false;
    };
    va >= exo_abi::USER_HEAP_BASE && end <= exo_abi::USER_HEAP_BASE + exo_abi::USER_HEAP_SIZE
}

fn va_in_process_window(va: u64, pages: u64) -> bool {
    if pages == 0 || (va & (exo_abi::PAGE_SIZE - 1)) != 0 {
        return false;
    }
    let Some(bytes) = pages.checked_mul(exo_abi::PAGE_SIZE) else {
        return false;
    };
    let Some(end) = va.checked_add(bytes) else {
        return false;
    };
    // 目标进程可以把代码、数据和栈放进普通用户窗口；Frame smoke 使用
    // 独立 frame arena，因此旧 FRAME_MAP 的限制仍然保留。
    (va >= exo_abi::USER_BASE && end <= exo_abi::USER_WINDOW_END)
        || (va >= exo_abi::FRAME_ARENA_BASE && end <= exo_abi::FRAME_ARENA_END)
}

/// 把普通 Frame 映射到当前线程所属的 VSpace，保持旧 ABI 兼容。
pub fn map_frame(
    owner: u32,
    frame_handle: u64,
    offset_pages: u64,
    pages: u64,
    va: u64,
    rights: u64,
) -> u64 {
    map_frame_inner(
        owner,
        current_handle(),
        frame_handle,
        offset_pages,
        pages,
        va,
        rights,
        true,
    )
}

/// 把调用者拥有的 Frame 映射到它创建的另一个 VSpace。
pub fn map_frame_to(
    owner: u32,
    target_vspace: u64,
    frame_handle: u64,
    offset_pages: u64,
    pages: u64,
    va: u64,
    rights: u64,
) -> u64 {
    map_frame_inner(
        owner,
        target_vspace,
        frame_handle,
        offset_pages,
        pages,
        va,
        rights,
        false,
    )
}

fn map_frame_inner(
    owner: u32,
    target_vspace: u64,
    frame_handle: u64,
    offset_pages: u64,
    pages: u64,
    va: u64,
    rights: u64,
    current_arena_only: bool,
) -> u64 {
    let _operation = MAP_OPERATION_LOCK.lock();
    if owner == 0
        || if current_arena_only {
            !(va_in_frame_arena(va, pages) || va_in_heap_window(va, pages))
        } else {
            !va_in_process_window(va, pages)
        }
    {
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
    let Ok((root, _)) = resolve_for_owner(owner, target_vspace) else {
        return exo_abi::SYS_ERR_NOT_FOUND;
    };

    // W^X检查、VA冲突检查和槽位保留在同一锁临界区内，防止并发调用制造
    // 同一 Frame 的 RW/RX别名或重复占用同一个 MappingHandle。
    let (slot, generation) = {
        let mut mappings = MAPPINGS.lock();
        let wants_write = rights & exo_abi::FRAME_RIGHT_WRITE != 0;
        let wants_execute = rights & exo_abi::FRAME_RIGHT_EXECUTE != 0;
        let mut found = None;
        for index in 0..MAX_FRAME_MAPPINGS {
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
        }
        let Some(slot) = found else {
            return exo_abi::SYS_ERR_NO_SLOT;
        };
        let generation = mappings[slot].generation;
        mappings[slot].owner = u32::MAX;
        (slot, generation)
    };

    let pa = frame_pa + offset_pages * exo_abi::PAGE_SIZE;
    if rights & exo_abi::FRAME_RIGHT_EXECUTE != 0 {
        mmu::sync_for_exec(pa, pages);
    }
    // 先登记引用，再写PTE。这样另一个CPU即使并发请求FREE/DESTROY，也会
    // 看到mapping_count非零而返回BUSY；下面任何失败路径都会撤销这两个引用。
    if frame::add_mapping(owner, frame_handle).is_err() {
        MAPPINGS.lock()[slot].owner = 0;
        return exo_abi::SYS_ERR_INVALID;
    }
    if add_mapping(target_vspace).is_err() {
        let _ = frame::remove_mapping(owner, frame_handle);
        MAPPINGS.lock()[slot].owner = 0;
        return exo_abi::SYS_ERR_BUSY;
    }
    match mmu::map_checked(root, va, pa, flags, pages) {
        Ok(()) => {}
        Err(mmu::MapError::Conflict) => {
            remove_mapping(target_vspace);
            let _ = frame::remove_mapping(owner, frame_handle);
            MAPPINGS.lock()[slot].owner = 0;
            return exo_abi::SYS_ERR_CONFLICT;
        }
        Err(mmu::MapError::NoMemory) => {
            remove_mapping(target_vspace);
            let _ = frame::remove_mapping(owner, frame_handle);
            MAPPINGS.lock()[slot].owner = 0;
            return exo_abi::SYS_ERR_NO_MEMORY;
        }
    }
    MAPPINGS.lock()[slot] = MappingEntry {
        owner,
        generation,
        frame_handle,
        frame_pa,
        target_vspace,
        target_root: root,
        offset_pages,
        pages,
        va,
        rights,
    };
    mapping_handle(slot, generation)
}

pub fn unmap_handle(owner: u32, mapping_handle_value: u64) -> u64 {
    let Some((slot, generation)) = decode_mapping(mapping_handle_value) else {
        return exo_abi::SYS_ERR_INVALID;
    };
    let _operation = MAP_OPERATION_LOCK.lock();
    let entry = {
        let mut mappings = MAPPINGS.lock();
        let entry = mappings[slot];
        if entry.owner != owner || entry.generation != generation {
            return exo_abi::SYS_ERR_NOT_FOUND;
        }
        mappings[slot].owner = u32::MAX;
        entry
    };
    let expected_pa = entry.frame_pa + entry.offset_pages * exo_abi::PAGE_SIZE;
    if !mmu::unmap_matching(entry.target_root, entry.va, expected_pa, entry.pages) {
        MAPPINGS.lock()[slot].owner = entry.owner;
        return exo_abi::SYS_ERR_CONFLICT;
    }
    mmu::flush_el1_tlb_range(entry.va, entry.pages);
    if frame::remove_mapping(owner, entry.frame_handle).is_err() {
        MAPPINGS.lock()[slot].owner = entry.owner;
        return exo_abi::SYS_ERR_INVALID;
    }
    remove_mapping(entry.target_vspace);
    MAPPINGS.lock()[slot] = MappingEntry {
        generation: next_generation(entry.generation),
        ..MappingEntry::EMPTY
    };
    0
}

/// 退出/故障清理时按每条 Mapping 自己记录的目标根撤销，而不是错误地使用
/// 触发退出的 CPU 当前根。
pub fn cleanup_owner(owner: u32, _root: u64) {
    if owner == 0 {
        return;
    }
    let _operation = MAP_OPERATION_LOCK.lock();
    let mut removed = [MappingEntry::EMPTY; MAX_FRAME_MAPPINGS];
    let mut removed_count = 0usize;
    {
        let mut mappings = MAPPINGS.lock();
        for slot in 0..MAX_FRAME_MAPPINGS {
            let entry = mappings[slot];
            if entry.owner == owner {
                removed[removed_count] = entry;
                removed_count += 1;
                mappings[slot] = MappingEntry {
                    generation: next_generation(entry.generation),
                    ..MappingEntry::EMPTY
                };
            }
        }
    }
    for entry in &removed[..removed_count] {
        mmu::unmap(entry.target_root, entry.va, entry.pages);
        let _ = frame::remove_mapping(owner, entry.frame_handle);
        remove_mapping(entry.target_vspace);
    }
}
