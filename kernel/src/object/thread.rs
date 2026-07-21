//! 单任务多线程的上下文与生命周期管理。

use crate::{mem, mmu, scheduler, trap::TrapFrame};

pub const MAX_THREADS: usize = 16;
const MAX_GENERATION: u32 = 0x7fff_ffff;
const DEFAULT_PRIORITY: u8 = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadState {
    Free,
    Ready,
    Running,
    Blocked,
    Exited,
}

#[derive(Clone, Copy)]
struct Thread {
    generation: u32,
    owner: u32,
    state: ThreadState,
    priority: u8,
    ready_order: u64,
    context: TrapFrame,
    stack_pa: u64,
    stack_pages: u64,
    stack_va: u64,
    ipc_pa: u64,
    ipc_va: u64,
}

impl Thread {
    const EMPTY: Self = Self {
        generation: 1,
        owner: 0,
        state: ThreadState::Free,
        priority: DEFAULT_PRIORITY,
        ready_order: 0,
        context: TrapFrame::ZERO,
        stack_pa: 0,
        stack_pages: 0,
        stack_va: 0,
        ipc_pa: 0,
        ipc_va: 0,
    };
}

static mut THREADS: [Thread; MAX_THREADS] = [Thread::EMPTY; MAX_THREADS];
static mut CURRENT: usize = 0;
static mut READY_CLOCK: u64 = 1;
static mut INITIALIZED: bool = false;

fn next_generation(value: u32) -> u32 {
    if value >= MAX_GENERATION {
        1
    } else {
        value + 1
    }
}

fn make_handle(slot: usize, generation: u32) -> u64 {
    ((generation as u64) << 32) | (slot as u64 + 1)
}

fn decode_handle(handle: u64) -> Option<(usize, u32)> {
    let raw_slot = handle as u32;
    let generation = (handle >> 32) as u32;
    if raw_slot == 0 || raw_slot as usize > MAX_THREADS || generation == 0 {
        return None;
    }
    Some((raw_slot as usize - 1, generation))
}

fn stack_base(slot: usize) -> u64 {
    exo_abi::THREAD_STACK_ARENA_BASE
        + slot as u64 * exo_abi::THREAD_STACK_PAGES * exo_abi::PAGE_SIZE
}

fn ipc_va(slot: usize) -> u64 {
    exo_abi::THREAD_IPC_BUFFER_BASE + slot as u64 * exo_abi::PAGE_SIZE
}

/// 在首次进入 EL0 前登记主线程。IPC 页已由 kmain 分配和映射。
pub fn install_initial(entry: u64, stack_top: u64, ipc_pa: u64) {
    let mut context = TrapFrame::ZERO;
    context.x[0] = exo_abi::USER_BOOT_INFO_VA;
    context.sp_el0 = stack_top;
    context.elr_el1 = entry;
    context.spsr_el1 = 0;
    unsafe {
        THREADS[0] = Thread {
            generation: THREADS[0].generation,
            owner: crate::task::current_owner(),
            state: ThreadState::Running,
            priority: DEFAULT_PRIORITY,
            ready_order: 0,
            context,
            stack_pa: 0,
            stack_pages: 0,
            stack_va: stack_top - exo_abi::USER_STACK_PAGES * exo_abi::PAGE_SIZE,
            ipc_pa,
            ipc_va: ipc_va(0),
        };
        CURRENT = 0;
        INITIALIZED = true;
    }
}

pub fn is_initialized() -> bool {
    unsafe { INITIALIZED }
}

pub fn current_slot() -> usize {
    unsafe { CURRENT }
}

pub fn current_ipc_va() -> u64 {
    unsafe { THREADS[CURRENT].ipc_va }
}

pub fn current_message() -> exo_abi::IpcMessage {
    unsafe { core::ptr::read(current_ipc_va() as *const exo_abi::IpcMessage) }
}

pub fn write_message(slot: usize, message: exo_abi::IpcMessage) {
    unsafe {
        core::ptr::write(THREADS[slot].ipc_va as *mut exo_abi::IpcMessage, message);
    }
}

pub fn save_current(frame: &TrapFrame) {
    unsafe {
        if INITIALIZED && THREADS[CURRENT].state != ThreadState::Free {
            THREADS[CURRENT].context = *frame;
        }
    }
}

pub fn create(entry: u64, arg: u64, priority: u64) -> u64 {
    if !is_initialized() || priority > u8::MAX as u64 {
        return exo_abi::SYS_ERR_INVALID;
    }
    let root = mmu::active_table();
    if !mmu::is_user_executable(root, entry) {
        return exo_abi::SYS_ERR_DENIED;
    }
    let slot = unsafe {
        (1..MAX_THREADS)
            .find(|&slot| matches!(THREADS[slot].state, ThreadState::Free | ThreadState::Exited))
    };
    let Some(slot) = slot else {
        return exo_abi::SYS_ERR_NO_SLOT;
    };

    let stack_pages = exo_abi::THREAD_STACK_PAGES;
    let Some(stack_pa) = mem::alloc_pages(stack_pages) else {
        return exo_abi::SYS_ERR_NO_MEMORY;
    };
    let Some(ipc_pa) = mem::alloc_page() else {
        mem::free_pages(stack_pa, stack_pages);
        return exo_abi::SYS_ERR_NO_MEMORY;
    };
    unsafe {
        core::ptr::write_bytes(
            stack_pa as *mut u8,
            0,
            (stack_pages * exo_abi::PAGE_SIZE) as usize,
        );
        core::ptr::write_bytes(ipc_pa as *mut u8, 0, exo_abi::PAGE_SIZE as usize);
    }
    let stack_va = stack_base(slot);
    if mmu::map_checked(root, stack_va, stack_pa, mmu::MMU_USER_RW, stack_pages).is_err() {
        mem::free_pages(stack_pa, stack_pages);
        mem::free_page(ipc_pa);
        return exo_abi::SYS_ERR_CONFLICT;
    }
    if mmu::map_checked(root, ipc_va(slot), ipc_pa, mmu::MMU_USER_RW, 1).is_err() {
        mmu::unmap(root, stack_va, stack_pages);
        mmu::flush_el1_tlb_range(stack_va, stack_pages);
        mem::free_pages(stack_pa, stack_pages);
        mem::free_page(ipc_pa);
        return exo_abi::SYS_ERR_CONFLICT;
    }

    unsafe {
        let generation = THREADS[slot].generation;
        let handle = make_handle(slot, generation);
        let mut context = TrapFrame::ZERO;
        context.x[0] = arg;
        context.x[1] = handle;
        context.x[2] = ipc_va(slot);
        context.sp_el0 = stack_va + stack_pages * exo_abi::PAGE_SIZE;
        context.elr_el1 = entry;
        context.spsr_el1 = 0;
        READY_CLOCK = READY_CLOCK.wrapping_add(1);
        THREADS[slot] = Thread {
            generation,
            owner: crate::task::current_owner(),
            state: ThreadState::Ready,
            priority: priority as u8,
            ready_order: READY_CLOCK,
            context,
            stack_pa,
            stack_pages,
            stack_va,
            ipc_pa,
            ipc_va: ipc_va(slot),
        };
        handle
    }
}

pub fn set_priority(handle: u64, priority: u64) -> u64 {
    if priority > u8::MAX as u64 {
        return exo_abi::SYS_ERR_INVALID;
    }
    let Some((slot, generation)) = decode_handle(handle) else {
        return exo_abi::SYS_ERR_INVALID;
    };
    unsafe {
        let thread = &mut THREADS[slot];
        if thread.owner != crate::task::current_owner()
            || thread.generation != generation
            || matches!(thread.state, ThreadState::Free | ThreadState::Exited)
        {
            return exo_abi::SYS_ERR_NOT_FOUND;
        }
        thread.priority = priority as u8;
    }
    0
}

pub fn take_next_ready() -> Option<usize> {
    unsafe {
        let mut selected = None;
        let mut slot = 0usize;
        while slot < MAX_THREADS {
            if THREADS[slot].state == ThreadState::Ready {
                selected = match selected {
                    None => Some(slot),
                    Some(best) => {
                        let candidate = THREADS[slot];
                        let current_best = THREADS[best];
                        if candidate.priority > current_best.priority
                            || (candidate.priority == current_best.priority
                                && candidate.ready_order < current_best.ready_order)
                        {
                            Some(slot)
                        } else {
                            Some(best)
                        }
                    }
                };
            }
            slot += 1;
        }
        selected
    }
}

fn activate(slot: usize) -> *mut TrapFrame {
    unsafe {
        CURRENT = slot;
        THREADS[slot].state = ThreadState::Running;
        &mut THREADS[slot].context
    }
}

fn wait_until_ready() -> usize {
    loop {
        if let Some(slot) = scheduler::next_ready() {
            return slot;
        }
        unsafe {
            core::arch::asm!(
                "msr daifclr, #2",
                "wfi",
                "msr daifset, #2",
                options(nomem, nostack)
            );
        }
    }
}

pub fn yield_current(frame: &TrapFrame) -> *mut TrapFrame {
    save_current(frame);
    unsafe {
        READY_CLOCK = READY_CLOCK.wrapping_add(1);
        THREADS[CURRENT].state = ThreadState::Ready;
        THREADS[CURRENT].ready_order = READY_CLOCK;
    }
    activate(wait_until_ready())
}

pub fn block_current(frame: &TrapFrame) -> *mut TrapFrame {
    save_current(frame);
    unsafe { THREADS[CURRENT].state = ThreadState::Blocked };
    activate(wait_until_ready())
}

pub fn wake(slot: usize, return_value: u64) {
    unsafe {
        if slot >= MAX_THREADS || THREADS[slot].state != ThreadState::Blocked {
            return;
        }
        THREADS[slot].context.x[0] = return_value;
        READY_CLOCK = READY_CLOCK.wrapping_add(1);
        THREADS[slot].ready_order = READY_CLOCK;
        THREADS[slot].state = ThreadState::Ready;
    }
}

pub fn exit_current(frame: &TrapFrame) -> Option<*mut TrapFrame> {
    save_current(frame);
    let slot = current_slot();
    crate::ipc::cancel_thread(slot);
    unsafe {
        let thread = THREADS[slot];
        if thread.stack_pages != 0 {
            mmu::unmap(mmu::active_table(), thread.stack_va, thread.stack_pages);
            mem::free_pages(thread.stack_pa, thread.stack_pages);
        }
        if thread.ipc_pa != 0 {
            mmu::unmap(mmu::active_table(), thread.ipc_va, 1);
            mem::free_page(thread.ipc_pa);
        }
        mmu::flush_el1_tlb();
        THREADS[slot] = Thread {
            generation: next_generation(thread.generation),
            state: ThreadState::Exited,
            ..Thread::EMPTY
        };
    }
    scheduler::next_ready().map(activate)
}

pub fn cleanup_all(root: u64) {
    if !is_initialized() {
        return;
    }
    unsafe {
        let mut slot = 0usize;
        while slot < MAX_THREADS {
            let thread = THREADS[slot];
            if thread.owner != 0 {
                if thread.stack_pages != 0 {
                    mmu::unmap(root, thread.stack_va, thread.stack_pages);
                    mem::free_pages(thread.stack_pa, thread.stack_pages);
                }
                if thread.ipc_pa != 0 {
                    mmu::unmap(root, thread.ipc_va, 1);
                    mem::free_page(thread.ipc_pa);
                }
                THREADS[slot] = Thread {
                    generation: next_generation(thread.generation),
                    ..Thread::EMPTY
                };
            }
            slot += 1;
        }
        INITIALIZED = false;
    }
}
