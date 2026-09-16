//! 单任务多线程的上下文与生命周期管理。

use crate::{mem, mmu, scheduler, trap::TrapFrame};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

pub const MAX_THREADS: usize = 16;
const MAX_GENERATION: u32 = 0x7fff_ffff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadState {
    Free,
    Ready,
    Running,
    Blocked,
    /// 已创建但尚未加入 Ready Queue 的线程。
    Suspended,
    Exited,
}

#[derive(Clone, Copy)]
struct Thread {
    generation: u32,
    owner: u32,
    creator_owner: u32,
    vspace_handle: u64,
    vspace_root: u64,
    state: ThreadState,
    context: TrapFrame,
    stack_pa: u64,
    stack_pages: u64,
    stack_va: u64,
    ipc_pa: u64,
    ipc_va: u64,
    affinity_cpu: u8,
    base_priority: u8,
    effective_priority: u8,
    max_control_priority: u8,
    running_cpu: u8,
}

impl Thread {
    const EMPTY: Self = Self {
        generation: 1,
        owner: 0,
        creator_owner: 0,
        vspace_handle: 0,
        vspace_root: 0,
        state: ThreadState::Free,
        context: TrapFrame::ZERO,
        stack_pa: 0,
        stack_pages: 0,
        stack_va: 0,
        ipc_pa: 0,
        ipc_va: 0,
        affinity_cpu: 0,
        base_priority: 0,
        effective_priority: 0,
        max_control_priority: 0,
        running_cpu: u8::MAX,
    };
}

static mut THREADS: [Thread; MAX_THREADS] = [Thread::EMPTY; MAX_THREADS];
const STATE_FREE: u8 = 0;
const STATE_READY: u8 = 1;
const STATE_RUNNING: u8 = 2;
const STATE_BLOCKED: u8 = 3;
const STATE_EXITED: u8 = 4;
const STATE_RESERVED: u8 = 5;
const STATE_SUSPENDED: u8 = 6;

// SMP热路径元数据不能直接读写`static mut THREADS`。创建者先用CAS把槽位
// 变为RESERVED，写完context/栈/亲和性后以Release发布READY；目标CPU用
// Acquire观察状态后才读取其余普通字段。运行时间与FIFO序号会被其他CPU
// 查询，因此分别使用AtomicU64，避免撕裂和Rust数据竞争。
static STATES: [AtomicU8; MAX_THREADS] = [const { AtomicU8::new(STATE_FREE) }; MAX_THREADS];
static READY_SEQUENCES: [AtomicU64; MAX_THREADS] = [const { AtomicU64::new(0) }; MAX_THREADS];
static RUNTIME_TICKS: [AtomicU64; MAX_THREADS] = [const { AtomicU64::new(0) }; MAX_THREADS];
static RUNNING_CPUS: [AtomicU8; MAX_THREADS] = [const { AtomicU8::new(u8::MAX) }; MAX_THREADS];
static EFFECTIVE_PRIORITIES: [AtomicU8; MAX_THREADS] = [const { AtomicU8::new(0) }; MAX_THREADS];
static BASE_PRIORITIES: [AtomicU8; MAX_THREADS] = [const { AtomicU8::new(0) }; MAX_THREADS];
// 每个CPU只写自己的CURRENT槽，但四个u8位于同一缓存行；普通static mut
// 写在Rust内存模型中仍是数据竞争，也可能因整缓存行回写丢失其他核更新。
// 原子字节让current_slot成为真正可并发访问的per-CPU状态。
static CURRENT: [AtomicU8; exo_abi::MAX_CPUS] = [
    AtomicU8::new(u8::MAX),
    AtomicU8::new(u8::MAX),
    AtomicU8::new(u8::MAX),
    AtomicU8::new(u8::MAX),
];
static INITIALIZED: AtomicBool = AtomicBool::new(false);

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
    let owner = crate::task::current_owner();
    let vspace_handle = crate::vspace::current_handle();
    let vspace_root = mmu::active_table();
    let mut context = TrapFrame::ZERO;
    context.x[0] = exo_abi::USER_BOOT_INFO_VA;
    context.sp_el0 = stack_top;
    context.elr_el1 = entry;
    context.spsr_el1 = 0;
    unsafe {
        THREADS[0] = Thread {
            generation: THREADS[0].generation,
            owner,
            creator_owner: owner,
            vspace_handle,
            vspace_root,
            state: ThreadState::Running,
            context,
            stack_pa: 0,
            stack_pages: 0,
            stack_va: stack_top - exo_abi::USER_STACK_PAGES * exo_abi::PAGE_SIZE,
            ipc_pa,
            ipc_va: ipc_va(0),
            affinity_cpu: 0,
            base_priority: 32,
            effective_priority: 32,
            max_control_priority: 63,
            running_cpu: 0,
        };
    }
    BASE_PRIORITIES[0].store(32, Ordering::Relaxed);
    EFFECTIVE_PRIORITIES[0].store(32, Ordering::Relaxed);
    RUNNING_CPUS[0].store(0, Ordering::Relaxed);
    RUNTIME_TICKS[0].store(0, Ordering::Relaxed);
    READY_SEQUENCES[0].store(0, Ordering::Relaxed);
    STATES[0].store(STATE_RUNNING, Ordering::Release);
    CURRENT[0].store(0, Ordering::Release);
    INITIALIZED.store(true, Ordering::Release);
}

pub fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}

pub fn current_slot() -> usize {
    let cpu = crate::arch::aarch64::cpu::id();
    CURRENT[cpu].load(Ordering::Acquire) as usize
}

/// 读取指定CPU当前运行槽，用于远程唤醒时判断是否真的需要发送抢占SGI。
pub fn current_slot_on(cpu: usize) -> usize {
    if cpu >= exo_abi::MAX_CPUS {
        return usize::MAX;
    }
    CURRENT[cpu].load(Ordering::Acquire) as usize
}

/// 返回当前线程的资源 owner。任务表在多 VSpace 阶段仍保留单一任务入口，
/// 但调度器切换到子 VSpace 线程后，Frame/IPC 检查必须能找到真实线程对象。
pub fn owner_of_current() -> u32 {
    if !is_initialized() {
        return 0;
    }
    let slot = current_slot();
    if slot >= MAX_THREADS {
        return 0;
    }
    unsafe { THREADS[slot].owner }
}

pub fn resolve_handle(handle: u64) -> Result<usize, u64> {
    let Some((slot, generation)) = decode_handle(handle) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    unsafe {
        let thread = THREADS[slot];
        let state = STATES[slot].load(Ordering::Acquire);
        if thread.owner != crate::task::current_owner()
            || thread.generation != generation
            || matches!(state, STATE_FREE | STATE_EXITED | STATE_RESERVED)
        {
            return Err(exo_abi::SYS_ERR_NOT_FOUND);
        }
    }
    Ok(slot)
}

fn resolve_control_handle(handle: u64) -> Result<usize, u64> {
    let Some((slot, generation)) = decode_handle(handle) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    let caller = crate::task::current_owner();
    unsafe {
        let thread = THREADS[slot];
        let state = STATES[slot].load(Ordering::Acquire);
        if thread.creator_owner != caller
            || thread.generation != generation
            || matches!(state, STATE_FREE | STATE_EXITED | STATE_RESERVED)
        {
            return Err(exo_abi::SYS_ERR_NOT_FOUND);
        }
    }
    Ok(slot)
}

pub fn is_ready(slot: usize) -> bool {
    slot < MAX_THREADS && STATES[slot].load(Ordering::Acquire) == STATE_READY
}

pub fn current_ipc_va() -> u64 {
    unsafe { THREADS[current_slot()].ipc_va }
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
    let current = current_slot();
    unsafe {
        if INITIALIZED.load(Ordering::Acquire)
            && current < MAX_THREADS
            && STATES[current].load(Ordering::Acquire) != STATE_FREE
        {
            THREADS[current].context = *frame;
        }
    }
}

pub fn create(entry: u64, arg: u64, cpu: u64, priority: u64, mcp: u64) -> u64 {
    if !is_initialized()
        || cpu >= crate::arch::aarch64::smp::cpu_count() as u64
        || priority > exo_abi::MAX_THREAD_PRIORITY as u64
        || mcp > exo_abi::MAX_THREAD_PRIORITY as u64
        || priority > mcp
    {
        return exo_abi::SYS_ERR_INVALID;
    }
    let root = mmu::active_table();
    if !mmu::is_user_executable(root, entry) {
        return exo_abi::SYS_ERR_DENIED;
    }
    let mut reserved = None;
    for slot in 1..MAX_THREADS {
        let state = STATES[slot].load(Ordering::Acquire);
        if matches!(state, STATE_FREE | STATE_EXITED)
            && STATES[slot]
                .compare_exchange(state, STATE_RESERVED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            reserved = Some((slot, state));
            break;
        }
    }
    let Some((slot, previous_state)) = reserved else {
        return exo_abi::SYS_ERR_NO_SLOT;
    };

    let stack_pages = exo_abi::THREAD_STACK_PAGES;
    let Some(stack_pa) = mem::alloc_pages(stack_pages) else {
        STATES[slot].store(previous_state, Ordering::Release);
        return exo_abi::SYS_ERR_NO_MEMORY;
    };
    let Some(ipc_pa) = mem::alloc_page() else {
        mem::free_pages(stack_pa, stack_pages);
        STATES[slot].store(previous_state, Ordering::Release);
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
        STATES[slot].store(previous_state, Ordering::Release);
        return exo_abi::SYS_ERR_CONFLICT;
    }
    if mmu::map_checked(root, ipc_va(slot), ipc_pa, mmu::MMU_USER_RW, 1).is_err() {
        mmu::unmap(root, stack_va, stack_pages);
        mmu::flush_el1_tlb_range(stack_va, stack_pages);
        mem::free_pages(stack_pa, stack_pages);
        mem::free_page(ipc_pa);
        STATES[slot].store(previous_state, Ordering::Release);
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
        let vspace_handle = crate::vspace::current_handle();
        if crate::vspace::add_thread(vspace_handle).is_err() {
            mmu::unmap(root, stack_va, stack_pages);
            mmu::unmap(root, ipc_va(slot), 1);
            mmu::flush_el1_tlb_range(stack_va, stack_pages);
            mem::free_pages(stack_pa, stack_pages);
            mem::free_page(ipc_pa);
            STATES[slot].store(previous_state, Ordering::Release);
            return exo_abi::SYS_ERR_BUSY;
        }
        THREADS[slot] = Thread {
            generation,
            owner: crate::task::current_owner(),
            creator_owner: crate::task::current_owner(),
            vspace_handle,
            vspace_root: root,
            state: ThreadState::Ready,
            context,
            stack_pa,
            stack_pages,
            stack_va,
            ipc_pa,
            ipc_va: ipc_va(slot),
            affinity_cpu: cpu as u8,
            base_priority: priority as u8,
            effective_priority: priority as u8,
            max_control_priority: mcp as u8,
            running_cpu: u8::MAX,
        };
        BASE_PRIORITIES[slot].store(priority as u8, Ordering::Relaxed);
        EFFECTIVE_PRIORITIES[slot].store(priority as u8, Ordering::Relaxed);
        RUNNING_CPUS[slot].store(u8::MAX, Ordering::Relaxed);
        READY_SEQUENCES[slot].store(0, Ordering::Relaxed);
        RUNTIME_TICKS[slot].store(0, Ordering::Relaxed);
        // Release把上面写入的普通context、栈和亲和性发布给目标CPU。
        STATES[slot].store(STATE_READY, Ordering::Release);
        scheduler::on_thread_ready(slot);
        handle
    }
}

/// 在指定 VSpace 中创建一个尚未运行的线程。
///
/// 配置结构位于调用者当前地址空间，Kernel 在 SVC 期间复制后才使用；entry
/// 和 stack_pointer 则在目标地址空间的页表中分别检查 RX/RW 权限。这样父级
/// libOS 可以先完成全部 Frame 映射，最后显式 THREAD_START。
pub fn create_in(target_vspace: u64, config_va: u64) -> u64 {
    let caller = crate::task::current_owner();
    let current_root = mmu::active_table();
    if caller == 0
        || (config_va & 7) != 0
        || !mmu::is_user_readable(
            current_root,
            config_va,
            core::mem::size_of::<exo_abi::ThreadCreateConfig>() as u64,
        )
    {
        return exo_abi::SYS_ERR_INVALID;
    }
    let config =
        unsafe { core::ptr::read_unaligned(config_va as *const exo_abi::ThreadCreateConfig) };
    if config.reserved != 0
        || config.cpu as usize >= crate::arch::aarch64::smp::cpu_count()
        || config.priority > exo_abi::MAX_THREAD_PRIORITY
        || config.max_control_priority > exo_abi::MAX_THREAD_PRIORITY
        || config.priority > config.max_control_priority
    {
        return exo_abi::SYS_ERR_INVALID;
    }
    let Ok((target_root, _)) = crate::vspace::resolve_for_owner(caller, target_vspace) else {
        return exo_abi::SYS_ERR_DENIED;
    };
    if !mmu::is_user_executable(target_root, config.entry)
        || config.stack_pointer < exo_abi::PAGE_SIZE
        || !mmu::is_user_writable(target_root, config.stack_pointer - 1, 1)
    {
        return exo_abi::SYS_ERR_DENIED;
    }

    let mut reserved = None;
    for slot in 1..MAX_THREADS {
        let state = STATES[slot].load(Ordering::Acquire);
        if matches!(state, STATE_FREE | STATE_EXITED)
            && STATES[slot]
                .compare_exchange(state, STATE_RESERVED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            reserved = Some((slot, state));
            break;
        }
    }
    let Some((slot, previous_state)) = reserved else {
        return exo_abi::SYS_ERR_NO_SLOT;
    };
    let Some(ipc_pa) = mem::alloc_page() else {
        STATES[slot].store(previous_state, Ordering::Release);
        return exo_abi::SYS_ERR_NO_MEMORY;
    };
    unsafe { core::ptr::write_bytes(ipc_pa as *mut u8, 0, exo_abi::PAGE_SIZE as usize) };
    let target_ipc_va = ipc_va(slot);
    if mmu::map_checked(target_root, target_ipc_va, ipc_pa, mmu::MMU_USER_RW, 1).is_err() {
        mem::free_page(ipc_pa);
        STATES[slot].store(previous_state, Ordering::Release);
        return exo_abi::SYS_ERR_CONFLICT;
    }
    if crate::vspace::add_thread(target_vspace).is_err() {
        mmu::unmap(target_root, target_ipc_va, 1);
        mmu::flush_el1_tlb_range(target_ipc_va, 1);
        mem::free_page(ipc_pa);
        STATES[slot].store(previous_state, Ordering::Release);
        return exo_abi::SYS_ERR_BUSY;
    }

    unsafe {
        let generation = THREADS[slot].generation;
        let handle = make_handle(slot, generation);
        let mut context = TrapFrame::ZERO;
        context.x[0] = config.arg0;
        context.x[1] = config.arg1;
        context.x[2] = handle;
        context.x[3] = target_ipc_va;
        context.sp_el0 = config.stack_pointer;
        context.elr_el1 = config.entry;
        THREADS[slot] = Thread {
            generation,
            owner: crate::vspace::resource_owner(caller, target_vspace).unwrap_or(caller),
            creator_owner: caller,
            vspace_handle: target_vspace,
            vspace_root: target_root,
            state: ThreadState::Suspended,
            context,
            stack_pa: 0,
            stack_pages: 0,
            stack_va: 0,
            ipc_pa,
            ipc_va: target_ipc_va,
            affinity_cpu: config.cpu,
            base_priority: config.priority,
            effective_priority: config.priority,
            max_control_priority: config.max_control_priority,
            running_cpu: u8::MAX,
        };
        BASE_PRIORITIES[slot].store(config.priority, Ordering::Relaxed);
        EFFECTIVE_PRIORITIES[slot].store(config.priority, Ordering::Relaxed);
        RUNNING_CPUS[slot].store(u8::MAX, Ordering::Relaxed);
        READY_SEQUENCES[slot].store(0, Ordering::Relaxed);
        RUNTIME_TICKS[slot].store(0, Ordering::Relaxed);
        STATES[slot].store(STATE_SUSPENDED, Ordering::Release);
        handle
    }
}

/// 将 Suspended 线程发布到指定 CPU 的 Ready Queue。
pub fn start(handle: u64) -> u64 {
    let Ok(slot) = resolve_control_handle(handle) else {
        return exo_abi::SYS_ERR_NOT_FOUND;
    };
    if STATES[slot]
        .compare_exchange(
            STATE_SUSPENDED,
            STATE_READY,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return exo_abi::SYS_ERR_BUSY;
    }
    unsafe { THREADS[slot].state = ThreadState::Ready };
    scheduler::on_thread_ready(slot);
    0
}

/// 暂停一个还未运行或已经在 Ready Queue 的线程。正在运行的线程不能由
/// 自己的控制者无条件拔走，第一版统一返回 BUSY。
pub fn suspend(handle: u64) -> u64 {
    let Ok(slot) = resolve_control_handle(handle) else {
        return exo_abi::SYS_ERR_NOT_FOUND;
    };
    if STATES[slot]
        .compare_exchange(
            STATE_READY,
            STATE_SUSPENDED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        unsafe { THREADS[slot].state = ThreadState::Suspended };
        scheduler::on_thread_exit(slot);
        return 0;
    }
    if STATES[slot].load(Ordering::Acquire) == STATE_SUSPENDED {
        0
    } else {
        exo_abi::SYS_ERR_BUSY
    }
}

/// 销毁一个非运行线程，并回收它自动分配的 IPC buffer。
pub fn destroy(handle: u64) -> u64 {
    let Ok(slot) = resolve_control_handle(handle) else {
        return exo_abi::SYS_ERR_NOT_FOUND;
    };
    let state = STATES[slot].load(Ordering::Acquire);
    if state == STATE_RUNNING || state == STATE_BLOCKED {
        return exo_abi::SYS_ERR_BUSY;
    }
    if STATES[slot]
        .compare_exchange(state, STATE_RESERVED, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return exo_abi::SYS_ERR_BUSY;
    }
    unsafe {
        let thread = THREADS[slot];
        if thread.ipc_pa != 0 {
            mmu::unmap(thread.vspace_root, thread.ipc_va, 1);
            mem::free_page(thread.ipc_pa);
        }
        mmu::flush_el1_tlb_range(thread.ipc_va, 1);
        crate::vspace::remove_thread(thread.vspace_handle);
        THREADS[slot] = Thread {
            generation: next_generation(thread.generation),
            state: ThreadState::Exited,
            ..Thread::EMPTY
        };
        BASE_PRIORITIES[slot].store(0, Ordering::Relaxed);
        EFFECTIVE_PRIORITIES[slot].store(0, Ordering::Relaxed);
        STATES[slot].store(STATE_EXITED, Ordering::Release);
    }
    scheduler::on_thread_exit(slot);
    0
}

pub fn activate(slot: usize) -> *mut TrapFrame {
    let cpu = crate::arch::aarch64::cpu::id();
    let handle = unsafe { THREADS[slot].vspace_handle };
    if crate::vspace::switch_to(handle).is_err() {
        crate::uart::puts("[exo] scheduler rejected VSpace switch\r\n");
        loop {
            unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
        }
    }
    unsafe {
        THREADS[slot].state = ThreadState::Running;
        THREADS[slot].running_cpu = cpu as u8;
        RUNNING_CPUS[slot].store(cpu as u8, Ordering::Relaxed);
        STATES[slot].store(STATE_RUNNING, Ordering::Release);
        CURRENT[cpu].store(slot as u8, Ordering::Release);
        &mut THREADS[slot].context
    }
}

pub fn make_current_ready(frame: &TrapFrame) {
    save_current(frame);
    let current = current_slot();
    unsafe {
        THREADS[current].state = ThreadState::Ready;
        THREADS[current].running_cpu = u8::MAX;
    }
    RUNNING_CPUS[current].store(u8::MAX, Ordering::Relaxed);
    STATES[current].store(STATE_READY, Ordering::Release);
    scheduler::on_thread_ready(current);
}

pub fn make_current_blocked(frame: &TrapFrame) {
    save_current(frame);
    let current = current_slot();
    unsafe {
        THREADS[current].state = ThreadState::Blocked;
        THREADS[current].running_cpu = u8::MAX;
    }
    RUNNING_CPUS[current].store(u8::MAX, Ordering::Relaxed);
    STATES[current].store(STATE_BLOCKED, Ordering::Release);
}

pub fn yield_current(frame: &TrapFrame) -> *mut TrapFrame {
    scheduler::yield_current(frame)
}

pub fn wake(slot: usize, return_value: u64) {
    if slot >= MAX_THREADS
        || STATES[slot]
            .compare_exchange(
                STATE_BLOCKED,
                STATE_RESERVED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
    {
        return;
    }
    unsafe {
        THREADS[slot].context.x[0] = return_value;
        THREADS[slot].state = ThreadState::Ready;
        THREADS[slot].running_cpu = u8::MAX;
    }
    RUNNING_CPUS[slot].store(u8::MAX, Ordering::Relaxed);
    STATES[slot].store(STATE_READY, Ordering::Release);
    scheduler::on_thread_ready(slot);
}

pub fn exit_current(frame: &TrapFrame) -> Option<*mut TrapFrame> {
    save_current(frame);
    let slot = current_slot();
    crate::ipc::cancel_thread(slot);
    scheduler::on_thread_exit(slot);
    unsafe {
        let thread = THREADS[slot];
        if thread.stack_pages != 0 {
            mmu::unmap(thread.vspace_root, thread.stack_va, thread.stack_pages);
            mem::free_pages(thread.stack_pa, thread.stack_pages);
        }
        if thread.ipc_pa != 0 {
            mmu::unmap(thread.vspace_root, thread.ipc_va, 1);
            mem::free_page(thread.ipc_pa);
        }
        crate::vspace::remove_thread(thread.vspace_handle);
        mmu::flush_el1_tlb();
        THREADS[slot] = Thread {
            generation: next_generation(thread.generation),
            state: ThreadState::Exited,
            ..Thread::EMPTY
        };
    }
    RUNNING_CPUS[slot].store(u8::MAX, Ordering::Relaxed);
    STATES[slot].store(STATE_EXITED, Ordering::Release);
    let has_live_thread = unsafe {
        let mut live = false;
        let mut index = 0usize;
        while index < MAX_THREADS {
            let thread = THREADS[index];
            let state = STATES[index].load(Ordering::Acquire);
            if thread.creator_owner == crate::task::current_owner()
                && !matches!(state, STATE_FREE | STATE_EXITED)
            {
                live = true;
                break;
            }
            index += 1;
        }
        live
    };
    if has_live_thread {
        Some(scheduler::schedule_after_block())
    } else {
        None
    }
}

pub fn affinity(slot: usize) -> usize {
    unsafe { THREADS[slot].affinity_cpu as usize }
}

pub fn effective_priority(slot: usize) -> u8 {
    EFFECTIVE_PRIORITIES[slot].load(Ordering::Acquire)
}

pub fn base_priority(slot: usize) -> u8 {
    BASE_PRIORITIES[slot].load(Ordering::Acquire)
}

pub fn ready_sequence(slot: usize) -> u64 {
    READY_SEQUENCES[slot].load(Ordering::Acquire)
}

pub fn set_ready_sequence(slot: usize, sequence: u64) {
    READY_SEQUENCES[slot].store(sequence, Ordering::Release)
}

pub fn set_effective_priority(slot: usize, priority: u8) {
    unsafe { THREADS[slot].effective_priority = priority };
    EFFECTIVE_PRIORITIES[slot].store(priority, Ordering::Release);
}

pub fn is_running(slot: usize) -> bool {
    slot < MAX_THREADS && STATES[slot].load(Ordering::Acquire) == STATE_RUNNING
}

pub fn running_cpu(slot: usize) -> usize {
    RUNNING_CPUS[slot].load(Ordering::Acquire) as usize
}

/// 调度器切换前用于核对保存上下文。合法EL0线程的返回PC绝不能为0。
pub fn saved_pc(slot: usize) -> u64 {
    unsafe { THREADS[slot].context.elr_el1 }
}

pub fn current_priority() -> u8 {
    effective_priority(current_slot())
}

pub fn set_priority(handle: u64, priority: u64) -> u64 {
    if priority > exo_abi::MAX_THREAD_PRIORITY as u64 {
        return exo_abi::SYS_ERR_INVALID;
    }
    let target = match resolve_handle(handle) {
        Ok(slot) => slot,
        Err(error) => return error,
    };
    let caller_mcp = unsafe { THREADS[current_slot()].max_control_priority };
    if priority as u8 > caller_mcp {
        return exo_abi::SYS_ERR_DENIED;
    }
    unsafe { THREADS[target].base_priority = priority as u8 };
    BASE_PRIORITIES[target].store(priority as u8, Ordering::Release);
    crate::ipc::recompute_inherited_priorities();
    scheduler::priority_changed(target);
    0
}

pub fn runtime(handle: u64) -> u64 {
    let slot = match resolve_handle(handle) {
        Ok(slot) => slot,
        Err(error) => return error,
    };
    let accumulated = RUNTIME_TICKS[slot].load(Ordering::Acquire);
    accumulated.wrapping_add(crate::scheduler::priority::active_runtime_ticks(slot))
}

pub fn charge(slot: usize, ticks: u64) {
    RUNTIME_TICKS[slot].fetch_add(ticks, Ordering::AcqRel);
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
                    mmu::unmap(
                        thread.vspace_root.max(root),
                        thread.stack_va,
                        thread.stack_pages,
                    );
                    mem::free_pages(thread.stack_pa, thread.stack_pages);
                }
                if thread.ipc_pa != 0 {
                    mmu::unmap(thread.vspace_root.max(root), thread.ipc_va, 1);
                    mem::free_page(thread.ipc_pa);
                }
                crate::vspace::remove_thread(thread.vspace_handle);
                THREADS[slot] = Thread {
                    generation: next_generation(thread.generation),
                    ..Thread::EMPTY
                };
                BASE_PRIORITIES[slot].store(0, Ordering::Relaxed);
                EFFECTIVE_PRIORITIES[slot].store(0, Ordering::Relaxed);
                RUNNING_CPUS[slot].store(u8::MAX, Ordering::Relaxed);
                READY_SEQUENCES[slot].store(0, Ordering::Relaxed);
                RUNTIME_TICKS[slot].store(0, Ordering::Relaxed);
                STATES[slot].store(STATE_FREE, Ordering::Release);
            }
            slot += 1;
        }
    }
    for current in &CURRENT {
        current.store(u8::MAX, Ordering::Release);
    }
    INITIALIZED.store(false, Ordering::Release);
}
