// SPDX-License-Identifier: GPL-2.0-or-later
//! IRQ 同名注册/注销。回调在所属 EL0 线程执行，不在 EL1 硬中断上下文执行。
//! 借助独占可变借用串行派发，回调上下文由调用者拥有，避免保存自引用裸指针。
use super::error::{EBUSY, EINVAL, EOPNOTSUPP};
pub trait IrqBackend {
    fn bind(&mut self, irq: u32) -> Result<(), i32>;
    fn wait(&mut self) -> Result<bool, i32>;
    fn ack(&mut self, irq: u32) -> Result<(), i32>;
    fn unbind(&mut self, irq: u32) -> Result<(), i32>;
}
pub struct IrqRegistration<C, B: IrqBackend> {
    /// 后端拥有 Notification 等内核资源；不包含驱动寄存器知识。
    backend: B,
    /// 限定 IRQ 所属线程，禁止安全代码把路由和回调派发器跨线程移动或共享。
    _owner: core::marker::PhantomData<*mut ()>,
    /// 注册后保存 IRQ、函数指针和诊断名；None 表示已解绑，不能继续派发。
    binding: Option<(u32, fn(&mut C), &'static str)>,
}
impl<C, B: IrqBackend> IrqRegistration<C, B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            binding: None,
            _owner: core::marker::PhantomData,
        }
    }
    pub fn is_bound(&self) -> bool {
        self.binding.is_some()
    }
    pub fn wait_and_dispatch(&mut self, context: &mut C) -> Result<(), i32> {
        let (irq, handler, _) = self.binding.ok_or(EINVAL)?;
        if self.backend.wait()? {
            handler(context);
            // 设备回调必须先消费 SRAM 并 W1C 清 pending，随后才能重新开放 GIC IRQ。
            self.backend.ack(irq)?;
        }
        Ok(())
    }
}
/// flags=0 表示独占、单线程处理；IRQF_SHARED 等暂不支持，必须显式失败。
pub fn request_irq<C, B: IrqBackend>(
    registration: &mut IrqRegistration<C, B>,
    irq: u32,
    handler: fn(&mut C),
    flags: u64,
    name: &'static str,
) -> Result<(), i32> {
    if flags != 0 {
        return Err(EOPNOTSUPP);
    }
    if registration.binding.is_some() {
        return Err(EBUSY);
    }
    registration.backend.bind(irq)?;
    registration.binding = Some((irq, handler, name));
    Ok(())
}
/// 独占 &mut 保证此时无回调并发执行；解绑失败保留 binding，禁止假装资源已回收。
pub fn free_irq<C, B: IrqBackend>(registration: &mut IrqRegistration<C, B>) -> Result<(), i32> {
    let (irq, _, _) = registration.binding.ok_or(EINVAL)?;
    registration.backend.unbind(irq)?;
    registration.binding = None;
    Ok(())
}
impl<C, B: IrqBackend> Drop for IrqRegistration<C, B> {
    fn drop(&mut self) {
        if self.is_bound() {
            // 失败立即终止当前执行；不能继续释放仍被 IRQ 使用的资源。
            free_irq(self).expect("IRQ 解绑失败，禁止继续回收");
        }
    }
}
