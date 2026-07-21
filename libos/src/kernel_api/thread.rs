//! EL0 线程接口。
//!
//! 线程对象和上下文保存在 EL1；libOS 只提交入口、参数和优先级，之后
//! 通过共享地址空间继续运行。线程入口的三个参数由 Kernel 注入：
//! x0=业务参数、x1=ThreadHandle、x2=该线程的 IPC Buffer VA。

pub struct Thread {
    pub(crate) handle: exo_abi::ThreadHandle,
}

impl Thread {
    pub fn spawn(
        entry: extern "C" fn(u64, u64, u64) -> !,
        arg: u64,
        priority: u8,
    ) -> Result<Self, u64> {
        let handle = crate::runtime::svc(
            exo_abi::SYS_THREAD_CREATE,
            entry as usize as u64,
            arg,
            priority as u64,
        );
        if exo_abi::is_sys_error(handle) {
            Err(handle)
        } else {
            Ok(Self {
                handle: exo_abi::ThreadHandle(handle),
            })
        }
    }

    pub fn handle(&self) -> exo_abi::ThreadHandle {
        self.handle
    }

    pub fn set_priority(&self, priority: u8) -> Result<(), u64> {
        match crate::runtime::svc(
            exo_abi::SYS_THREAD_SET_PRIORITY,
            self.handle.0,
            priority as u64,
            0,
        ) {
            0 => Ok(()),
            error => Err(error),
        }
    }
}

pub fn yield_now() -> Result<(), u64> {
    match crate::runtime::svc(exo_abi::SYS_THREAD_YIELD, 0, 0, 0) {
        0 => Ok(()),
        error => Err(error),
    }
}

pub fn spawn(
    entry: extern "C" fn(u64, u64, u64) -> !,
    arg: u64,
    priority: u8,
) -> Result<exo_abi::ThreadHandle, u64> {
    Thread::spawn(entry, arg, priority).map(|thread| thread.handle())
}

pub fn set_priority(handle: exo_abi::ThreadHandle, priority: u8) -> Result<(), u64> {
    let result = crate::runtime::svc(
        exo_abi::SYS_THREAD_SET_PRIORITY,
        handle.0,
        priority as u64,
        0,
    );
    match result {
        0 => Ok(()),
        error => Err(error),
    }
}

pub fn exit(code: u64) -> ! {
    crate::runtime::svc(exo_abi::SYS_THREAD_EXIT, code, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}
