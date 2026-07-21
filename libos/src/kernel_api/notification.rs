//! Notification 异步事件对象的用户态封装。

pub struct Notification {
    pub(crate) handle: exo_abi::NotificationHandle,
}

impl Notification {
    pub fn from_raw(handle: u64) -> Self {
        Self {
            handle: exo_abi::NotificationHandle(handle),
        }
    }

    pub fn raw(&self) -> u64 {
        self.handle.0
    }

    pub fn create() -> Result<Self, u64> {
        let handle = crate::runtime::svc(exo_abi::SYS_NOTIFICATION_CREATE, 0, 0, 0);
        if exo_abi::is_sys_error(handle) {
            Err(handle)
        } else {
            Ok(Self {
                handle: exo_abi::NotificationHandle(handle),
            })
        }
    }

    pub fn handle(&self) -> exo_abi::NotificationHandle {
        self.handle
    }

    pub fn signal(&self, badge: u64) -> Result<(), u64> {
        match crate::runtime::svc(exo_abi::SYS_NOTIFICATION_SIGNAL, self.handle.0, badge, 0) {
            0 => Ok(()),
            error => Err(error),
        }
    }

    /// 将一个已经获授权的硬件 IRQ 连接到本 Notification。
    pub fn bind_irq(&self, intid: u32, badge: u64) -> Result<(), u64> {
        crate::runtime::irq_bind_notification(intid, self.handle, badge)
    }

    pub fn wait(&self) -> Result<u64, u64> {
        let badge = crate::runtime::svc(exo_abi::SYS_NOTIFICATION_WAIT, self.handle.0, 0, 0);
        if exo_abi::is_sys_error(badge) {
            Err(badge)
        } else {
            Ok(badge)
        }
    }

    pub fn poll(&self) -> Result<u64, u64> {
        let badge = crate::runtime::svc(exo_abi::SYS_NOTIFICATION_POLL, self.handle.0, 0, 0);
        if exo_abi::is_sys_error(badge) {
            Err(badge)
        } else {
            Ok(badge)
        }
    }

    pub fn destroy(&self) -> Result<(), u64> {
        match crate::runtime::svc(exo_abi::SYS_NOTIFICATION_DESTROY, self.handle.0, 0, 0) {
            0 => Ok(()),
            error => Err(error),
        }
    }
}
