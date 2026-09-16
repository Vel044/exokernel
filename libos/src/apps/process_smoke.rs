//! 独立 VSpace 的最小资源实验。
//!
//! 这个 smoke 不运行外部 ELF，而是先验证 ProcessBuilder 所依赖的 Kernel
//! 原语：两个独立 VSpace 可以把不同 Frame 映射到同一个用户 VA，随后各自
//! 解除映射并回收。真正的 ELF 入口启动由 `ProcessBuilder` API 负责。

pub fn run() -> ! {
    crate::runtime::puts(b"[libos] ===== process VSpace smoke start =====\r\n");

    let first = crate::vspace::VSpace::create().unwrap_or_else(|_| {
        crate::runtime::puts(b"[libos] VSPACE_CREATE A failed\r\n");
        crate::runtime::exit(0x410)
    });
    let second = crate::vspace::VSpace::create().unwrap_or_else(|_| {
        crate::runtime::puts(b"[libos] VSPACE_CREATE B failed\r\n");
        crate::runtime::exit(0x411)
    });
    let frame_a = crate::memory::Frame::allocate(1, 1).unwrap_or_else(|_| {
        crate::runtime::puts(b"[libos] Frame A allocate failed\r\n");
        crate::runtime::exit(0x412)
    });
    let frame_b = crate::memory::Frame::allocate(1, 1).unwrap_or_else(|_| {
        crate::runtime::puts(b"[libos] Frame B allocate failed\r\n");
        crate::runtime::exit(0x413)
    });

    // 同一个 VA 在不同页表中可以分别指向不同物理 Frame；映射本身由
    // FRAME_MAP_TO 的目标 VSpaceHandle 区分，不能互相覆盖。
    let va = exo_abi::USER_BASE;
    let mapping_a = frame_a
        .map_to(&first, 0, 1, va, crate::memory::Rights::READ_WRITE)
        .unwrap_or_else(|_| {
            crate::runtime::puts(b"[libos] Frame A map-to failed\r\n");
            crate::runtime::exit(0x414)
        });
    let mapping_b = frame_b
        .map_to(&second, 0, 1, va, crate::memory::Rights::READ_WRITE)
        .unwrap_or_else(|_| {
            crate::runtime::puts(b"[libos] Frame B map-to failed\r\n");
            crate::runtime::exit(0x415)
        });

    if mapping_a.unmap().is_err()
        || mapping_b.unmap().is_err()
        || frame_a.free().is_err()
        || frame_b.free().is_err()
        || first.destroy().is_err()
        || second.destroy().is_err()
    {
        crate::runtime::puts(b"[libos] process VSpace smoke cleanup failed\r\n");
        crate::runtime::exit(0x416)
    }
    crate::runtime::puts(b"[libos] process VSpace smoke passed\r\n");
    crate::runtime::exit(0)
}
