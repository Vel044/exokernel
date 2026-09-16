//! libOS普通Frame的安全薄封装。
//!
//! Handle只在建立/撤销映射和释放时使用。调用者平时通过Mapping提供的
//! 指针和长度直接访问EL0虚拟内存。

use core::ptr::NonNull;

#[derive(Clone, Copy)]
pub struct Rights(u64);

impl Rights {
    pub const READ: Self = Self(exo_abi::FRAME_RIGHT_READ);
    pub const READ_WRITE: Self = Self(exo_abi::FRAME_RIGHT_READ | exo_abi::FRAME_RIGHT_WRITE);
    pub const READ_EXECUTE: Self = Self(exo_abi::FRAME_RIGHT_READ | exo_abi::FRAME_RIGHT_EXECUTE);
}

pub struct Frame {
    // Kernel分配的普通Frame不透明句柄。
    handle: exo_abi::FrameHandle,
    // 该Frame覆盖的页数。
    pages: u64,
}

impl Frame {
    // 通过SVC向Kernel申请普通物理Frame。
    pub fn allocate(pages: u64, align_pages: u64) -> Result<Self, u64> {
        let handle = crate::runtime::frame_alloc(pages, align_pages)?;
        Ok(Self { handle, pages })
    }

    pub fn map(
        &self,
        offset_pages: u64,
        pages: u64,
        va: u64,
        rights: Rights,
    ) -> Result<Mapping, u64> {
        // 将Frame的一段映射到当前EL0地址空间。
        let handle = crate::runtime::frame_map(self.handle, offset_pages, pages, va, rights.0)?;
        let ptr = NonNull::new(va as *mut u8).ok_or(exo_abi::SYS_ERR_INVALID)?;
        Ok(Mapping {
            handle,
            ptr,
            len: pages as usize * exo_abi::PAGE_SIZE as usize,
        })
    }

    /// 把同一 Frame 的一段内容映射到另一个 VSpace。目标页表由 Kernel
    /// 维护，调用者只提供不透明 VSpaceHandle 和目标用户 VA。
    pub fn map_to(
        &self,
        target: &crate::vspace::VSpace,
        offset_pages: u64,
        pages: u64,
        va: u64,
        rights: Rights,
    ) -> Result<Mapping, u64> {
        let handle = crate::runtime::frame_map_to(
            self.handle,
            target.handle(),
            offset_pages,
            pages,
            va,
            rights.0,
        )?;
        let ptr = NonNull::new(va as *mut u8).ok_or(exo_abi::SYS_ERR_INVALID)?;
        Ok(Mapping {
            handle,
            ptr,
            len: pages as usize * exo_abi::PAGE_SIZE as usize,
        })
    }

    pub fn pages(&self) -> u64 {
        // 返回Frame持有的页数。
        self.pages
    }

    pub fn raw_handle(&self) -> exo_abi::FrameHandle {
        // 返回供内部系统调用使用的不透明句柄。
        self.handle
    }

    // 通过SVC释放当前Frame及其未释放的关联资源。
    pub fn free(self) -> Result<(), u64> {
        crate::runtime::frame_free(self.handle)
    }
}

pub struct Mapping {
    // Kernel创建的映射不透明句柄。
    handle: exo_abi::MappingHandle,
    // 映射起始处的EL0 CPU虚拟地址。
    ptr: NonNull<u8>,
    // 当前映射覆盖的字节数。
    len: usize,
}

impl Mapping {
    // 返回映射起始处的CPU VA。
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    // 返回映射覆盖的字节数。
    pub fn len(&self) -> usize {
        self.len
    }

    // 返回供内部系统调用使用的不透明映射句柄。
    pub fn raw_handle(&self) -> exo_abi::MappingHandle {
        self.handle
    }

    // 通过SVC撤销当前Frame映射。
    pub fn unmap(self) -> Result<(), u64> {
        crate::runtime::frame_unmap(self.handle)
    }
}
