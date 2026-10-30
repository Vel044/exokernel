// SPDX-License-Identifier: GPL-2.0-or-later
//! Linux 形状的 IRQ 注册；EL0 持有回调及 dev_id，EL1 只持有 Notification 路由。
//! free_irq 移除回调、关闭路由、取消等待，并同步正在执行的处理后才销毁通知。
use super::{error::*, kernel, lock::Lock, memory};
use alloc::vec::Vec;
use core::{
    ffi::{c_char, c_void},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
pub type irqreturn_t = i32;
pub const IRQ_NONE: i32 = 0;
pub const IRQ_HANDLED: i32 = 1;
pub const IRQ_WAKE_THREAD: i32 = 2;
pub type irq_handler_t = unsafe fn(i32, *mut c_void) -> irqreturn_t;
struct Lifetime {
    // 已进入 dispatch 的使用者数量（包括等待通知者）；free_irq 等待清零后才能释放。
    users: AtomicUsize,
    // 注销标志；free_irq 用 Release 发布，dispatch 用 Acquire 观察并停止回调。
    cancelled: AtomicBool,
    // 已登记 net_device 的访问锁地址；0 表示没有设备锁，锁本身不由此结构拥有。
    context: usize,
}
struct Action {
    irq: u32,
    handler: irq_handler_t,
    flags: u64,
    name: usize,
    dev: usize,
    notification: u64,
    lifetime: usize,
    worker: u64,
}
static ACTIONS: Lock<Vec<Action>> = Lock::new(Vec::new());
/// 注册 EL0 中断回调，绑定 IRQ 通知并启动负责 dispatch 的服务线程。
/// 成功返回 0；失败返回负错误码。目前仅支持 flags == 0，且每个 IRQ 只能注册一次。
///
/// # 安全前提
/// handler 及其使用的 dev 上下文必须在注册期间保持有效，直到 free_irq 同步结束；
/// name 仅保存地址，不复制或解引用，其所有权不转移。平台操作必须已安装。
/// 同一 IRQ 的注册与注销须由调用方串行化，避免启动线程期间注册记录被移除。
pub unsafe fn request_irq(
    irq: u32,
    handler: irq_handler_t,
    flags: u64,
    name: *const c_char,
    dev: *mut c_void,
) -> i32 {
    // 不静默忽略触发方式、共享中断等标志；当前适配层尚不支持这些请求。
    if flags != 0 {
        return -EOPNOTSUPP;
    }
    let mut actions = ACTIONS.lock();
    // 检查与插入共用 ACTIONS 锁，防止并发注册占用同一个 IRQ。
    if actions.iter().any(|a| a.irq == irq) {
        return -EBUSY;
    }
    // 先为注册表预留容量，避免取得内核资源后 push 再触发扩容分配。
    if actions.try_reserve(1).is_err() {
        return -ENOMEM;
    }
    // 单独堆分配以保持地址稳定，不随 Vec 扩容移动；由 free_irq 在使用者退出后释放。
    // 此时尚无 dispatch 使用者，也未发出取消请求；设备锁用于串行化应用与回调访问。
    let lifetime = memory::value(Lifetime {
        users: AtomicUsize::new(0),
        cancelled: AtomicBool::new(false),
        context: super::netdevice::irq_gate(dev),
    });
    if lifetime.is_null() {
        return -ENOMEM;
    }
    // 委托平台绑定 IRQ 与 Notification，返回通知句柄；回调和 dev 仍留在 EL0。
    let notification = match (kernel::ops().bind)(irq) {
        Ok(h) => h,
        Err(e) => {
            // 尚未发布注册记录，仅需回收本地生命周期对象；错误码直接传回调用方。
            memory::free_value(lifetime);
            return e;
        }
    };
    // 先发布记录，确保新服务线程一旦运行即可通过 IRQ 找到回调与通知句柄。
    // name/dev 以地址保存，并不复制所指对象；lifetime 和 notification 交由注销路径回收。
    actions.push(Action {
        irq,
        handler,
        flags,
        name: name as usize,
        dev: dev as usize,
        notification,
        lifetime: lifetime as usize,
        worker: 0, // 启动前的占位值；当前平台 stop_worker(0) 无操作，供失败回滚使用。
    });
    // 启动线程前释放注册表锁，dispatch 自身也需要取得这把锁。
    drop(actions);
    let worker = match (kernel::ops().start_worker)(irq) {
        Ok(w) => w,
        Err(e) => {
            // 记录已发布且 IRQ 已绑定，复用注销流程解绑、同步使用者并销毁通知和 lifetime。
            free_irq(irq, dev);
            return e;
        }
    };
    let mut actions = ACTIONS.lock();
    // 回填服务线程句柄，供完成等待与注销时使用。这里依赖调用方不并发注销同一 IRQ；
    // 若该前提被破坏，expect 会 panic，并不提供竞态恢复。
    actions
        .iter_mut()
        .find(|a| a.irq == irq && a.dev == dev as usize)
        .expect("注册记录丢失")
        .worker = worker;
    0
}
pub unsafe fn free_irq(irq: u32, dev_id: *mut c_void) -> *const c_void {
    let action = {
        let mut actions = ACTIONS.lock();
        let Some(i) = actions
            .iter()
            .position(|a| a.irq == irq && a.dev == dev_id as usize)
        else {
            return core::ptr::null();
        };
        actions.remove(i)
    };
    let lifetime = &*(action.lifetime as *const Lifetime);
    lifetime.cancelled.store(true, Ordering::Release);
    if (kernel::ops().unbind)(irq).is_err() {
        lifetime.cancelled.store(false, Ordering::Release);
        ACTIONS.lock().push(action);
        panic!("IRQ解绑失败，保留注册记录");
    }
    if lifetime.users.load(Ordering::Acquire) != 0 {
        if (kernel::ops().cancel_wait)(action.notification).is_err() {
            panic!("取消IRQ等待失败");
        }
        while lifetime.users.load(Ordering::Acquire) != 0 {
            (kernel::ops().yield_now)();
        }
    }
    (kernel::ops().stop_worker)(action.worker);
    if (kernel::ops().destroy)(action.notification).is_err() {
        panic!("Notification销毁失败");
    }
    memory::free_value(action.lifetime as *mut Lifetime);
    action.name as *const c_void
}
struct InFlight(*const Lifetime);
impl Drop for InFlight {
    fn drop(&mut self) {
        unsafe {
            (*self.0).users.fetch_sub(1, Ordering::Release);
        }
    }
}
pub unsafe fn dispatch(irq: u32) -> Result<(), i32> {
    let (handler, dev, handle, life) = {
        let actions = ACTIONS.lock();
        let a = actions.iter().find(|a| a.irq == irq).ok_or(-ENXIO)?;
        (*(a.lifetime as *const Lifetime))
            .users
            .fetch_add(1, Ordering::Acquire);
        (
            a.handler,
            a.dev as *mut c_void,
            a.notification,
            a.lifetime as *const Lifetime,
        )
    };
    let guard = InFlight(life);
    let badge = (kernel::ops().wait)(handle)?;
    if (*guard.0).cancelled.load(Ordering::Acquire) || badge & 1 == 0 {
        return Ok(());
    }
    // SAFETY: free_irq 等待 users 清零后才能让框架释放设备；应用和回调共用同一把设备锁。
    let _device_guard = if (*guard.0).context != 0 {
        let gate = &*((*guard.0).context as *const Lock<()>);
        loop {
            if (*guard.0).cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            if let Some(g) = gate.try_lock() {
                break Some(g);
            }
            (kernel::ops().yield_now)();
        }
    } else {
        None
    };
    if (*guard.0).cancelled.load(Ordering::Acquire) {
        return Ok(());
    }
    let _ = handler(irq as i32, dev);
    if !(*guard.0).cancelled.load(Ordering::Acquire) {
        if let Err(e) = (kernel::ops().ack)(irq) {
            if !(*guard.0).cancelled.load(Ordering::Acquire) {
                return Err(e);
            }
        }
    }
    // Linux 的 NET_TX 后续处理在原 handler 返回后推进；目标侧同样由 IRQ 服务调度积压发送。
    if (*guard.0).context != 0 && !(*guard.0).cancelled.load(Ordering::Acquire) {
        super::netdevice::run_scheduled_tx(dev.cast());
    }
    Ok(())
}
pub fn bindings() -> usize {
    ACTIONS.lock().len()
}

/// 应用只等待处理完成；硬件回调由 request_irq 创建的服务线程自动派发。
pub fn wait_completion(irq: u32) -> Result<(), i32> {
    let worker = {
        let actions = ACTIONS.lock();
        actions.iter().find(|a| a.irq == irq).ok_or(-ENXIO)?.worker
    };
    (kernel::ops().wait_worker)(worker)
}
