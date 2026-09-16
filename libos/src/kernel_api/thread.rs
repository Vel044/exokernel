//! EL0静态优先级线程接口。
//!
//! 线程创建时固定CPU亲和性；基础优先级可在MCP授权范围内修改。高优先级
//! Ready线程由Kernel立即抢占，同优先级线程按1ms时间片轮转。

#[derive(Clone, Copy)]
pub struct ThreadConfig {
    pub cpu: u8,
    pub priority: u8,
    pub max_control_priority: u8,
}

impl ThreadConfig {
    pub const fn new(cpu: u8, priority: u8, max_control_priority: u8) -> Self {
        Self {
            cpu,
            priority,
            max_control_priority,
        }
    }
}

pub struct Thread {
    handle: exo_abi::ThreadHandle,
}

impl Thread {
    pub fn spawn(
        entry: extern "C" fn(u64, u64, u64) -> !,
        arg: u64,
        config: ThreadConfig,
    ) -> Result<Self, u64> {
        let value = crate::runtime::svc5(
            exo_abi::SYS_THREAD_CREATE,
            entry as usize as u64,
            arg,
            config.cpu as u64,
            config.priority as u64,
            config.max_control_priority as u64,
        );
        if exo_abi::is_sys_error(value) {
            Err(value)
        } else {
            Ok(Self {
                handle: exo_abi::ThreadHandle(value),
            })
        }
    }

    pub fn handle(&self) -> exo_abi::ThreadHandle {
        self.handle
    }

    /// 在目标 VSpace 中创建一个 Suspended 线程。Kernel只在本次 SVC期间
    /// 读取栈上的固定 ABI 配置，不保存这个用户态指针。
    pub fn spawn_in(
        target: &crate::vspace::VSpace,
        entry: u64,
        stack_pointer: u64,
        arg0: u64,
        arg1: u64,
        config: ThreadConfig,
    ) -> Result<Self, u64> {
        let create = exo_abi::ThreadCreateConfig {
            entry,
            stack_pointer,
            arg0,
            arg1,
            cpu: config.cpu,
            priority: config.priority,
            max_control_priority: config.max_control_priority,
            reserved: 0,
        };
        let value = crate::runtime::svc(
            exo_abi::SYS_THREAD_CREATE_IN,
            target.handle().0,
            (&create as *const exo_abi::ThreadCreateConfig) as u64,
            0,
        );
        if exo_abi::is_sys_error(value) {
            Err(value)
        } else {
            Ok(Self {
                handle: exo_abi::ThreadHandle(value),
            })
        }
    }

    /// 将 Suspended 线程发布到调度器。
    pub fn start(&self) -> Result<(), u64> {
        result(crate::runtime::svc(exo_abi::SYS_THREAD_START, self.handle.0, 0, 0))
    }

    pub fn suspend(&self) -> Result<(), u64> {
        result(crate::runtime::svc(exo_abi::SYS_THREAD_SUSPEND, self.handle.0, 0, 0))
    }

    pub fn destroy(self) -> Result<(), u64> {
        result(crate::runtime::svc(exo_abi::SYS_THREAD_DESTROY, self.handle.0, 0, 0))
    }

    pub fn set_priority(&self, priority: u8) -> Result<(), u64> {
        result(crate::runtime::svc(
            exo_abi::SYS_THREAD_SET_PRIORITY,
            self.handle.0,
            priority as u64,
            0,
        ))
    }

    pub fn runtime_ticks(&self) -> Result<u64, u64> {
        let value = crate::runtime::svc(exo_abi::SYS_THREAD_RUNTIME, self.handle.0, 0, 0);
        if exo_abi::is_sys_error(value) {
            Err(value)
        } else {
            Ok(value)
        }
    }
}

/// 当前同优先级队列仍有其他线程时，把本线程移到队尾。
pub fn yield_now() {
    let _ = crate::runtime::svc(exo_abi::SYS_THREAD_YIELD, 0, 0, 0);
}

/// Kernel写入只读`TPIDRRO_EL0`的逻辑CPU编号。
pub fn current_cpu() -> usize {
    let cpu: u64;
    unsafe {
        core::arch::asm!(
            "mrs {cpu}, tpidrro_el0",
            cpu = out(reg) cpu,
            options(nomem, nostack)
        );
    }
    cpu as usize
}

pub fn exit(code: u64) -> ! {
    crate::runtime::svc(exo_abi::SYS_THREAD_EXIT, code, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

fn result(value: u64) -> Result<(), u64> {
    if value == 0 {
        Ok(())
    } else {
        Err(value)
    }
}
