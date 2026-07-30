//! Endpoint/Reply 同步 IPC 的用户态封装。
//!
//! 消息正文放在当前线程的固定 IPC Buffer。这里的 syscall 只传 Handle，
//! 因而线程在 Endpoint 上阻塞时，Kernel 不需要保存任意用户指针。

pub struct Endpoint {
    pub(crate) handle: exo_abi::EndpointHandle,
}

impl Endpoint {
    pub fn from_raw(handle: u64) -> Self {
        Self {
            handle: exo_abi::EndpointHandle(handle),
        }
    }

    pub fn raw(&self) -> u64 {
        self.handle.0
    }

    pub fn create() -> Result<Self, u64> {
        let handle = crate::runtime::svc(exo_abi::SYS_ENDPOINT_CREATE, 0, 0, 0);
        if exo_abi::is_sys_error(handle) {
            Err(handle)
        } else {
            Ok(Self {
                handle: exo_abi::EndpointHandle(handle),
            })
        }
    }

    pub fn handle(&self) -> exo_abi::EndpointHandle {
        self.handle
    }

    /// 销毁当前任务拥有的空闲Endpoint。
    ///
    /// Kernel会拒绝仍有Sender、Receiver或未完成Call/Reply的对象；成功后
    /// generation递增，因此其他位置保存的同一数值Handle不能再次使用。
    pub fn destroy(&self) -> Result<(), u64> {
        match crate::runtime::svc(exo_abi::SYS_ENDPOINT_DESTROY, self.handle.0, 0, 0) {
            0 => Ok(()),
            error => Err(error),
        }
    }

    pub fn send(&self) -> Result<(), u64> {
        match crate::runtime::svc(exo_abi::SYS_ENDPOINT_SEND, self.handle.0, 0, 0) {
            0 => Ok(()),
            error => Err(error),
        }
    }

    pub fn recv(&self) -> Result<(), u64> {
        let result = crate::runtime::svc(exo_abi::SYS_ENDPOINT_RECV, self.handle.0, 0, 0);
        match result {
            0 => Ok(()),
            error => Err(error),
        }
    }

    pub fn call(&self) -> Result<(), u64> {
        match crate::runtime::svc(exo_abi::SYS_ENDPOINT_CALL, self.handle.0, 0, 0) {
            0 => Ok(()),
            error => Err(error),
        }
    }

    pub fn reply(&self, reply: exo_abi::ReplyHandle) -> Result<(), u64> {
        match crate::runtime::svc(exo_abi::SYS_ENDPOINT_REPLY, reply.0, 0, 0) {
            0 => Ok(()),
            error => Err(error),
        }
    }

    pub fn reply_recv(&self, reply: exo_abi::ReplyHandle) -> Result<(), u64> {
        let result = crate::runtime::svc5(
            exo_abi::SYS_ENDPOINT_REPLY_RECV,
            self.handle.0,
            reply.0,
            0,
            0,
            0,
        );
        match result {
            0 => Ok(()),
            error => Err(error),
        }
    }
}

/// 每个线程独占一页 IPC Buffer。主线程使用固定首地址，新线程从
/// Kernel 注入的入口参数 x2 构造，避免共享全局“当前 Buffer”状态。
pub struct IpcBuffer {
    ptr: *mut exo_abi::IpcMessage,
}

impl IpcBuffer {
    pub fn initial() -> Self {
        Self {
            ptr: exo_abi::THREAD_IPC_BUFFER_BASE as *mut exo_abi::IpcMessage,
        }
    }

    pub unsafe fn from_va(va: u64) -> Self {
        Self {
            ptr: va as *mut exo_abi::IpcMessage,
        }
    }

    pub fn read(&self) -> exo_abi::IpcMessage {
        unsafe { core::ptr::read_volatile(self.ptr) }
    }

    pub fn write(&mut self, message: exo_abi::IpcMessage) {
        unsafe { core::ptr::write_volatile(self.ptr, message) }
    }
}
