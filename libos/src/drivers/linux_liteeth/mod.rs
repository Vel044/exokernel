//! 原 Linux LiteEth 的 EL0 适配边界：C 驱动不进入 EL1，也没有网络 syscall。
//! 单个所属线程串行执行 probe/open/TX/IRQ/stop/remove；不支持跨线程共享。
//! 设备 SRAM 采用 Device MMIO，CPU 用 volatile 复制，不使用 DMA_ALLOC。
use super::platform::LiteEthInfo;
use crate::{notification::Notification, runtime};
use alloc::alloc::{alloc_zeroed, dealloc};
use core::{
    alloc::Layout,
    ffi::c_void,
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};
const MAC_VA: u64 = 0x4050_0000;
const BUFFER_VA: u64 = 0x4060_0000;
/// C ABI 仅传递 DTB 描述的 CPU PA、长度、GIC INTID 和 slot 布局。
#[repr(C)]
struct Config {
    /// CSR 起始 CPU PA，来自 EL0 解析 DTB。
    mac_pa: u64,
    /// CSR 字节范围，映射仍由 EL1 MMIO grant 审核。
    mac_size: u64,
    /// 设备 SRAM 起始 CPU PA，不是 DMA 主存地址。
    buffer_pa: u64,
    /// SRAM 总字节数，容纳紧邻的 RX 与 TX slots。
    buffer_size: u64,
    /// 已经由 GIC SPI 编号换算的 INTID。
    irq: u32,
    /// 前半段接收 slots 的数量。
    rx_slots: u32,
    /// 后半段发送 slots 的数量。
    tx_slots: u32,
    /// 每个 SRAM slot 的字节容量。
    slot_size: u32,
    /// DTB MAC；无效地址交由原驱动选择本地地址。
    mac: [u8; 6],
}
/// 线性 L2 报文统计；顺序与窄兼容层一致，不是 Linux 二进制 ABI。
#[repr(C)]
#[derive(Default)]
pub struct Stats {
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
}
unsafe extern "C" {
    fn exo_liteeth_attach(c: *const Config) -> i32;
    fn exo_liteeth_open() -> i32;
    fn exo_liteeth_send(p: *const u8, n: usize) -> i32;
    fn exo_liteeth_handle_irq() -> i32;
    fn exo_liteeth_stats(s: *mut Stats) -> i32;
    fn exo_liteeth_close() -> i32;
    fn exo_liteeth_detach() -> i32;
}
// 只有本场景的一个所属线程能进入 C；IRQ 由 wait 返回后主动调用，不会异步重入。
// RX 回调复制到独立内存，避免在原 IRQ 回调中重入 ndo_start_xmit。
struct Bridge {
    /// 当前所属任务持有的内核通知句柄。
    notification: u64,
    intid: u32,
    /// 上层拥有的收包副本，C skb 回收后仍有效。
    frame: [u8; 2048],
    /// 副本有效字节数，零表示接收槽空闲。
    length: usize,
}
// Acquire/Release 只用于独占实例的取得/归还，不支持并发调用同一 C 实例。
static CLAIMED: AtomicBool = AtomicBool::new(false);
static mut BRIDGE: Bridge = Bridge {
    notification: 0,
    intid: 0,
    frame: [0; 2048],
    length: 0,
};
/// 独占 C 全局驱动实例；裸指针标记禁止把实例 Send/Sync 给其他线程。
pub struct Driver {
    notification: Notification,
    intid: u32,
    _owner: PhantomData<*mut ()>,
}
fn checked(result: i32) -> Result<(), i32> {
    if result == 0 {
        Ok(())
    } else {
        Err(result)
    }
}
impl Driver {
    pub fn liteeth_probe(info: LiteEthInfo) -> Result<Self, i32> {
        if CLAIMED
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(-16);
        }
        let notification = match Notification::create() {
            Ok(n) => n,
            Err(_) => {
                CLAIMED.store(false, Ordering::Release);
                return Err(-5);
            }
        };
        unsafe {
            (*core::ptr::addr_of_mut!(BRIDGE)).notification = notification.raw();
            (*core::ptr::addr_of_mut!(BRIDGE)).intid = info.mac.intid;
        }
        let config = Config {
            mac_pa: info.mac.base,
            mac_size: info.mac.size,
            buffer_pa: info.buffer.base,
            buffer_size: info.buffer.size,
            irq: info.mac.intid,
            rx_slots: info.rx_slots,
            tx_slots: info.tx_slots,
            slot_size: info.slot_size,
            mac: info.address,
        };
        // SAFETY: Config 布局与 C 一致，C 在调用期间读取，不保存该栈指针。
        if let Err(e) = checked(unsafe { exo_liteeth_attach(&config) }) {
            let _ = notification.destroy();
            unsafe {
                (*core::ptr::addr_of_mut!(BRIDGE)).notification = 0;
            }
            CLAIMED.store(false, Ordering::Release);
            return Err(e);
        }
        Ok(Self {
            notification,
            intid: info.mac.intid,
            _owner: PhantomData,
        })
    }
    pub fn liteeth_open(&mut self) -> Result<(), i32> {
        unsafe { checked(exo_liteeth_open()) }
    }
    pub fn liteeth_start_xmit(&mut self, frame: &[u8]) -> Result<bool, i32> {
        // C 同步复制报文，不保存 Rust 借用；BUSY 时保留调用者所有权以便重试。
        match unsafe { exo_liteeth_send(frame.as_ptr(), frame.len()) } {
            0 => Ok(true),
            16 => Ok(false),
            e => Err(e),
        }
    }
    pub fn wait_irq(&mut self) -> Result<(), i32> {
        let badge = self.notification.wait().map_err(|_| -5)?;
        if badge & 1 != 0 {
            // EL1 已 EOI 并屏蔽 SPI；先让原驱动 W1C 清设备事件，最后 ACK 重开 GIC。
            if unsafe { exo_liteeth_handle_irq() } != 1 {
                return Err(-5);
            }
            checked(runtime::svc(exo_abi::SYS_IRQ_ACK, self.intid as u64, 0, 0) as i32)?;
        }
        Ok(())
    }
    pub fn receive(&mut self, out: &mut [u8]) -> usize {
        // 单线程独占；回调已结束才取出帧。过小上层缓冲明确失败，不截断帧。
        unsafe {
            let p = core::ptr::addr_of_mut!(BRIDGE);
            let n = (*p).length;
            if n > out.len() {
                exo_lx_fault();
            }
            core::ptr::copy_nonoverlapping(
                core::ptr::addr_of!((*p).frame).cast::<u8>(),
                out.as_mut_ptr(),
                n,
            );
            (*p).length = 0;
            n
        }
    }
    pub fn liteeth_get_stats64(&mut self) -> Result<Stats, i32> {
        let mut s = Stats::default();
        unsafe {
            checked(exo_liteeth_stats(&mut s))?;
        }
        Ok(s)
    }
    pub fn liteeth_stop(&mut self) -> Result<(), i32> {
        unsafe { checked(exo_liteeth_close()) }
    }
    pub fn liteeth_remove(self) -> Result<(), i32> {
        unsafe {
            checked(exo_liteeth_detach())?;
        }
        self.notification.destroy().map_err(|_| -5)?;
        unsafe {
            (*core::ptr::addr_of_mut!(BRIDGE)).notification = 0;
            (*core::ptr::addr_of_mut!(BRIDGE)).length = 0;
        }
        CLAIMED.store(false, Ordering::Release);
        Ok(())
    }
}
/// 以下 C 桥只由所属 C 任务调用；分配器保持 16 字节对齐且零填充。
#[no_mangle]
extern "C" fn exo_lx_alloc(size: usize) -> *mut c_void {
    if size == 0 || size > 65536 {
        return core::ptr::null_mut();
    }
    match Layout::from_size_align(size, 16) {
        Ok(layout) => unsafe { alloc_zeroed(layout).cast() },
        Err(_) => core::ptr::null_mut(),
    }
}
#[no_mangle]
unsafe extern "C" fn exo_lx_free(ptr: *mut c_void, size: usize) {
    // SAFETY: C 只释放 alloc 的原始指针，并保留相同 size；所有权交接在 adapter.c。
    if !ptr.is_null() {
        unsafe {
            dealloc(ptr.cast(), Layout::from_size_align(size, 16).unwrap());
        }
    }
}
#[no_mangle]
extern "C" fn exo_lx_map(pa: u64, size: u64, index: u32) -> *mut c_void {
    // 只接受页对齐的两个小设备窗口；EL1 再核对 MMIO grant 和 VA 空闲状态。
    let va = match index {
        0 if size <= 4096 => MAC_VA,
        1 if size <= 65536 => BUFFER_VA,
        _ => return core::ptr::null_mut(),
    };
    if pa & 4095 != 0 || size == 0 || size & 4095 != 0 {
        return core::ptr::null_mut();
    }
    if runtime::map_mmio(pa, size, va).is_ok() {
        va as *mut c_void
    } else {
        core::ptr::null_mut()
    }
}
#[no_mangle]
extern "C" fn exo_lx_unmap(va: *mut c_void, size: u64) -> i32 {
    if runtime::unmap_mmio(va as u64, size).is_ok() {
        0
    } else {
        -5
    }
}
#[no_mangle]
extern "C" fn exo_lx_bind_irq(irq: u32) -> i32 {
    unsafe {
        let p = core::ptr::addr_of!(BRIDGE);
        if irq != (*p).intid {
            return -22;
        }
        if Notification::from_raw((*p).notification)
            .bind_irq(irq, 1)
            .is_ok()
        {
            0
        } else {
            -5
        }
    }
}
#[no_mangle]
extern "C" fn exo_lx_unbind_irq(irq: u32) -> i32 {
    if runtime::irq_unbind(irq).is_ok() {
        0
    } else {
        -5
    }
}
#[no_mangle]
unsafe extern "C" fn exo_lx_receive(frame: *const u8, length: usize) -> i32 {
    // SAFETY: C 在 netif_rx 消费 skb 前同步传入有效 L2 字节；回调不保存该指针。
    unsafe {
        let p = core::ptr::addr_of_mut!(BRIDGE);
        if length > 2048 || (*p).length != 0 {
            return 1;
        }
        core::ptr::copy_nonoverlapping(frame, core::ptr::addr_of_mut!((*p).frame).cast(), length);
        (*p).length = length;
        0
    }
}
#[no_mangle]
unsafe extern "C" fn exo_lx_puts(bytes: *const u8, length: usize) {
    unsafe {
        runtime::puts(core::slice::from_raw_parts(bytes, length));
    }
}
#[no_mangle]
extern "C" fn exo_lx_ticks() -> u64 {
    runtime::counter()
}
#[no_mangle]
extern "C" fn exo_lx_frequency() -> u64 {
    runtime::counter_frequency()
}
#[no_mangle]
extern "C" fn exo_lx_fault() -> ! {
    runtime::puts(b"[liteeth] adapter bounds/resource fault\r\n");
    runtime::exit(0x701)
}
