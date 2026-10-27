// SPDX-License-Identifier: GPL-2.0-or-later
//! Linux 同名接口的 Exokernel 接入；设备状态机仍在 EL0，SVC 只跨保护边界。
use super::{
    io::barrier,
    kernel::{self, KernelOps},
};
use crate::{notification::Notification, runtime};
use core::{
    ffi::c_void,
    sync::atomic::{AtomicUsize, Ordering},
};
static CPUS: AtomicUsize = AtomicUsize::new(1);
/// 从只读 DTB 取得 QEMU/固件提供的随机种子，不用时钟伪造随机源。
pub fn initialize(dtb: &[u8], cpu_count: u32) {
    CPUS.store(cpu_count as usize, Ordering::Release);
    if let Ok(tree) = fdt::Fdt::new(dtb) {
        if let Some(seed) = tree
            .find_node("/chosen")
            .and_then(|n| n.property("rng-seed"))
        {
            if seed.value.len() >= 32 {
                super::random::seed(seed.value);
            }
        }
    }
    // SAFETY: OPS 是静态对象，在设备初始化前安装，此后不替换。
    unsafe {
        kernel::install(&OPS);
    }
}
unsafe fn map(pa: u64, size: u64, va: u64) -> Result<*mut c_void, i32> {
    runtime::map_mmio(pa, size, va).map_err(|_| -5)?;
    Ok(va as *mut c_void)
}
unsafe fn unmap(va: *mut c_void, size: u64) -> Result<(), i32> {
    runtime::unmap_mmio(va as u64, size).map_err(|_| -5)
}
fn bind(irq: u32) -> Result<u64, i32> {
    let n = Notification::create().map_err(|_| -5)?;
    if n.bind_irq(irq, 1).is_err() {
        let _ = n.destroy();
        return Err(-5);
    }
    Ok(n.raw())
}
fn wait(handle: u64) -> Result<u64, i32> {
    Notification::from_raw(handle).wait().map_err(|_| -5)
}
fn ack(irq: u32) -> Result<(), i32> {
    if runtime::svc(exo_abi::SYS_IRQ_ACK, irq as u64, 0, 0) == 0 {
        Ok(())
    } else {
        Err(-5)
    }
}
fn unbind(irq: u32) -> Result<(), i32> {
    runtime::irq_unbind(irq).map_err(|_| -5)
}
fn destroy(handle: u64) -> Result<(), i32> {
    Notification::from_raw(handle).destroy().map_err(|_| -5)
}
// SAFETY: 平台框架只把已授权、仍存活的 Device VA 交给这些入口；原驱动保证事务宽度和范围。
unsafe fn read32(reg: *const c_void) -> u32 {
    let value = core::ptr::read_volatile(reg.cast::<u32>());
    barrier();
    u32::from_le(value)
}
unsafe fn write32(reg: *mut c_void, val: u32) {
    barrier();
    core::ptr::write_volatile(reg.cast::<u32>(), val.to_le());
    barrier();
}
unsafe fn fromio(to: *mut u8, from: *const c_void, count: usize) {
    for i in 0..count {
        *to.add(i) = core::ptr::read_volatile(from.cast::<u8>().add(i));
    }
    barrier();
}
unsafe fn toio(to: *mut c_void, from: *const u8, count: usize) {
    for i in 0..count {
        core::ptr::write_volatile(to.cast::<u8>().add(i), *from.add(i));
    }
    barrier();
}

struct Worker {
    irq: u32,
    completed: Notification,
    exited: core::sync::atomic::AtomicBool,
    thread: core::sync::atomic::AtomicU64,
}
extern "C" fn irq_worker(arg: u64, _boot: u64, _reserved: u64) -> ! {
    // SAFETY: request_irq 的 Worker 在 free_irq 等待线程退出之前始终存活。
    let worker = unsafe { &*(arg as *const Worker) };
    loop {
        match unsafe { super::interrupt::dispatch(worker.irq) } {
            Ok(()) => {
                if worker.completed.signal(1).is_err() {
                    panic!("IRQ完成通知失败");
                }
            }
            Err(e) if e == -super::error::ENXIO => break,
            Err(_) => panic!("IRQ服务失败"),
        }
    }
    worker.exited.store(true, Ordering::Release);
    crate::thread::exit(0)
}
fn start_worker(irq: u32) -> Result<u64, i32> {
    let completed = Notification::create().map_err(|_| -12)?;
    let completed_handle = completed.raw();
    let worker = unsafe {
        super::memory::value(Worker {
            irq,
            completed,
            exited: core::sync::atomic::AtomicBool::new(false),
            thread: core::sync::atomic::AtomicU64::new(0),
        })
    };
    if worker.is_null() {
        // 分配失败时销毁已经取得的内核对象。
        let _ = Notification::from_raw(completed_handle).destroy();
        return Err(-12);
    }
    let thread = match crate::thread::Thread::spawn(
        irq_worker,
        worker as u64,
        crate::thread::ThreadConfig::new(crate::thread::current_cpu() as u8, 32, 32),
    ) {
        Ok(t) => t,
        Err(_) => {
            unsafe {
                let _ = (*worker).completed.destroy();
                super::memory::free_value(worker);
            }
            return Err(-12);
        }
    };
    unsafe {
        (*worker).thread.store(thread.handle().0, Ordering::Release);
    }
    Ok(worker as u64)
}
fn wait_worker(handle: u64) -> Result<(), i32> {
    // SAFETY: 只有仍注册的 IRQ 才会把有效 Worker 地址交给 wait_completion。
    let worker = unsafe { &*(handle as *const Worker) };
    worker.completed.wait().map(|_| ()).map_err(|_| -5)
}
fn stop_worker(handle: u64) {
    if handle == 0 {
        return;
    }
    let worker = unsafe { &*(handle as *const Worker) };
    while !worker.exited.load(Ordering::Acquire) {
        crate::thread::yield_now();
    }
    // THREAD_EXIT 自行回收线程并递增句柄 generation；旧 handle 失效后才可释放 Worker。
    loop {
        let ret = runtime::svc(
            exo_abi::SYS_THREAD_RUNTIME,
            worker.thread.load(Ordering::Acquire),
            0,
            0,
        );
        if ret == exo_abi::SYS_ERR_NOT_FOUND {
            break;
        }
        if exo_abi::is_sys_error(ret) {
            panic!("IRQ线程退出同步失败");
        }
        crate::thread::yield_now();
    }
    if worker.completed.destroy().is_err() {
        panic!("IRQ完成通知销毁失败");
    }
    unsafe {
        super::memory::free_value(handle as *mut Worker);
    }
}
static OPS: KernelOps = KernelOps {
    map,
    unmap,
    bind,
    wait,
    ack,
    unbind,
    destroy,
    cancel_wait: |h| Notification::from_raw(h).signal(2).map_err(|_| -5),
    yield_now: crate::thread::yield_now,
    start_worker,
    wait_worker,
    stop_worker,
    read32,
    write32,
    fromio,
    toio,
    log: |s| runtime::puts(s.as_bytes()),
    ticks: runtime::counter,
    frequency: runtime::counter_frequency,
    cpu: crate::thread::current_cpu,
    cpus: || CPUS.load(Ordering::Acquire),
};
