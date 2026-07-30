//! Notification 异步事件对象的用户态封装。
//!
//! Notification保存一个pending badge位图。signal和硬件IRQ都会把badge按位
//! OR进去；wait原子取得并清空pending，若为空则只阻塞当前线程。

pub struct Notification {
    /// 不透明Kernel handle，包含对象表slot和generation，不能当作地址使用。
    pub(crate) handle: exo_abi::NotificationHandle,
}

impl Notification {
    /// 包装已经由其他模块保存的handle，不创建或复制Kernel对象。
    pub fn from_raw(handle: u64) -> Self {
        Self {
            handle: exo_abi::NotificationHandle(handle),
        }
    }

    /// 返回裸handle，供跨线程AtomicU64或BootInfo式结构保存。
    pub fn raw(&self) -> u64 {
        self.handle.0
    }

    /// 在当前任务的Notification对象表中申请一个空槽。
    pub fn create() -> Result<Self, u64> {
        // 无参数SVC；成功返回slot+generation编码后的非零handle。
        let handle = crate::runtime::svc(exo_abi::SYS_NOTIFICATION_CREATE, 0, 0, 0);
        if exo_abi::is_sys_error(handle) {
            Err(handle)
        } else {
            Ok(Self {
                handle: exo_abi::NotificationHandle(handle),
            })
        }
    }

    /// 返回带类型的ABI handle，供IRQ绑定等接口使用。
    pub fn handle(&self) -> exo_abi::NotificationHandle {
        self.handle
    }

    /// 异步发送事件；不会等待接收方，多个badge会被Kernel合并。
    pub fn signal(&self, badge: u64) -> Result<(), u64> {
        match crate::runtime::svc(exo_abi::SYS_NOTIFICATION_SIGNAL, self.handle.0, badge, 0) {
            0 => Ok(()),
            error => Err(error),
        }
    }

    /// 将一个已经获授权的硬件 IRQ 连接到本 Notification。
    pub fn bind_irq(&self, intid: u32, badge: u64) -> Result<(), u64> {
        // Kernel同时验证IRQ grant、Notification owner及badge非零。
        crate::runtime::irq_bind_notification(intid, self.handle, badge)
    }

    /// 等待事件；无pending badge时Kernel把当前线程置为Blocked。
    pub fn wait(&self) -> Result<u64, u64> {
        let badge = crate::runtime::svc(exo_abi::SYS_NOTIFICATION_WAIT, self.handle.0, 0, 0);
        if exo_abi::is_sys_error(badge) {
            Err(badge)
        } else {
            Ok(badge)
        }
    }

    /// 非阻塞读取pending badge；没有事件时立即返回0。
    pub fn poll(&self) -> Result<u64, u64> {
        let badge = crate::runtime::svc(exo_abi::SYS_NOTIFICATION_POLL, self.handle.0, 0, 0);
        if exo_abi::is_sys_error(badge) {
            Err(badge)
        } else {
            Ok(badge)
        }
    }

    /// 销毁对象。仍绑定IRQ或仍有等待线程时Kernel会拒绝。
    pub fn destroy(&self) -> Result<(), u64> {
        match crate::runtime::svc(exo_abi::SYS_NOTIFICATION_DESTROY, self.handle.0, 0, 0) {
            0 => Ok(()),
            error => Err(error),
        }
    }
}
