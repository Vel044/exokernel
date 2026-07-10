//! 单 EL0 任务的资源记录与退出回收。
//!
//! 这是后续 capability/task table 的最小前身。EL1 记录哪些物理页属于当前
//! libOS，以及向它开放了哪些用户 VA、绑定了哪些 IRQ；EL0 不能自行伪造或释放这些资源。
//!
//! IRQ 模型 (v1 单任务，无线程调度):
//!   - IRQ 只在 SYS_IRQ_WAIT 期间在 GICD 中使能。EL0 执行时不使能任何 IRQ。
//!   - WAIT 内 IRQ 触发 → EL1 IRQ handler 读 GICC_IAR、记录 token、清除 GICD 使能。
//!   - EL0 处理完毕后调 SYS_IRQ_ACK → EL1 校验 token 后写 GICC_EOIR。
//!   - 同一时间最多一个已确认未 ACK 的 IRQ。

use crate::config::{DTB_USER_VA, USER_BASE, USER_STACK_PAGES, USER_STACK_TOP};
use crate::{gic, mem, mmu, uart};

const MAX_MMIO_MAPPINGS: usize = 16;
const MAX_IRQ_BINDINGS: usize = 16;

#[derive(Clone, Copy)]
struct UserMapping {
    va: u64,
    pages: u64,
}

impl UserMapping {
    const EMPTY: Self = Self { va: 0, pages: 0 };
}

#[derive(Clone, Copy)]
struct IrqBinding {
    intid: u32,
}

impl IrqBinding {
    const EMPTY: Self = Self { intid: 0 };
}

struct TaskResources {
    active: bool,
    code_pa: u64,
    code_pages: u64,
    stack_pa: u64,
    dtb_pages: u64,
    mmio: [UserMapping; MAX_MMIO_MAPPINGS],
    mmio_count: usize,
    // IRQ state
    irq_bindings: [IrqBinding; MAX_IRQ_BINDINGS],
    irq_binding_count: usize,
    active_iar: u32, // 完整 GICC_IAR token；0 = 无已确认但未 EOI 的 IRQ
}

static mut CURRENT: TaskResources = TaskResources {
    active: false,
    code_pa: 0,
    code_pages: 0,
    stack_pa: 0,
    dtb_pages: 0,
    mmio: [UserMapping::EMPTY; MAX_MMIO_MAPPINGS],
    mmio_count: 0,
    irq_bindings: [IrqBinding::EMPTY; MAX_IRQ_BINDINGS],
    irq_binding_count: 0,
    active_iar: 0,
};

/// 在第一次 eret 到 EL0 前登记该任务拥有的资源。
pub fn install(code_pa: u64, code_pages: u64, stack_pa: u64, dtb_pages: u64) {
    unsafe {
        CURRENT = TaskResources {
            active: true,
            code_pa,
            code_pages,
            stack_pa,
            dtb_pages,
            mmio: [UserMapping::EMPTY; MAX_MMIO_MAPPINGS],
            mmio_count: 0,
            irq_bindings: [IrqBinding::EMPTY; MAX_IRQ_BINDINGS],
            irq_binding_count: 0,
            active_iar: 0,
        };
    }
}

/// 在真正修改页表前确认资源表还能记录这次 MMIO 映射。
pub fn can_record_mmio() -> bool {
    unsafe { CURRENT.active && CURRENT.mmio_count < MAX_MMIO_MAPPINGS }
}

/// 记录已经成功建立的 EL0 MMIO 虚拟映射。
pub fn record_mmio(user_va: u64, pages: u64) {
    unsafe {
        let index = CURRENT.mmio_count;
        CURRENT.mmio[index] = UserMapping { va: user_va, pages };
        CURRENT.mmio_count += 1;
    }
}

// ── IRQ 绑定管理 ──

/// 检查 INTID 是否可以由当前任务绑定。
/// v1 只允许已知的 UART IRQ (由 kmain 预先在 protect 中登记)。
pub fn can_bind_irq(intid: u32) -> bool {
    unsafe { CURRENT.active && gic::is_authorized(intid) }
}

/// 绑定一个 IRQ 到当前任务。绑定后该 IRQ 可在 SYS_IRQ_WAIT 中使能。
/// 返回 true 表示成功；false 表示已绑定或超出容量。
pub fn bind_irq(intid: u32) -> bool {
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
        CURRENT.irq_bindings[CURRENT.irq_binding_count] = IrqBinding { intid };
        CURRENT.irq_binding_count += 1;
        true
    }
}

/// 解除绑定并禁用该 IRQ。
pub fn unbind_irq(intid: u32) -> bool {
    unsafe {
        if !CURRENT.active {
            return false;
        }
        let mut i = 0;
        while i < CURRENT.irq_binding_count {
            if CURRENT.irq_bindings[i].intid == intid {
                gic::disable_spi(intid);
                // active IRQ 不能直接丢弃，必须把原始 IAR token 写回 EOIR。
                if CURRENT.active_iar != 0 && gic::intid(CURRENT.active_iar) == intid {
                    gic::write_eoir(CURRENT.active_iar);
                    CURRENT.active_iar = 0;
                }
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

/// 第一个绑定的 INTID (v1 单 IRQ 场景)。
pub fn first_bound_intid() -> Option<u32> {
    unsafe {
        if CURRENT.irq_binding_count > 0 {
            Some(CURRENT.irq_bindings[0].intid)
        } else {
            None
        }
    }
}

pub fn enable_bound_irqs() {
    unsafe {
        let mut i = 0;
        while i < CURRENT.irq_binding_count {
            gic::enable_spi(CURRENT.irq_bindings[i].intid);
            i += 1;
        }
    }
}

pub fn disable_bound_irqs() {
    unsafe {
        let mut i = 0;
        while i < CURRENT.irq_binding_count {
            gic::disable_spi(CURRENT.irq_bindings[i].intid);
            i += 1;
        }
    }
}

// ── IAR token 管理 (由 IRQ handler 写入，WAIT/ACK 消费) ──

/// 由 EL1 IRQ handler 调用，保存完整 GICC_IAR token。
pub fn record_iar(iar_token: u32) {
    unsafe {
        CURRENT.active_iar = iar_token;
    }
}

/// 检查是否有未确认的 active IRQ。
pub fn has_active_iar() -> bool {
    unsafe { CURRENT.active_iar != 0 }
}

/// 获取 active IAR 不清零（给 ACK 校验用）。
pub fn active_iar() -> u32 {
    unsafe { CURRENT.active_iar }
}

/// 确认 IRQ 已完成处理：校验 INTID 匹配当前 active token，写 GICC_EOIR。
/// 返回 true 表示成功。
pub fn ack_irq(intid: u32) -> bool {
    unsafe {
        if CURRENT.active_iar == 0 || gic::intid(CURRENT.active_iar) != intid {
            return false;
        }
        gic::write_eoir(CURRENT.active_iar);
        CURRENT.active_iar = 0;
        true
    }
}

/// 退出时清理所有 IRQ：禁用并清除 IAR。
pub fn cleanup_irqs() {
    unsafe {
        let mut i = 0;
        while i < CURRENT.irq_binding_count {
            gic::disable_spi(CURRENT.irq_bindings[i].intid);
            i += 1;
        }
        if CURRENT.active_iar != 0 {
            gic::write_eoir(CURRENT.active_iar);
            CURRENT.active_iar = 0;
        }
        CURRENT.irq_binding_count = 0;
    }
}

/// 结束当前 EL0 任务。该函数不会返回。
pub fn exit_current(code: u64) -> ! {
    uart::puts("\r\n[exo] EL0 exit code=");
    uart::hex(code);
    uart::puts("\r\n");

    unsafe {
        if !CURRENT.active {
            uart::puts("[exo] no active EL0 task\r\n");
            halt();
        }

        cleanup_irqs();

        let root = mmu::active_table();

        // 先撤销所有用户映射。此时 CPU 已因 SVC 位于 EL1，不再从 EL0 页面取指。
        mmu::unmap(root, USER_BASE, CURRENT.code_pages);
        mmu::unmap(
            root,
            USER_STACK_TOP - USER_STACK_PAGES * 4096,
            USER_STACK_PAGES,
        );
        mmu::unmap(root, DTB_USER_VA, CURRENT.dtb_pages);

        let mut i = 0;
        while i < CURRENT.mmio_count {
            let mapping = CURRENT.mmio[i];
            mmu::unmap(root, mapping.va, mapping.pages);
            i += 1;
        }

        // Break-before-reuse：清除 PTE 后先让旧 TLB 项失效，再把 RAM 归还分配器。
        mmu::flush_el1_tlb();
        mem::free_pages(CURRENT.code_pa, CURRENT.code_pages);
        mem::free_pages(CURRENT.stack_pa, USER_STACK_PAGES);

        CURRENT.active = false;
        uart::puts("[exo] EL0 mappings revoked, code/stack pages reclaimed\r\n");
    }

    halt()
}

fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
