//! 综合实验中的Frame/VSpace子测试。
//!
//! 本文件检查普通内存资源接口及共享VSpace的跨核TLB失效，不操作设备。
//! 成功后返回统一runner继续执行IPC、静态优先级和机器人并发测试。

use crate::memory::{Frame, Rights};
use core::sync::atomic::{AtomicU64, Ordering};

const TEST_VA: u64 = exo_abi::FRAME_ARENA_BASE;
const SECOND_VA: u64 = TEST_VA + 0x20_000;
const SHOOTDOWN_VA: u64 = TEST_VA + 0x40_000;
const OLD_VALUE: u64 = 0x1111_2222_3333_4444;
const NEW_VALUE: u64 = 0xaaaa_bbbb_cccc_dddd;

static SHOOTDOWN_PHASE: AtomicU64 = AtomicU64::new(0);
static REMOTE_FIRST: AtomicU64 = AtomicU64::new(0);
static REMOTE_SECOND: AtomicU64 = AtomicU64::new(0);

fn expect_error<T>(result: Result<T, u64>, wanted: u64, stage: u64) -> Result<(), u64> {
    match result {
        Err(error) if error == wanted => Ok(()),
        Err(error) => {
            crate::runtime::puts(b"[libos] Frame error mismatch wanted=");
            crate::runtime::hex(wanted);
            crate::runtime::puts(b" actual=");
            crate::runtime::hex(error);
            crate::runtime::puts(b"\r\n");
            Err(stage)
        }
        Ok(_) => {
            crate::runtime::puts(b"[libos] Frame expected rejection but syscall succeeded\r\n");
            Err(stage)
        }
    }
}

fn smoke() -> Result<(), u64> {
    crate::runtime::puts(b"[libos] Frame + VSpace smoke start\r\n");

    expect_error(
        crate::runtime::frame_alloc(0, 1),
        exo_abi::SYS_ERR_INVALID,
        1,
    )?;
    expect_error(
        crate::runtime::frame_alloc(1, 3),
        exo_abi::SYS_ERR_INVALID,
        2,
    )?;
    expect_error(
        crate::runtime::frame_alloc(4097, 1),
        exo_abi::SYS_ERR_INVALID,
        28,
    )?;
    expect_error(
        crate::runtime::frame_alloc(1, 1024),
        exo_abi::SYS_ERR_INVALID,
        29,
    )?;

    let frame = Frame::allocate(4, 4).map_err(|_| 3u64)?;
    if frame.pages() != 4 {
        return Err(4);
    }
    let stale_frame = frame.raw_handle();
    let mapping = frame
        .map(1, 2, TEST_VA, Rights::READ_WRITE)
        .map_err(|_| 5u64)?;
    if mapping.len() != 2 * exo_abi::PAGE_SIZE as usize {
        return Err(6);
    }

    let ptr = mapping.as_ptr();
    let mut index = 0usize;
    while index < mapping.len() {
        if unsafe { ptr.add(index).read_volatile() } != 0 {
            return Err(7);
        }
        index += 257;
    }
    unsafe {
        ptr.write_volatile(0x5a);
        ptr.add(mapping.len() - 1).write_volatile(0xa5);
        if ptr.read_volatile() != 0x5a || ptr.add(mapping.len() - 1).read_volatile() != 0xa5 {
            return Err(8);
        }
    }

    expect_error(
        frame.map(0, 1, TEST_VA, Rights::READ),
        exo_abi::SYS_ERR_CONFLICT,
        9,
    )?;
    expect_error(
        crate::runtime::frame_map(
            frame.raw_handle(),
            4,
            1,
            SECOND_VA,
            exo_abi::FRAME_RIGHT_READ,
        ),
        exo_abi::SYS_ERR_INVALID,
        10,
    )?;
    expect_error(
        crate::runtime::frame_map(
            frame.raw_handle(),
            0,
            0,
            SECOND_VA,
            exo_abi::FRAME_RIGHT_READ,
        ),
        exo_abi::SYS_ERR_INVALID,
        30,
    )?;
    expect_error(
        crate::runtime::frame_map(
            frame.raw_handle(),
            0,
            1,
            exo_abi::FRAME_ARENA_END,
            exo_abi::FRAME_RIGHT_READ,
        ),
        exo_abi::SYS_ERR_INVALID,
        31,
    )?;
    expect_error(
        crate::runtime::frame_map(
            frame.raw_handle(),
            u64::MAX,
            2,
            SECOND_VA,
            exo_abi::FRAME_RIGHT_READ,
        ),
        exo_abi::SYS_ERR_INVALID,
        32,
    )?;
    expect_error(
        crate::runtime::frame_map(
            frame.raw_handle(),
            0,
            1,
            SECOND_VA,
            exo_abi::FRAME_RIGHT_WRITE,
        ),
        exo_abi::SYS_ERR_DENIED,
        11,
    )?;
    expect_error(
        crate::runtime::frame_map(
            frame.raw_handle(),
            0,
            1,
            SECOND_VA,
            exo_abi::FRAME_RIGHT_READ | exo_abi::FRAME_RIGHT_EXECUTE,
        ),
        exo_abi::SYS_ERR_DENIED,
        12,
    )?;
    expect_error(
        crate::runtime::frame_map(
            frame.raw_handle(),
            0,
            1,
            SECOND_VA,
            exo_abi::FRAME_RIGHT_READ | exo_abi::FRAME_RIGHT_WRITE | exo_abi::FRAME_RIGHT_EXECUTE,
        ),
        exo_abi::SYS_ERR_DENIED,
        23,
    )?;
    expect_error(
        crate::runtime::frame_free(frame.raw_handle()),
        exo_abi::SYS_ERR_BUSY,
        13,
    )?;

    let stale_mapping = mapping.raw_handle();
    mapping.unmap().map_err(|_| 14u64)?;
    expect_error(
        crate::runtime::frame_unmap(stale_mapping),
        exo_abi::SYS_ERR_NOT_FOUND,
        15,
    )?;
    let executable = frame
        .map(0, 1, SECOND_VA, Rights::READ_EXECUTE)
        .map_err(|_| 24u64)?;
    executable.unmap().map_err(|_| 25u64)?;
    frame.free().map_err(|_| 16u64)?;
    expect_error(
        crate::runtime::frame_free(stale_frame),
        exo_abi::SYS_ERR_NOT_FOUND,
        17,
    )?;
    expect_error(
        crate::runtime::frame_map(stale_frame, 0, 1, SECOND_VA, exo_abi::FRAME_RIGHT_READ),
        exo_abi::SYS_ERR_NOT_FOUND,
        18,
    )?;
    expect_error(
        crate::runtime::frame_free(exo_abi::FrameHandle(0x1234_5678)),
        // 该伪造值没有高32位generation，不是“曾经存在但已过期”的
        // Handle，而是连编码格式都不合法，因此应返回INVALID。
        exo_abi::SYS_ERR_INVALID,
        19,
    )?;

    let reused = Frame::allocate(1, 1).map_err(|_| 20u64)?;
    if reused.raw_handle() == stale_frame {
        return Err(21);
    }
    reused.free().map_err(|_| 22u64)?;
    crate::runtime::puts(b"[libos] Frame + VSpace smoke passed\r\n");
    Ok(())
}

/// CPU1先访问旧映射，把`SHOOTDOWN_VA`的地址翻译装入本地TLB。
///
/// phase=2由CPU0在同一VA换成不同物理Frame后发布；第二次读取必须经过
/// 新页表翻译得到NEW_VALUE。这里使用volatile，防止编译器复用第一次读取值。
extern "C" fn shootdown_reader(_arg: u64, _thread: u64, _ipc: u64) -> ! {
    let actual_cpu = crate::thread::current_cpu();
    if actual_cpu != 1 {
        REMOTE_FIRST.store(u64::MAX, Ordering::Release);
        SHOOTDOWN_PHASE.store(3, Ordering::Release);
        crate::thread::exit(1);
    }

    let pointer = SHOOTDOWN_VA as *const u64;
    REMOTE_FIRST.store(unsafe { pointer.read_volatile() }, Ordering::Release);
    SHOOTDOWN_PHASE.store(1, Ordering::Release);
    while SHOOTDOWN_PHASE.load(Ordering::Acquire) < 2 {
        core::hint::spin_loop();
    }
    REMOTE_SECOND.store(unsafe { pointer.read_volatile() }, Ordering::Release);
    SHOOTDOWN_PHASE.store(3, Ordering::Release);
    crate::thread::exit(0)
}

fn wait_phase(wanted: u64, stage: u64) -> Result<(), u64> {
    let deadline = crate::runtime::counter()
        .wrapping_add(crate::runtime::counter_frequency().saturating_div(2));
    while SHOOTDOWN_PHASE.load(Ordering::Acquire) < wanted
        && (crate::runtime::counter().wrapping_sub(deadline) as i64) < 0
    {
        core::hint::spin_loop();
    }
    if SHOOTDOWN_PHASE.load(Ordering::Acquire) < wanted {
        Err(stage)
    } else {
        Ok(())
    }
}

fn smp_tlb_shootdown() -> Result<(), u64> {
    SHOOTDOWN_PHASE.store(0, Ordering::Release);
    REMOTE_FIRST.store(0, Ordering::Release);
    REMOTE_SECOND.store(0, Ordering::Release);

    let old_frame = Frame::allocate(1, 1).map_err(|_| 33u64)?;
    let old_mapping = old_frame
        .map(0, 1, SHOOTDOWN_VA, Rights::READ_WRITE)
        .map_err(|_| 34u64)?;
    unsafe { (old_mapping.as_ptr() as *mut u64).write_volatile(OLD_VALUE) };

    let reader = crate::thread::Thread::spawn(
        shootdown_reader,
        0,
        crate::thread::ThreadConfig::new(1, 45, 45),
    )
    .map_err(|_| 35u64)?;
    wait_phase(1, 36)?;
    if REMOTE_FIRST.load(Ordering::Acquire) != OLD_VALUE {
        return Err(37);
    }

    // 只撤销旧映射，不释放old_frame，保证下面new_frame得到不同PA。
    // FRAME_UNMAP和FRAME_MAP都会执行Inner Shareable TLBI；CPU1不能继续
    // 使用第一次读取时缓存的VA到旧PA翻译。
    old_mapping.unmap().map_err(|_| 38u64)?;
    let new_frame = Frame::allocate(1, 1).map_err(|_| 39u64)?;
    let new_mapping = new_frame
        .map(0, 1, SHOOTDOWN_VA, Rights::READ_WRITE)
        .map_err(|_| 40u64)?;
    unsafe { (new_mapping.as_ptr() as *mut u64).write_volatile(NEW_VALUE) };
    SHOOTDOWN_PHASE.store(2, Ordering::Release);
    wait_phase(3, 41)?;
    if REMOTE_SECOND.load(Ordering::Acquire) != NEW_VALUE {
        return Err(42);
    }

    // 等待线程对象进入Exited并被Kernel回收，避免后续测试过早复用线程slot。
    let deadline = crate::runtime::counter()
        .wrapping_add(crate::runtime::counter_frequency().saturating_div(2));
    while reader.runtime_ticks() != Err(exo_abi::SYS_ERR_NOT_FOUND)
        && (crate::runtime::counter().wrapping_sub(deadline) as i64) < 0
    {
        core::hint::spin_loop();
    }
    if reader.runtime_ticks() != Err(exo_abi::SYS_ERR_NOT_FOUND) {
        return Err(43);
    }

    new_mapping.unmap().map_err(|_| 44u64)?;
    new_frame.free().map_err(|_| 45u64)?;
    old_frame.free().map_err(|_| 46u64)?;
    crate::runtime::puts(b"[libos] SMP TLB shootdown passed\r\n");
    Ok(())
}

fn leave_resources_for_exit_cleanup() -> Result<(), u64> {
    let frame = Frame::allocate(3, 1).map_err(|_| 26u64)?;
    let mapping = frame
        .map(0, 1, TEST_VA, Rights::READ_WRITE)
        .map_err(|_| 27u64)?;
    unsafe { mapping.as_ptr().write_volatile(0x7e) };

    // 这些封装当前没有Drop；故意不调用UNMAP/FREE，用来验收SYS_EXIT兜底回收。
    core::mem::forget(mapping);
    core::mem::forget(frame);
    crate::runtime::puts(b"[libos] leaving 3 Frame pages for SYS_EXIT cleanup\r\n");
    Ok(())
}

pub fn run() {
    if let Err(stage) = smoke() {
        crate::runtime::puts(b"[libos] Frame smoke failed stage=");
        crate::runtime::hex(stage);
        crate::runtime::puts(b"\r\n");
        crate::runtime::exit(0x400 + stage);
    }

    if let Err(stage) = smp_tlb_shootdown() {
        crate::runtime::puts(b"[libos] SMP TLB shootdown failed stage=");
        crate::runtime::hex(stage);
        crate::runtime::puts(b"\r\n");
        crate::runtime::exit(0x400 + stage);
    }

    if let Err(stage) = leave_resources_for_exit_cleanup() {
        crate::runtime::puts(b"[libos] Frame exit-cleanup setup failed stage=");
        crate::runtime::hex(stage);
        crate::runtime::puts(b"\r\n");
        crate::runtime::exit(0x400 + stage);
    }
}
