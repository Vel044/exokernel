//! mmu.rs —— EL1 stage-1 页表管理
//!
//! EL1 外核管理 stage-1 页表, 同时约束 EL0 libOS 能访问哪些 VA。
//!
//! ARMv8 4KB granule 页表结构:
//!   虚拟地址 VA[47:0] 分 3 级翻译:
//!     L1: VA[38:30]  → 512 个表项, 每个覆盖 1GB
//!     L2: VA[29:21]  → 512 个表项, 每个覆盖 2MB
//!     L3: VA[20:12]  → 512 个表项, 每个映射一个 4KB 页
//!
//! 当前 EL2 的 MMU 由固件管理 (identity map), 我们直接用物理地址
//! 读写页表 (不在 EL2 的虚拟地址空间里)。
//!
use crate::mem;
use core::sync::atomic::{AtomicU64, Ordering};

// ── 页表条目格式 ──
const DESC_TABLE: u64 = 0b11; // L1/L2 表项的 bit[1:0]=11: 指向下级表
const DESC_BLOCK: u64 = 0b01; // L1/L2 表项的 bit[1:0]=01: block 映射
const DESC_PAGE: u64 = 0b11; // L3 表项的 bit[1:0]=11: 指向一个 4KB 物理页
const OUTPUT_ADDRESS_MASK: u64 = 0x0000_ffff_ffff_f000;

// ── 内存属性 (对应 MAIR_EL1 的索引) ──
pub const ATTR_NORMAL: u64 = 0; // 索引 0 = 普通内存 (Normal WB-WA, 可缓存)
pub const ATTR_DEVICE: u64 = 1; // 索引 1 = 设备内存 (Device-nGnRE, 不可缓存, MMIO 用)
pub const ATTR_NORMAL_NC: u64 = 2; // 索引 2 = 普通不可缓存内存 (DMA v1 用)
                                   // SH[9:8]=0b11表示Inner Shareable。四核共享Thread表、Ready Queue、页表和
                                   // EL0普通RAM时必须使用该属性，才能让原子操作、缓存一致性和ISH barrier
                                   // 位于同一个共享域；只标Normal Cacheable而SH=0在SMP下是不完整的。
const SH_INNER: u64 = 0b11 << 8;
// Device寄存器本身不可缓存，但多个CPU可能访问同一个GIC Distributor，
// 因而用Outer Shareable表达系统级设备观察顺序。
const SH_OUTER: u64 = 0b10 << 8;

// ── 访问权限 ──
pub const AP_RW_EL1: u64 = 0 << 6; // EL1 可读写, EL0 不可访问
pub const AP_RW_EL0: u64 = 1 << 6; // EL1/EL0 都可读写
pub const AP_RO_EL0: u64 = 3 << 6; // EL1/EL0 都只读

// ── 预设的映射标志组合 ──
pub const MMU_KERNEL: u64 = AP_RW_EL1 | (ATTR_NORMAL << 2) | SH_INNER | (1 << 10) | (1 << 54);
pub const MMU_USER_RX: u64 = AP_RO_EL0 | (ATTR_NORMAL << 2) | SH_INNER | (1 << 10) | (1 << 53);
pub const MMU_USER_RW: u64 =
    AP_RW_EL0 | (ATTR_NORMAL << 2) | SH_INNER | (1 << 10) | (1 << 53) | (1 << 54);
pub const MMU_USER_RO: u64 =
    AP_RO_EL0 | (ATTR_NORMAL << 2) | SH_INNER | (1 << 10) | (1 << 53) | (1 << 54);
pub const MMU_DEV: u64 =
    AP_RW_EL1 | (ATTR_DEVICE << 2) | SH_OUTER | (1 << 10) | (1 << 53) | (1 << 54);
pub const MMU_USER_DEV: u64 =
    AP_RW_EL0 | (ATTR_DEVICE << 2) | SH_OUTER | (1 << 10) | (1 << 53) | (1 << 54);
pub const MMU_USER_DMA: u64 =
    AP_RW_EL0 | (ATTR_NORMAL_NC << 2) | SH_INNER | (1 << 10) | (1 << 53) | (1 << 54);

// 每个CPU记录自己当前运行线程所属的页表根。以前这里只有一个全局根，
// 适用于单VSpace；独立进程后不能让CPU0切换页表影响CPU2的当前地址空间。
static ACTIVE_TABLES: [AtomicU64; exo_abi::MAX_CPUS] =
    [const { AtomicU64::new(0) }; exo_abi::MAX_CPUS];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapError {
    Conflict,
    NoMemory,
}

/// 从虚拟地址提取各级索引
fn l1_index(va: u64) -> usize {
    ((va >> 30) & 0x1ff) as usize
} // VA[38:30]
fn l2_index(va: u64) -> usize {
    ((va >> 21) & 0x1ff) as usize
} // VA[29:21]

/// 创建空页表, 返回 L1 表 (根页表) 的物理地址
/// L1 表也是一个 4KB 物理页, 初始全零 (无效)
pub fn create_table() -> u64 {
    let pa = mem::alloc_page().expect("mmu: failed to alloc L1 table");
    unsafe {
        core::ptr::write_bytes(pa as *mut u8, 0, 4096);
    } // 清零
    pa
}

/// 尝试创建一个新的 L1 页表根。
///
/// VSpace 是可由 EL0 请求创建的对象，失败必须返回错误码，不能像启动阶段
/// 那样 panic。因此它与启动路径使用的 `create_table` 分开，避免改变原有
/// bring-up 路径的行为。
pub fn try_create_table() -> Option<u64> {
    let pa = mem::alloc_page()?;
    unsafe {
        core::ptr::write_bytes(pa as *mut u8, 0, exo_abi::PAGE_SIZE as usize);
    }
    Some(pa)
}

pub fn map_block_2m(table_pa: u64, va: u64, pa: u64, flags: u64) {
    unsafe {
        let l1_ptr = table_pa as *mut u64;
        let l1e = l1_ptr.add(l1_index(va));

        if *l1e == 0 {
            let l2_pa = mem::alloc_page().expect("mmu: failed to alloc L2 table");
            core::ptr::write_bytes(l2_pa as *mut u8, 0, 4096);
            *l1e = l2_pa | DESC_TABLE;
        }

        let l2_pa = *l1e & !0xfff;
        let l2_ptr = l2_pa as *mut u64;
        *l2_ptr.add(l2_index(va)) = (pa & !0x1f_ffff) | flags | DESC_BLOCK;
    }
}

/// 可失败的 2MiB block 映射版本，供 EL0 请求创建 VSpace 使用。
///
/// 启动阶段的 `map_block_2m` 使用 `expect` 是因为页表耗尽属于不可恢复的
/// 启动错误；运行时创建地址空间则必须把同一个条件转换为 `NO_MEMORY`，不能
/// 让不可信的 EL0 请求直接终止整个 Kernel。
pub fn try_map_block_2m(table_pa: u64, va: u64, pa: u64, flags: u64) -> bool {
    unsafe {
        let l1_ptr = table_pa as *mut u64;
        let l1e = l1_ptr.add(l1_index(va));
        if *l1e == 0 {
            let Some(l2_pa) = mem::alloc_page() else {
                return false;
            };
            core::ptr::write_bytes(l2_pa as *mut u8, 0, 4096);
            *l1e = l2_pa | DESC_TABLE;
        }
        let l2_pa = *l1e & !0xfff;
        *(l2_pa as *mut u64).add(l2_index(va)) = (pa & !0x1f_ffff) | flags | DESC_BLOCK;
    }
    true
}

pub fn map_range_2m(
    table_pa: u64,
    start: u64,
    end: u64,
    flags: u64,
    skip_start: u64,
    skip_end: u64,
) {
    let mut va = start & !0x1f_ffff;
    let end_aligned = (end + 0x1f_ffff) & !0x1f_ffff;
    while va < end_aligned {
        if !(va >= skip_start && va < skip_end) {
            map_block_2m(table_pa, va, va, flags);
        }
        va += 0x20_0000;
    }
}

/// 往页表映射一个 4KB 页
///
/// 流程: L1 索引 → 看有没有 L2 表, 没有就分配 → L2 索引 → 看有没有 L3 表,
/// 没有就分配 → L3 索引 → 填 L3 条目 (物理地址 + 权限/属性)
fn map_one(table_pa: u64, va: u64, pa: u64, flags: u64) {
    unsafe {
        // L1 表: 512 个 64 位条目
        let l1_ptr = table_pa as *mut u64;
        let l1e = l1_ptr.add(l1_index(va));

        // 如果 L1 条目是空的, 分配一页做 L2 表
        if *l1e == 0 {
            let l2_pa = mem::alloc_page().expect("mmu: failed to alloc L2 table");
            core::ptr::write_bytes(l2_pa as *mut u8, 0, 4096);
            *l1e = l2_pa | DESC_TABLE; // bit[1:0]=11 表示"这是页表目录"
        }
        let l2_pa = *l1e & !0xfff; // 取 L2 表物理地址 (低 12 位是属性)
        let l2_ptr = l2_pa as *mut u64;
        let l2e = l2_ptr.add(l2_index(va));

        // 如果 L2 条目是空的, 分配一页做 L3 表
        if *l2e == 0 {
            let l3_pa = mem::alloc_page().expect("mmu: failed to alloc L3 table");
            core::ptr::write_bytes(l3_pa as *mut u8, 0, 4096);
            *l2e = l3_pa | DESC_TABLE;
        } else if (*l2e & 0b11) == DESC_BLOCK {
            panic!("mmu: cannot place L3 page under existing L2 block");
        }
        let l3_pa = *l2e & !0xfff;
        let l3_ptr = l3_pa as *mut u64;
        let l3_idx = ((va >> 12) & 0x1ff) as usize; // VA[20:12] 选 L3 中的哪一项

        // 填 L3 页描述符: 物理地址 + 标志 + 0b11 (有效页)
        *l3_ptr.add(l3_idx) = (pa & !0xfff) | flags | DESC_PAGE;
    }
}

fn map_one_checked(table_pa: u64, va: u64, pa: u64, flags: u64) -> Result<(), MapError> {
    unsafe {
        let l1_ptr = table_pa as *mut u64;
        let l1e = l1_ptr.add(l1_index(va));
        if *l1e == 0 {
            let l2_pa = mem::alloc_page().ok_or(MapError::NoMemory)?;
            core::ptr::write_bytes(l2_pa as *mut u8, 0, 4096);
            *l1e = l2_pa | DESC_TABLE;
        } else if (*l1e & 0b11) != DESC_TABLE {
            return Err(MapError::Conflict);
        }

        let l2_pa = *l1e & !0xfff;
        let l2e = (l2_pa as *mut u64).add(l2_index(va));
        if *l2e == 0 {
            let l3_pa = mem::alloc_page().ok_or(MapError::NoMemory)?;
            core::ptr::write_bytes(l3_pa as *mut u8, 0, 4096);
            *l2e = l3_pa | DESC_TABLE;
        } else if (*l2e & 0b11) != DESC_TABLE {
            return Err(MapError::Conflict);
        }

        let l3_pa = *l2e & !0xfff;
        let l3_idx = ((va >> 12) & 0x1ff) as usize;
        let pte = (l3_pa as *mut u64).add(l3_idx);
        if (*pte & 0b11) == DESC_PAGE {
            return Err(MapError::Conflict);
        }
        *pte = (pa & !0xfff) | flags | DESC_PAGE;
    }
    Ok(())
}

/// 连续映射 count 个 4KB 页 (虚拟地址连续, 物理地址连续)
pub fn map(table_pa: u64, va: u64, pa: u64, flags: u64, count: u64) {
    for i in 0..count {
        map_one(table_pa, va + i * 4096, pa + i * 4096, flags);
    }
}

/// 建立一段不可覆盖的4KB页映射。失败时撤销本次已经写入的PTE。
///
/// 新分配的空页表页保留为当前VSpace基础设施，后续映射可以复用。
pub fn map_checked(
    table_pa: u64,
    va: u64,
    pa: u64,
    flags: u64,
    count: u64,
) -> Result<(), MapError> {
    if count == 0 || !range_unmapped(table_pa, va, count) {
        return Err(MapError::Conflict);
    }
    let mut mapped = 0u64;
    while mapped < count {
        if let Err(error) = map_one_checked(table_pa, va + mapped * 4096, pa + mapped * 4096, flags)
        {
            unmap(table_pa, va, mapped);
            if mapped != 0 {
                flush_el1_tlb_range(va, mapped);
            }
            return Err(error);
        }
        mapped += 1;
    }
    flush_el1_tlb_range(va, count);
    Ok(())
}

pub fn mapped_pa(table_pa: u64, va: u64) -> Option<u64> {
    unsafe {
        let l1e = *((table_pa as *const u64).add(l1_index(va)));
        if (l1e & 0b11) != DESC_TABLE {
            return None;
        }
        let l2_pa = l1e & !0xfff;
        let l2e = *((l2_pa as *const u64).add(l2_index(va)));
        if (l2e & 0b11) != DESC_TABLE {
            return None;
        }
        let l3_pa = l2e & !0xfff;
        let l3_idx = ((va >> 12) & 0x1ff) as usize;
        let pte = *((l3_pa as *const u64).add(l3_idx));
        if (pte & 0b11) != DESC_PAGE {
            return None;
        }
        // 只提取输出物理地址[47:12]。UXN/PXN等属性位位于高位，不能用
        // `!0xfff`，否则精确映射核对会把权限位误当成PA的一部分。
        Some(pte & OUTPUT_ADDRESS_MASK)
    }
}

/// 检查一个 EL0 地址是否落在可执行的 L3 用户页中。
pub fn is_user_executable(table_pa: u64, va: u64) -> bool {
    unsafe {
        let l1e = *((table_pa as *const u64).add(l1_index(va)));
        if (l1e & 0b11) != DESC_TABLE {
            return false;
        }
        let l2 = (l1e & OUTPUT_ADDRESS_MASK) as *const u64;
        let l2e = *l2.add(l2_index(va));
        if (l2e & 0b11) != DESC_TABLE {
            return false;
        }
        let l3 = (l2e & OUTPUT_ADDRESS_MASK) as *const u64;
        let pte = *l3.add(((va >> 12) & 0x1ff) as usize);
        (pte & 0b11) == DESC_PAGE && (pte & (1 << 54)) == 0
    }
}

/// 读取目标 VSpace 中某个用户地址对应的 PTE。
///
/// 该函数只在 Kernel 已经确认 `table_pa` 属于自己的页表对象后使用；它不
/// 接受 EL0 提供的页表地址，因此裸指针解引用不会把任意 PA 变成 Kernel 的
/// 页表。
fn user_pte(table_pa: u64, va: u64) -> Option<u64> {
    unsafe {
        let l1e = core::ptr::read_volatile((table_pa as *const u64).add(l1_index(va)));
        if (l1e & 0b11) != DESC_TABLE {
            return None;
        }
        let l2 = (l1e & OUTPUT_ADDRESS_MASK) as *const u64;
        let l2e = core::ptr::read_volatile(l2.add(l2_index(va)));
        if (l2e & 0b11) != DESC_TABLE {
            return None;
        }
        let l3 = (l2e & OUTPUT_ADDRESS_MASK) as *const u64;
        let pte = core::ptr::read_volatile(l3.add(((va >> 12) & 0x1ff) as usize));
        if (pte & 0b11) != DESC_PAGE {
            None
        } else {
            Some(pte)
        }
    }
}

/// 检查一段地址是否全部是 EL0 可读映射。
pub fn is_user_readable(table_pa: u64, va: u64, bytes: u64) -> bool {
    if bytes == 0 {
        return false;
    }
    let Some(end) = va.checked_add(bytes) else {
        return false;
    };
    let first = va & !(exo_abi::PAGE_SIZE - 1);
    let last = (end - 1) & !(exo_abi::PAGE_SIZE - 1);
    let mut page = first;
    loop {
        let Some(pte) = user_pte(table_pa, page) else {
            return false;
        };
        // AP[2:1]=01/11 表示 EL0 可访问；PXN/UXN不影响读取。
        if (pte & (1 << 6)) == 0 && (pte & (1 << 7)) == 0 {
            return false;
        }
        if page == last {
            break;
        }
        page = page.saturating_add(exo_abi::PAGE_SIZE);
    }
    true
}

/// 检查一段地址是否全部是 EL0 可写映射。
pub fn is_user_writable(table_pa: u64, va: u64, bytes: u64) -> bool {
    if bytes == 0 {
        return false;
    }
    let Some(end) = va.checked_add(bytes) else {
        return false;
    };
    let first = va & !(exo_abi::PAGE_SIZE - 1);
    let last = (end - 1) & !(exo_abi::PAGE_SIZE - 1);
    let mut page = first;
    loop {
        let Some(pte) = user_pte(table_pa, page) else {
            return false;
        };
        // AP=01 为 EL0/EL1 RW；AP=11 是只读，因此必须明确检查 AP bit1。
        if (pte & (1 << 6)) == 0 || (pte & (1 << 7)) != 0 {
            return false;
        }
        if page == last {
            break;
        }
        page = page.saturating_add(exo_abi::PAGE_SIZE);
    }
    true
}

pub fn range_unmapped(table_pa: u64, va: u64, count: u64) -> bool {
    let mut index = 0u64;
    while index < count {
        if mapped_pa(table_pa, va + index * 4096).is_some() {
            return false;
        }
        index += 1;
    }
    true
}

pub fn range_matches(table_pa: u64, va: u64, pa: u64, count: u64) -> bool {
    let mut index = 0u64;
    while index < count {
        if mapped_pa(table_pa, va + index * 4096) != Some(pa + index * 4096) {
            return false;
        }
        index += 1;
    }
    true
}

/// 删除一个 EL0 L3 页映射。
///
/// 只清 PTE，不释放它指向的物理页，也不回收 L2/L3 页表页。资源所有者必须在
/// TLB 刷新后自行决定是否归还物理页。
fn unmap_one(table_pa: u64, va: u64) -> bool {
    unsafe {
        let l1e = *((table_pa as *const u64).add(l1_index(va)));
        if (l1e & 0b11) != DESC_TABLE {
            return false;
        }

        let l2_pa = l1e & !0xfff;
        let l2e = *((l2_pa as *const u64).add(l2_index(va)));
        if (l2e & 0b11) != DESC_TABLE {
            return false;
        }

        let l3_pa = l2e & !0xfff;
        let l3_idx = ((va >> 12) & 0x1ff) as usize;
        let pte = (l3_pa as *mut u64).add(l3_idx);
        if (*pte & 0b11) != DESC_PAGE {
            return false;
        }

        *pte = 0;
        true
    }
}

/// 删除连续的 4KB 用户映射，返回实际清除的 PTE 数量。
pub fn unmap(table_pa: u64, va: u64, count: u64) -> u64 {
    let mut removed = 0;
    let mut i = 0;
    while i < count {
        if unmap_one(table_pa, va + i * 4096) {
            removed += 1;
        }
        i += 1;
    }
    removed
}

/// 只有整段映射仍指向预期PA时才撤销，防止错误Handle破坏其他映射。
pub fn unmap_matching(table_pa: u64, va: u64, pa: u64, count: u64) -> bool {
    if count == 0 || !range_matches(table_pa, va, pa, count) {
        return false;
    }
    unmap(table_pa, va, count) == count
}

pub fn active_table() -> u64 {
    let cpu = crate::arch::aarch64::cpu::id();
    ACTIVE_TABLES[cpu.min(exo_abi::MAX_CPUS - 1)].load(Ordering::Acquire)
}

pub fn flush_el1_tlb() {
    crate::arch::aarch64::smp::shootdown_tlb();
}

pub fn flush_el1_tlb_range(va: u64, pages: u64) {
    let _ = (va, pages);
    // 当前四核共享一个VSpace且尚未分配ASID。为保证线程栈/IPC页在slot
    // 复用时绝不命中远程核旧翻译，第一版统一广播VMALLE1IS。
    crate::arch::aarch64::smp::shootdown_tlb();
}

/// 仅在当前PE执行TLB失效，不发送SGI。
///
/// 该原语供SMP shootdown协议使用；普通映射代码必须调用上面的公开接口，
/// 否则只会清除调用核的TLB，远程EL0线程仍可能访问已经释放的物理页。
pub(crate) fn flush_el1_tlb_local() {
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack)
        )
    };
}

/// 将普通Frame此前的CPU写入同步到指令侧，供RW解除后重新映射为RX。
pub fn sync_for_exec(pa: u64, pages: u64) {
    let bytes = pages * 4096;
    let end = pa + bytes;
    let ctr = mrs!("ctr_el0");
    let dline = 4u64 << ((ctr >> 16) & 0xf);
    let iline = 4u64 << (ctr & 0xf);

    let mut address = pa & !(dline - 1);
    while address < end {
        unsafe { core::arch::asm!("dc cvau, {}", in(reg) address, options(nostack)) };
        address += dline;
    }
    unsafe { core::arch::asm!("dsb ish", options(nostack)) };

    address = pa & !(iline - 1);
    while address < end {
        unsafe { core::arch::asm!("ic ivau, {}", in(reg) address, options(nostack)) };
        address += iline;
    }
    unsafe { core::arch::asm!("dsb ish; isb", options(nostack)) };
}

/// 激活 EL1 页表: 写系统寄存器, 开 MMU
///
/// 写 TTBR0_EL1 (页表基址)
/// 写 TCR_EL1 (翻译控制: 4KB granule, 39-bit 虚拟地址)
/// 写 MAIR_EL1 (内存属性索引: 普通内存可缓存 + 设备内存不可缓存)
/// 写 SCTLR_EL1.M=1 (开 MMU)
/// 清 TLB + 内存屏障 (确保新配置生效)
pub fn activate(table_pa: u64) {
    let cpu = crate::arch::aarch64::cpu::id();
    ACTIVE_TABLES[cpu.min(exo_abi::MAX_CPUS - 1)].store(table_pa, Ordering::Release);

    // TTBR0_EL1: 页表物理地址
    msr!("ttbr0_el1", table_pa);

    // TCR_EL1: T0SZ=25 → 虚拟地址 39 位, 3 级翻译从 L1 开始
    let tcr: u64 = (0b00 << 14)  // TG0=4KB granule
                 | (25 << 0)     // T0SZ=64-39=25
                 | (0b11 << 8)   // IRGN0=Inner WB-WA (普通内存)
                 | (0b11 << 10)  // ORGN0=Outer WB-WA
                 | (0b11 << 12)  // SH0=Inner Shareable
                 | (0b101 << 32); // IPS=48-bit PA, 允许映射 4GB 以上物理地址
    msr!("tcr_el1", tcr);

    // MAIR_EL1: 定义三个属性索引
    //   Attr[0] = 0xff (Normal Memory, Write-Back, 可缓存, 页表和普通数据用)
    //   Attr[1] = 0x04 (Device-nGnRE, 不可缓存, MMIO 寄存器用)
    //   Attr[2] = 0x44 (Normal Non-cacheable, DMA bring-up 用)
    let mair: u64 = (0xff << 0) | (0x04 << 8) | (0x44 << 16);
    msr!("mair_el1", mair);

    unsafe { core::arch::asm!("dsb ish; tlbi vmalle1; dsb ish; isb") };

    // SCTLR_EL1: RES1 位 + M/C/I。这里不沿用固件留下的 EL1 状态。
    let sctlr: u64 = (1 << 0)   // M: enable MMU
                   | (1 << 2)   // C: data cache
                   | (1 << 11)  // RES1
                   | (1 << 12)  // I: instruction cache
                   | (1 << 20)  // RES1
                   | (1 << 22)  // RES1
                   | (1 << 23)  // RES1
                   | (1 << 28)  // RES1
                   | (1 << 29); // RES1
    msr!("sctlr_el1", sctlr);
    unsafe { core::arch::asm!("isb") };
}

/// 在已经配置好TCR/MAIR/MMU的CPU上切换到另一个VSpace。
///
/// ASID由vspace模块分配并随TTBR0一起写入。这里不清空全部TLB，依靠ASID
/// 隔离不同地址空间；页表修改本身仍由映射路径执行全核TLB shootdown。
pub fn switch_to(table_pa: u64, asid: u16) {
    let cpu = crate::arch::aarch64::cpu::id();
    ACTIVE_TABLES[cpu.min(exo_abi::MAX_CPUS - 1)].store(table_pa, Ordering::Release);
    let ttbr = table_pa | ((asid as u64) << 48);
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "msr ttbr0_el1, {ttbr}",
            "isb",
            ttbr = in(reg) ttbr,
            options(nostack)
        );
    }
}

/// 释放一个独立VSpace拥有的页表根及其下级页表页。
///
/// 该函数只接收新建VSpace的私有根；启动根永远不经过这里，因此不会释放
/// Kernel启动页表或其中的EL1共享映射。
pub fn destroy_table(root: u64) {
    if root == 0 {
        return;
    }
    unsafe {
        let l1 = root as *mut u64;
        for l1_index in 0..512usize {
            let l1_entry = core::ptr::read_volatile(l1.add(l1_index));
            if (l1_entry & 0b11) != DESC_TABLE {
                continue;
            }
            let l2_pa = l1_entry & OUTPUT_ADDRESS_MASK;
            let l2 = l2_pa as *mut u64;
            for l2_index in 0..512usize {
                let l2_entry = core::ptr::read_volatile(l2.add(l2_index));
                if (l2_entry & 0b11) == DESC_TABLE {
                    let l3_pa = l2_entry & OUTPUT_ADDRESS_MASK;
                    mem::free_page(l3_pa);
                }
            }
            mem::free_page(l2_pa);
        }
        mem::free_page(root);
    }
}
