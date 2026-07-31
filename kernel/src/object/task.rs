//! 单 EL0 任务的资源记录与退出回收。
//!
//! 这是后续 capability/task table 的最小前身。EL1 记录哪些物理页属于当前
//! libOS，以及向它开放了哪些用户 VA、绑定了哪些 IRQ；EL0 不能自行伪造或释放这些资源。
//!
//! IRQ统一绑定到Notification：EL1先禁用SPI并EOI，再投递badge；用户线程
//! 处理设备状态后通过SYS_IRQ_ACK清除pending状态并重新使能SPI。

use crate::config::{
    USER_BOOT_INFO_VA, USER_HEAP_BASE, USER_HEAP_SIZE, USER_STACK_PAGES, USER_STACK_TOP,
};
use crate::{gic, mem, mmu, uart};

const MAX_MMIO_MAPPINGS: usize = 16;
const MAX_IRQ_BINDINGS: usize = 16;
const MAX_OWNED_MAPPINGS: usize = 12;
const MAX_DMA_MAPPINGS: usize = 64;
pub const MAX_MMIO_GRANTS: usize = 8;
pub const MAX_IRQ_GRANTS: usize = 32;

/// EL1 从 DTB 资源快照中授予当前任务的一段 MMIO 物理地址范围。
#[derive(Clone, Copy)]
pub struct MmioGrant {
    pub base: u64,
    pub size: u64,
}

impl MmioGrant {
    pub const EMPTY: Self = Self { base: 0, size: 0 };
}

/// 当前任务可绑定的 GIC SPI，以及 DTB interrupt specifier 中的触发标志。
#[derive(Clone, Copy)]
pub struct IrqGrant {
    pub intid: u32,
    pub flags: u32,
}

impl IrqGrant {
    pub const EMPTY: Self = Self { intid: 0, flags: 0 };
}

#[derive(Clone, Copy)]
pub struct OwnedPages {
    pub va: u64,
    pub pa: u64,
    pub pages: u64,
}

impl OwnedPages {
    pub const EMPTY: Self = Self {
        va: 0,
        pa: 0,
        pages: 0,
    };
}

#[derive(Clone, Copy)]
struct UserMapping {
    va: u64,
    pages: u64,
}

#[derive(Clone, Copy)]
struct DmaMapping {
    va: u64,
    pa: u64,
    pages: u64,
}

impl DmaMapping {
    const EMPTY: Self = Self {
        va: 0,
        pa: 0,
        pages: 0,
    };
}

impl UserMapping {
    const EMPTY: Self = Self { va: 0, pages: 0 };
}

#[derive(Clone, Copy)]
struct IrqBinding {
    intid: u32,
    notification: u64,
    badge: u64,
    /// IRQ入口已经完成GICC EOIR；该位表示设备处理尚未由EL0 ACK。
    awaiting_ack: bool,
}

impl IrqBinding {
    const EMPTY: Self = Self {
        intid: 0,
        notification: 0,
        badge: 0,
        awaiting_ack: false,
    };
}

struct TaskResources {
    active: bool,
    mmio_grants: [MmioGrant; MAX_MMIO_GRANTS],
    mmio_grant_count: usize,
    irq_grants: [IrqGrant; MAX_IRQ_GRANTS],
    irq_grant_count: usize,
    owned: [OwnedPages; MAX_OWNED_MAPPINGS],
    owned_count: usize,
    stack_pa: u64,
    boot_info_pa: u64,
    heap_pa: u64,
    mmio: [UserMapping; MAX_MMIO_MAPPINGS],
    mmio_count: usize,
    dma: [DmaMapping; MAX_DMA_MAPPINGS],
    dma_count: usize,
    // IRQ state
    irq_bindings: [IrqBinding; MAX_IRQ_BINDINGS],
    irq_binding_count: usize,
}

static mut CURRENT: TaskResources = TaskResources {
    active: false,
    mmio_grants: [MmioGrant::EMPTY; MAX_MMIO_GRANTS],
    mmio_grant_count: 0,
    irq_grants: [IrqGrant::EMPTY; MAX_IRQ_GRANTS],
    irq_grant_count: 0,
    owned: [OwnedPages::EMPTY; MAX_OWNED_MAPPINGS],
    owned_count: 0,
    stack_pa: 0,
    boot_info_pa: 0,
    heap_pa: 0,
    mmio: [UserMapping::EMPTY; MAX_MMIO_MAPPINGS],
    mmio_count: 0,
    dma: [DmaMapping::EMPTY; MAX_DMA_MAPPINGS],
    dma_count: 0,
    irq_bindings: [IrqBinding::EMPTY; MAX_IRQ_BINDINGS],
    irq_binding_count: 0,
};
// 单任务并不等于单核：USB IRQ入口、CPU0协调线程和CPU2驱动线程会并发
// 查询或修改授权/绑定状态。该锁保护CURRENT中的所有计数器和数组。
static TASK_LOCK: crate::sync::SpinLock<()> = crate::sync::SpinLock::new(());

/// v1只有一个资源域；0始终表示没有活动任务。
pub fn current_owner() -> u32 {
    let _guard = TASK_LOCK.lock();
    unsafe {
        if CURRENT.active {
            1
        } else {
            0
        }
    }
}

/// 在第一次 eret 到 EL0 前登记该任务拥有的内存和设备授权。
pub fn install(
    segments: &[OwnedPages],
    stack_pa: u64,
    boot_info_pa: u64,
    heap_pa: u64,
    mmio_grants: &[MmioGrant],
    irq_grants: &[IrqGrant],
) {
    let _guard = TASK_LOCK.lock();
    unsafe {
        CURRENT = TaskResources {
            active: true,
            mmio_grants: [MmioGrant::EMPTY; MAX_MMIO_GRANTS],
            mmio_grant_count: 0,
            irq_grants: [IrqGrant::EMPTY; MAX_IRQ_GRANTS],
            irq_grant_count: 0,
            owned: [OwnedPages::EMPTY; MAX_OWNED_MAPPINGS],
            owned_count: 0,
            stack_pa,
            boot_info_pa,
            heap_pa,
            mmio: [UserMapping::EMPTY; MAX_MMIO_MAPPINGS],
            mmio_count: 0,
            dma: [DmaMapping::EMPTY; MAX_DMA_MAPPINGS],
            dma_count: 0,
            irq_bindings: [IrqBinding::EMPTY; MAX_IRQ_BINDINGS],
            irq_binding_count: 0,
        };
        let mut index = 0usize;
        while index < segments.len() && index < MAX_OWNED_MAPPINGS {
            CURRENT.owned[index] = segments[index];
            index += 1;
        }
        CURRENT.owned_count = index;

        index = 0;
        while index < mmio_grants.len() && index < MAX_MMIO_GRANTS {
            CURRENT.mmio_grants[index] = mmio_grants[index];
            index += 1;
        }
        CURRENT.mmio_grant_count = index;

        index = 0;
        while index < irq_grants.len() && index < MAX_IRQ_GRANTS {
            CURRENT.irq_grants[index] = irq_grants[index];
            index += 1;
        }
        CURRENT.irq_grant_count = index;
    }
}

/// 检查完整物理范围是否属于当前任务的一项 MMIO grant。
pub fn is_mmio_granted(base: u64, size: u64) -> bool {
    if size == 0 {
        return false;
    }
    let Some(end) = base.checked_add(size) else {
        return false;
    };
    let _guard = TASK_LOCK.lock();
    unsafe {
        if !CURRENT.active {
            return false;
        }
        let mut index = 0usize;
        while index < CURRENT.mmio_grant_count {
            let grant = CURRENT.mmio_grants[index];
            if let Some(grant_end) = grant.base.checked_add(grant.size) {
                if base >= grant.base && end <= grant_end {
                    return true;
                }
            }
            index += 1;
        }
    }
    false
}

/// 在真正修改页表前确认资源表还能记录这次 MMIO 映射。
pub fn can_record_mmio() -> bool {
    let _guard = TASK_LOCK.lock();
    unsafe { CURRENT.active && CURRENT.mmio_count < MAX_MMIO_MAPPINGS }
}

/// 记录已经成功建立的 EL0 MMIO 虚拟映射。
pub fn record_mmio(user_va: u64, pages: u64) {
    let _guard = TASK_LOCK.lock();
    unsafe {
        let index = CURRENT.mmio_count;
        CURRENT.mmio[index] = UserMapping { va: user_va, pages };
        CURRENT.mmio_count += 1;
    }
}

pub fn remove_mmio(user_va: u64, pages: u64) -> bool {
    let _guard = TASK_LOCK.lock();
    unsafe {
        let mut index = 0usize;
        while index < CURRENT.mmio_count {
            let mapping = CURRENT.mmio[index];
            if mapping.va == user_va && mapping.pages == pages {
                CURRENT.mmio_count -= 1;
                CURRENT.mmio[index] = CURRENT.mmio[CURRENT.mmio_count];
                CURRENT.mmio[CURRENT.mmio_count] = UserMapping::EMPTY;
                return true;
            }
            index += 1;
        }
    }
    false
}

pub fn range_available(va: u64, pages: u64) -> bool {
    if pages == 0 {
        return false;
    }
    let Some(end) = va.checked_add(pages * 4096) else {
        return false;
    };
    if va < exo_abi::USER_BASE || end > exo_abi::USER_WINDOW_END {
        return false;
    }
    let _guard = TASK_LOCK.lock();
    unsafe {
        if !CURRENT.active {
            return false;
        }
        let overlaps = |start: u64, count: u64| {
            let other_end = start.saturating_add(count * 4096);
            va < other_end && end > start
        };
        let mut index = 0usize;
        while index < CURRENT.owned_count {
            let item = CURRENT.owned[index];
            if overlaps(item.va, item.pages) {
                return false;
            }
            index += 1;
        }
        if overlaps(USER_BOOT_INFO_VA, 1)
            || overlaps(USER_HEAP_BASE, USER_HEAP_SIZE / 4096)
            || overlaps(USER_STACK_TOP - USER_STACK_PAGES * 4096, USER_STACK_PAGES)
        {
            return false;
        }
        index = 0;
        while index < CURRENT.mmio_count {
            let item = CURRENT.mmio[index];
            if overlaps(item.va, item.pages) {
                return false;
            }
            index += 1;
        }
        index = 0;
        while index < CURRENT.dma_count {
            let item = CURRENT.dma[index];
            if overlaps(item.va, item.pages) {
                return false;
            }
            index += 1;
        }
    }
    true
}

pub fn record_dma(va: u64, pa: u64, pages: u64) -> bool {
    let _guard = TASK_LOCK.lock();
    unsafe {
        if !CURRENT.active || CURRENT.dma_count == MAX_DMA_MAPPINGS {
            return false;
        }
        CURRENT.dma[CURRENT.dma_count] = DmaMapping { va, pa, pages };
        CURRENT.dma_count += 1;
    }
    true
}

pub fn take_dma(va: u64, pages: u64) -> Option<u64> {
    let _guard = TASK_LOCK.lock();
    unsafe {
        let mut index = 0usize;
        while index < CURRENT.dma_count {
            let item = CURRENT.dma[index];
            if item.va == va && item.pages == pages {
                CURRENT.dma_count -= 1;
                CURRENT.dma[index] = CURRENT.dma[CURRENT.dma_count];
                CURRENT.dma[CURRENT.dma_count] = DmaMapping::EMPTY;
                return Some(item.pa);
            }
            index += 1;
        }
    }
    None
}

// ── IRQ 绑定管理 ──

/// 返回当前任务获授权 IRQ 的 DTB flags；未授权时返回 None。
pub fn irq_flags(intid: u32) -> Option<u32> {
    let _guard = TASK_LOCK.lock();
    unsafe {
        if !CURRENT.active {
            return None;
        }
        let mut index = 0usize;
        while index < CURRENT.irq_grant_count {
            let grant = CURRENT.irq_grants[index];
            if grant.intid == intid {
                return Some(grant.flags);
            }
            index += 1;
        }
    }
    None
}

/// 把一个获授权IRQ绑定到当前任务拥有的Notification。
/// 返回 true 表示成功；false 表示已绑定或超出容量。
pub fn bind_irq(intid: u32, notification: u64, badge: u64) -> bool {
    let _guard = TASK_LOCK.lock();
    unsafe {
        if !CURRENT.active || CURRENT.irq_binding_count >= MAX_IRQ_BINDINGS {
            return false;
        }
        // 检查是否已绑定
        let mut i = 0;
        while i < CURRENT.irq_binding_count {
            if CURRENT.irq_bindings[i].intid == intid {
                return false;
            }
            i += 1;
        }
        CURRENT.irq_bindings[CURRENT.irq_binding_count] = IrqBinding {
            intid,
            notification,
            badge,
            awaiting_ack: false,
        };
        CURRENT.irq_binding_count += 1;
        true
    }
}

/// 解除绑定并禁用该 IRQ。
pub fn unbind_irq(intid: u32) -> bool {
    let _guard = TASK_LOCK.lock();
    unsafe {
        if !CURRENT.active {
            return false;
        }
        let mut i = 0;
        while i < CURRENT.irq_binding_count {
            if CURRENT.irq_bindings[i].intid == intid {
                gic::disable_spi(intid);
                // 从数组中移除
                CURRENT.irq_binding_count -= 1;
                CURRENT.irq_bindings[i] = CURRENT.irq_bindings[CURRENT.irq_binding_count];
                return true;
            }
            i += 1;
        }
        false
    }
}

/// 检查给定 INTID 是否已被当前任务绑定。
pub fn has_binding(intid: u32) -> bool {
    let _guard = TASK_LOCK.lock();
    unsafe {
        let mut i = 0;
        while i < CURRENT.irq_binding_count {
            if CURRENT.irq_bindings[i].intid == intid {
                return true;
            }
            i += 1;
        }
        false
    }
}

/// 返回IRQ绑定的异步通知对象和badge。
pub fn irq_notification(intid: u32) -> Option<(u64, u64)> {
    let _guard = TASK_LOCK.lock();
    unsafe {
        let mut index = 0usize;
        while index < CURRENT.irq_binding_count {
            let binding = CURRENT.irq_bindings[index];
            if binding.intid == intid {
                return Some((binding.notification, binding.badge));
            }
            index += 1;
        }
    }
    None
}

/// 检查 Notification 是否仍被某个 IRQ 使用，避免用户先销毁通知对象，
/// 再让硬件 IRQ 到达后把事件投递到一个过期 Handle。
pub fn notification_is_bound(notification: u64) -> bool {
    let _guard = TASK_LOCK.lock();
    unsafe {
        let mut index = 0usize;
        while index < CURRENT.irq_binding_count {
            if CURRENT.irq_bindings[index].notification == notification {
                return true;
            }
            index += 1;
        }
    }
    false
}

/// IRQ入口在禁用SPI并写EOIR后记录“等待用户ACK”状态。
pub fn record_irq_pending(intid: u32) {
    let _guard = TASK_LOCK.lock();
    unsafe {
        let mut index = 0usize;
        while index < CURRENT.irq_binding_count {
            if CURRENT.irq_bindings[index].intid == intid {
                CURRENT.irq_bindings[index].awaiting_ack = true;
                return;
            }
            index += 1;
        }
    }
}

/// 确认IRQ设备状态已处理完毕：校验INTID和pending状态并重新使能SPI。
/// GICC EOIR已在IRQ入口完成，ACK不再持有或处理IAR token。
/// 返回 true 表示成功。
pub fn ack_irq(intid: u32) -> bool {
    let _guard = TASK_LOCK.lock();
    unsafe {
        let mut index = 0usize;
        while index < CURRENT.irq_binding_count {
            if CURRENT.irq_bindings[index].intid == intid {
                if !CURRENT.irq_bindings[index].awaiting_ack {
                    return false;
                }
                CURRENT.irq_bindings[index].awaiting_ack = false;
                gic::enable_spi(intid);
                return true;
            }
            index += 1;
        }
        false
    }
}

/// 退出时禁用全部IRQ并清除等待ACK状态。
pub fn cleanup_irqs() {
    let _guard = TASK_LOCK.lock();
    unsafe {
        let mut i = 0;
        while i < CURRENT.irq_binding_count {
            gic::disable_spi(CURRENT.irq_bindings[i].intid);
            i += 1;
        }
        CURRENT.irq_binding_count = 0;
    }
}

/// 结束当前 EL0 任务。该函数不会返回。
pub fn exit_current(code: u64) -> ! {
    uart::puts("\r\n[exo] EL0 exit code=");
    uart::hex(code);
    uart::puts("\r\n");

    // 先让CPU1..3离开共享EL0 VSpace。等待完整STOPPED mask之后，本核
    // 才能安全撤销代码、线程栈、MMIO和DMA映射。
    crate::arch::aarch64::smp::stop_other_cpus();

    unsafe {
        if !CURRENT.active {
            uart::puts("[exo] no active EL0 task\r\n");
            halt();
        }

        cleanup_irqs();

        let root = mmu::active_table();
        let owner = current_owner();

        // 先停掉Generic Timer并停止所属线程，防止清理线程栈和页表时
        // 再发生一次EL0调度。
        crate::scheduler::priority::cleanup_owner(owner);

        // Endpoint、Reply 和 Notification 都属于任务；先使所有 Handle
        // 失效，避免线程表回收后留下指向旧线程 slot 的等待关系。
        crate::ipc::cleanup_owner(owner);

        // 线程栈和 IPC Buffer 属于线程对象，先撤销并回收，随后再清理
        // 任务级的 ELF、heap、Frame、MMIO、DMA 和其他设备资源。
        crate::thread::cleanup_all(root);

        // 先撤销所有用户映射。此时 CPU 已因 SVC 位于 EL1，不再从 EL0 页面取指。
        crate::vspace::cleanup_owner(owner, root);
        let mut owned_index = 0usize;
        while owned_index < CURRENT.owned_count {
            let mapping = CURRENT.owned[owned_index];
            mmu::unmap(root, mapping.va, mapping.pages);
            owned_index += 1;
        }
        mmu::unmap(
            root,
            USER_STACK_TOP - USER_STACK_PAGES * 4096,
            USER_STACK_PAGES,
        );
        mmu::unmap(root, USER_BOOT_INFO_VA, 1);
        mmu::unmap(root, USER_HEAP_BASE, USER_HEAP_SIZE / 4096);

        let mut i = 0;
        while i < CURRENT.mmio_count {
            let mapping = CURRENT.mmio[i];
            mmu::unmap(root, mapping.va, mapping.pages);
            i += 1;
        }
        i = 0;
        while i < CURRENT.dma_count {
            let mapping = CURRENT.dma[i];
            mmu::unmap(root, mapping.va, mapping.pages);
            i += 1;
        }

        // Break-before-reuse：清除 PTE 后先让旧 TLB 项失效，再把 RAM 归还分配器。
        mmu::flush_el1_tlb();
        let free_before_frame_cleanup = mem::free_pages_total();
        crate::frame::cleanup_owner(owner);
        let reclaimed_frame_pages =
            mem::free_pages_total().saturating_sub(free_before_frame_cleanup);
        uart::puts("[exo] Frame exit cleanup reclaimed pages=");
        uart::hex(reclaimed_frame_pages);
        uart::puts("\r\n");
        owned_index = 0;
        while owned_index < CURRENT.owned_count {
            let mapping = CURRENT.owned[owned_index];
            mem::free_pages(mapping.pa, mapping.pages);
            owned_index += 1;
        }
        mem::free_pages(CURRENT.stack_pa, USER_STACK_PAGES);
        mem::free_page(CURRENT.boot_info_pa);
        mem::free_pages(CURRENT.heap_pa, USER_HEAP_SIZE / 4096);
        i = 0;
        while i < CURRENT.dma_count {
            let mapping = CURRENT.dma[i];
            mem::free_pages(mapping.pa, mapping.pages);
            i += 1;
        }

        CURRENT.active = false;
        CURRENT.mmio_grant_count = 0;
        CURRENT.irq_grant_count = 0;
        uart::puts("[exo] EL0 mappings revoked, code/stack pages reclaimed\r\n");
    }

    halt()
}

fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
