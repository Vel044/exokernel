//! QEMU上的Frame/VSpace验收程序。

use crate::memory::{Frame, Rights};

const TEST_VA: u64 = exo_abi::FRAME_ARENA_BASE;
const SECOND_VA: u64 = TEST_VA + 0x20_000;

fn expect_error<T>(result: Result<T, u64>, wanted: u64, stage: u64) -> Result<(), u64> {
    match result {
        Err(error) if error == wanted => Ok(()),
        _ => Err(stage),
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
        exo_abi::SYS_ERR_NOT_FOUND,
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

#[cfg(not(feature = "frame-fault-test"))]
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

pub fn run() -> ! {
    if let Err(stage) = smoke() {
        crate::runtime::puts(b"[libos] Frame smoke failed stage=");
        crate::runtime::hex(stage);
        crate::runtime::puts(b"\r\n");
        crate::runtime::exit(0x400 + stage);
    }

    #[cfg(feature = "frame-fault-test")]
    {
        let frame = Frame::allocate(1, 1).expect("fault-test frame");
        let mapping = frame
            .map(0, 1, TEST_VA, Rights::READ_WRITE)
            .expect("fault-test mapping");
        let stale_ptr = mapping.as_ptr();
        unsafe { stale_ptr.write_volatile(0x33) };
        mapping.unmap().expect("fault-test unmap");
        crate::runtime::puts(b"[libos] expecting Data Abort at Frame VA\r\n");
        unsafe { core::ptr::read_volatile(stale_ptr) };
        crate::runtime::exit(0x4ff);
    }

    #[cfg(not(feature = "frame-fault-test"))]
    {
        if let Err(stage) = leave_resources_for_exit_cleanup() {
            crate::runtime::puts(b"[libos] Frame exit-cleanup setup failed stage=");
            crate::runtime::hex(stage);
            crate::runtime::puts(b"\r\n");
            crate::runtime::exit(0x400 + stage);
        }
        crate::runtime::exit(0)
    }
}
