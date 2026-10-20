//! UEFI 向 EL0 移交完整 DTB，只读取容器头的长度，不解析节点或设备属性。
//!
//! 在 EBS 前复制到页对齐的 LoaderData，尾页清零；这批页属于启动交接，
//! 不进入 Conventional RAM 分配池。EL1 将其映射为 EL0 只读、不可执行。

use super::boot_info::BootInfo;
use uefi::boot::{allocate_pages, AllocateType, MemoryType};

pub fn copy_dtb(info: &mut BootInfo, firmware_pa: u64) -> Result<(), &'static str> {
    let fallback = super::platform::FALLBACK_DTB;
    let bytes = if firmware_pa != 0 {
        // SAFETY: 地址来自可信固件 FDT_GUID；UEFI 在 EBS 前保证头部可读。
        // 只按字节读取，避免固件关闭 MMU 时出现未对齐访问。
        let mut header = [0u8; 8];
        for (index, byte) in header.iter_mut().enumerate() {
            *byte = unsafe { core::ptr::read_volatile((firmware_pa as *const u8).add(index)) };
        }
        if u32::from_be_bytes(header[..4].try_into().unwrap()) != 0xd00d_feed {
            return Err("invalid FDT magic");
        }
        let size = u32::from_be_bytes(header[4..].try_into().unwrap()) as usize;
        uefi::println!("[exo] firmware DTB bytes={}", size);
        if size < 40 || size > exo_abi::USER_DTB_MAX_SIZE as usize {
            return Err("invalid FDT length");
        }
        firmware_pa
            .checked_add(size as u64)
            .ok_or("FDT address overflow")?;
        // SAFETY: 有效固件 FDT 由 totalsize 描述；上面限定了交接最大长度。
        unsafe { core::slice::from_raw_parts(firmware_pa as *const u8, size) }
    } else {
        fallback.ok_or("firmware DTB required on this platform")?
    };
    if bytes.len() < 40 || bytes.len() > exo_abi::USER_DTB_MAX_SIZE as usize {
        return Err("DTB exceeds EL0 window");
    }
    let pages = bytes.len().div_ceil(exo_abi::PAGE_SIZE as usize);
    let target = allocate_pages(
        AllocateType::MaxAddress(0xffff_ffff),
        MemoryType::LOADER_DATA,
        pages,
    )
    .map_err(|_| "no LoaderData for DTB")?;
    // SAFETY: UEFI 刚分配了独占连续页，不与源 blob 重叠；清零包括尾页，
    // 防止只读映射暴露同页的其他固件数据。逐字节 volatile 兼容早期内存属性。
    for index in 0..pages * exo_abi::PAGE_SIZE as usize {
        let value = if index < bytes.len() {
            unsafe { core::ptr::read_volatile(bytes.as_ptr().add(index)) }
        } else {
            0
        };
        unsafe { core::ptr::write_volatile(target.as_ptr().add(index), value) };
    }
    info.dtb = target.as_ptr() as u64;
    info.dtb_size = bytes.len() as u64;
    Ok(())
}
