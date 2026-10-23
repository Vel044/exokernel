// SPDX-License-Identifier: GPL-2.0-or-later
//! 同名接口的 Exokernel 后端：只负责 MMIO/IRQ/Notification 与诊断，绝不解析设备协议。
use super::{
    interrupt::IrqBackend,
    io::IoRegion,
    platform::{Mapper, NamedResource},
    printk::LogSink,
};
use crate::{notification::Notification, runtime};
use core::fmt::{self, Write};
fn fatal() -> ! {
    runtime::puts(b"[linux_api] resource cleanup failed\r\n");
    runtime::exit(0x703)
}
/// 不携带任何网卡寄存器地址；VA 由每个驱动的平台描述选择，EL1 仍审核授权。
pub struct ExoMapper;
pub struct Mapping {
    /// 拥有一次 Device-nGnRE 映射的 CPU VA 和精确长度，Drop 撤销，不拥有设备 SRAM。
    va: u64,
    size: u64,
    /// 当前实现只允许拥有线程撤销映射，避免跨线程映射/撤销竞争。
    _owner: core::marker::PhantomData<*mut ()>,
}
impl Mapper for ExoMapper {
    type Mapping = Mapping;
    fn map(&self, resource: NamedResource) -> Result<Mapping, i32> {
        if resource.base & 4095 != 0
            || resource.user_va & 4095 != 0
            || resource.size == 0
            || resource.size & 4095 != 0
            || resource.user_va.checked_add(resource.size).is_none()
        {
            return Err(-22);
        }
        // EL0 传 PA/size/VA，SVC 后 EL1 检查 grant、页对齐及冲突，成功建立 Device PTE。
        runtime::map_mmio(resource.base, resource.size, resource.user_va).map_err(|_| -5)?;
        Ok(Mapping {
            va: resource.user_va,
            size: resource.size,
            _owner: core::marker::PhantomData,
        })
    }
}
impl Mapping {
    pub fn region(&self) -> IoRegion<'_> {
        // SAFETY: Mapping 成功后拥有整个已授权 Device VA，视图借用阻止提前 Drop 撤销。
        unsafe { IoRegion::new(self.va as *mut u8, self.size as usize) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        // EL0 传 VA/size；EL1 核对当前任务映射记录，成功移除 PTE 和记录。
        if runtime::unmap_mmio(self.va, self.size).is_err() {
            fatal();
        }
    }
}
pub struct ExoIrq {
    /// 拥有一个通知 handle，IRQ 注册层必须先解绑，再允许本字段销毁。
    notification: Notification,
}
impl ExoIrq {
    pub fn new() -> Result<Self, i32> {
        Ok(Self {
            notification: Notification::create().map_err(|_| -5)?,
        })
    }
}
impl IrqBackend for ExoIrq {
    fn bind(&mut self, irq: u32) -> Result<(), i32> {
        // SYS_IRQ_BIND：EL0 传 INTID/handle/badge=1/当前 CPU；EL1 校验 grant/owner 后连接 GIC。
        self.notification.bind_irq(irq, 1).map_err(|_| -5)
    }
    fn wait(&mut self) -> Result<bool, i32> {
        // SYS_NOTIFICATION_WAIT：原子取走 badge；无事件则只阻塞当前 EL0 线程。
        Ok(self.notification.wait().map_err(|_| -5)? & 1 != 0)
    }
    fn ack(&mut self, irq: u32) -> Result<(), i32> {
        // GIC 在 EL1 已 EOI 并屏蔽；回调清设备 pending 后 SYS_IRQ_ACK 重新开放该 INTID。
        match runtime::svc(exo_abi::SYS_IRQ_ACK, irq as u64, 0, 0) {
            0 => Ok(()),
            _ => Err(-5),
        }
    }
    fn unbind(&mut self, irq: u32) -> Result<(), i32> {
        runtime::irq_unbind(irq).map_err(|_| -5)
    }
}
impl Drop for ExoIrq {
    fn drop(&mut self) {
        if self.notification.destroy().is_err() {
            fatal();
        }
    }
}
pub struct RuntimeLog;
impl Write for RuntimeLog {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        runtime::puts(value.as_bytes());
        Ok(())
    }
}
impl LogSink for RuntimeLog {
    fn write(&mut self, level: &str, device: &str, message: fmt::Arguments<'_>) {
        let _ = writeln!(self, "[{level}] {device}: {message}");
    }
}
