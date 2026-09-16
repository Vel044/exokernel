//! libOS 对独立 VSpace 的安全薄封装。
//!
//! VSpaceHandle 不是页表地址，也不能直接被用户解引用；它只用于把 Frame
//! 和 Suspended Thread 交给 Kernel 管理。ProcessBuilder 通过该对象组合进程。

pub struct VSpace {
    handle: exo_abi::VSpaceHandle,
}

impl VSpace {
    pub fn create() -> Result<Self, u64> {
        Ok(Self {
            handle: crate::runtime::vspace_create()?,
        })
    }

    pub fn handle(&self) -> exo_abi::VSpaceHandle {
        self.handle
    }

    pub fn destroy(self) -> Result<(), u64> {
        crate::runtime::vspace_destroy(self.handle)
    }
}
