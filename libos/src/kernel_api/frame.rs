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
    handle: exo_abi::FrameHandle,
    pages: u64,
}

impl Frame {
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
        let handle = crate::runtime::frame_map(self.handle, offset_pages, pages, va, rights.0)?;
        let ptr = NonNull::new(va as *mut u8).ok_or(exo_abi::SYS_ERR_INVALID)?;
        Ok(Mapping {
            handle,
            ptr,
            len: pages as usize * exo_abi::PAGE_SIZE as usize,
        })
    }

    pub fn pages(&self) -> u64 {
        self.pages
    }

    pub fn raw_handle(&self) -> exo_abi::FrameHandle {
        self.handle
    }

    pub fn free(self) -> Result<(), u64> {
        crate::runtime::frame_free(self.handle)
    }
}

pub struct Mapping {
    handle: exo_abi::MappingHandle,
    ptr: NonNull<u8>,
    len: usize,
}

impl Mapping {
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn raw_handle(&self) -> exo_abi::MappingHandle {
        self.handle
    }

    pub fn unmap(self) -> Result<(), u64> {
        crate::runtime::frame_unmap(self.handle)
    }
}
