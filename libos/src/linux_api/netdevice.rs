// SPDX-License-Identifier: GPL-2.0-or-later
//! 网络设备对象、队列、每 CPU 统计及自动停机；堆对象地址在注销和 devm 回收前保持稳定。
use super::{
    error::*,
    kernel, memory,
    platform::{device, manage_memory},
    skbuff::*,
};
use alloc::{collections::VecDeque, vec::Vec};
use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};
pub type netdev_tx_t = u32;
pub const NETDEV_TX_OK: netdev_tx_t = 0;
pub const NETDEV_TX_BUSY: netdev_tx_t = 0x10;
#[repr(C)]
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct net_device_stats {
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
    pub multicast: u64,
    pub collisions: u64,
    pub rx_length_errors: u64,
    pub rx_over_errors: u64,
    pub rx_crc_errors: u64,
    pub rx_frame_errors: u64,
    pub rx_fifo_errors: u64,
    pub rx_missed_errors: u64,
    pub tx_aborted_errors: u64,
    pub tx_carrier_errors: u64,
    pub tx_fifo_errors: u64,
    pub tx_heartbeat_errors: u64,
    pub tx_window_errors: u64,
    pub rx_compressed: u64,
    pub tx_compressed: u64,
}
#[repr(C)]
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct rtnl_link_stats64 {
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
    pub multicast: u64,
    pub collisions: u64,
    pub rx_length_errors: u64,
    pub rx_over_errors: u64,
    pub rx_crc_errors: u64,
    pub rx_frame_errors: u64,
    pub rx_fifo_errors: u64,
    pub rx_missed_errors: u64,
    pub tx_aborted_errors: u64,
    pub tx_carrier_errors: u64,
    pub tx_fifo_errors: u64,
    pub tx_heartbeat_errors: u64,
    pub tx_window_errors: u64,
    pub rx_compressed: u64,
    pub tx_compressed: u64,
    pub rx_nohandler: u64,
    pub rx_otherhost_dropped: u64,
}
#[derive(Default)]
pub struct pcpu_sw_netstats {
    pub rx_packets: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub tx_packets: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub syncp: AtomicU64,
}
pub struct net_device_ops {
    pub ndo_open: unsafe fn(*mut net_device) -> i32,
    pub ndo_stop: unsafe fn(*mut net_device) -> i32,
    pub ndo_get_stats64: unsafe fn(*mut net_device, *mut rtnl_link_stats64),
    pub ndo_start_xmit: unsafe fn(*mut sk_buff, *mut net_device) -> netdev_tx_t,
}
#[derive(Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct DeviceName([u8; 16]);
impl DeviceName {
    fn new() -> Self {
        let mut n = Self([0; 16]);
        n.0[..5].copy_from_slice(b"eth%d");
        n
    }
    pub fn as_ptr(&self) -> *const core::ffi::c_char {
        self.0.as_ptr().cast()
    }
    pub fn as_str(&self) -> &str {
        let n = self.0.iter().position(|&c| c == 0).unwrap_or(16);
        core::str::from_utf8(&self.0[..n]).expect("框架生成 ASCII 网卡名")
    }
    fn set(&mut self, name: &str) {
        assert!(name.len() < 16);
        self.0.fill(0);
        self.0[..name.len()].copy_from_slice(name.as_bytes());
    }
}
impl core::fmt::Display for DeviceName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}
// 对应 ARM64 Linux：网卡对象和尾随私有区按 64 字节缓存行对齐。
#[repr(C, align(64))]
pub struct net_device {
    pub(crate) access: super::lock::Lock<()>,
    pub name: DeviceName,
    pub dev: device,
    pub irq: u32,
    pub dev_addr: [u8; 6],
    pub stats: net_device_stats,
    pub tstats: *mut pcpu_sw_netstats,
    pub netdev_ops: *const net_device_ops,
    pub addr_assign_type: u8,
    pub dsa_without_ethertype: bool,
    pub registered: bool,
    pub running: bool,
    pub stopped: bool,
    pub carrier: bool,
    pub tx_scheduled: bool,
    pub link_events: Vec<bool>,
    pub rx_queue: VecDeque<*mut sk_buff>,
    pub tx_queue: VecDeque<*mut sk_buff>,
}
unsafe fn drop_netdev(p: *mut u8) {
    let n = p.cast::<net_device>();
    while let Some(s) = (*n).rx_queue.pop_front() {
        dev_kfree_skb_any(s);
    }
    while let Some(s) = (*n).tx_queue.pop_front() {
        dev_kfree_skb_any(s);
    }
    // 这里只析构对象；devres 随后整体释放网卡对象与尾随私有区。
    ptr::drop_in_place(n);
}
pub unsafe fn devm_alloc_etherdev(dev: *mut device, sizeof_priv: i32) -> *mut net_device {
    if sizeof_priv < 0 {
        return ptr::null_mut();
    }
    // sizeof(net_device) 已含末尾对齐填充，私有区紧随其后；只申请一次堆内存。
    let size = match core::mem::size_of::<net_device>().checked_add(sizeof_priv as usize) {
        Some(size) => size,
        None => return ptr::null_mut(),
    };
    let align = core::mem::align_of::<net_device>();
    let n = memory::allocate(size, align).cast::<net_device>();
    if n.is_null() {
        return ptr::null_mut();
    }
    // 整块已清零，再构造对象中的锁、队列等 Rust 字段；尾随私有区保持清零。
    ptr::write(
        n,
        net_device {
            access: super::lock::Lock::new(()),
            name: DeviceName::new(),
            dev: device::new("eth0", (*dev).of_node),
            irq: 0,
            dev_addr: [0; 6],
            stats: Default::default(),
            tstats: ptr::null_mut(),
            netdev_ops: ptr::null(),
            addr_assign_type: 0,
            dsa_without_ethertype: false,
            registered: false,
            running: false,
            stopped: true,
            carrier: false,
            tx_scheduled: false,
            link_events: Vec::new(),
            rx_queue: VecDeque::new(),
            tx_queue: VecDeque::new(),
        },
    );
    if !manage_memory(dev, n.cast(), size, align, Some(drop_netdev)) {
        drop_netdev(n.cast());
        memory::release(n.cast(), size, align);
        return ptr::null_mut();
    }
    n
}
pub unsafe fn netdev_priv(dev: *const net_device) -> *mut c_void {
    // 与 Linux dev->priv 的关系一致：从稳定的对象基址计算尾随私有区地址。
    dev.cast::<u8>()
        .add(core::mem::size_of::<net_device>())
        .cast_mut()
        .cast()
}
pub unsafe fn SET_NETDEV_DEV(net: *mut net_device, pdev: *mut device) {
    (*net).dev.parent = pdev;
}
pub unsafe fn alloc_pcpu_stats<T: Default>(dev: *mut device) -> *mut T {
    let count = (kernel::ops().cpus)().max(1);
    let size = match core::mem::size_of::<T>().checked_mul(count) {
        Some(s) => s,
        None => return ptr::null_mut(),
    };
    let p = memory::allocate(size, core::mem::align_of::<T>()).cast::<T>();
    if p.is_null() {
        return p;
    }
    for cpu in 0..count {
        ptr::write(p.add(cpu), T::default());
    }
    if !manage_memory(dev, p.cast(), size, core::mem::align_of::<T>(), None) {
        memory::release(p.cast(), size, core::mem::align_of::<T>());
        return ptr::null_mut();
    }
    p
}
// 稳定地址以整数保存在带锁目录；解引用仍受设备的注册生命周期约束。
static DEVICES: super::lock::Lock<Vec<usize>> = super::lock::Lock::new(Vec::new());
pub unsafe fn register_netdev(dev: *mut net_device) -> i32 {
    if (*dev).registered {
        return -EBUSY;
    }
    if (*dev).netdev_ops.is_null() {
        return -EINVAL;
    }
    let mut devices = DEVICES.lock();
    if devices.try_reserve(1).is_err() {
        return -ENOMEM;
    }
    let mut index = 0;
    loop {
        let mut name = alloc::string::String::new();
        if name.try_reserve(23).is_err() {
            return -ENOMEM;
        }
        use core::fmt::Write;
        let _ = write!(&mut name, "eth{}", index);
        if !devices
            .iter()
            .any(|p| (*(*p as *mut net_device)).name.as_str() == name)
        {
            (*dev).name.set(&name);
            break;
        }
        index += 1;
    }
    devices.push(dev as usize);
    (*dev).registered = true;
    0
}
pub fn registered_devices() -> usize {
    DEVICES.lock().len()
}
pub unsafe fn dev_open(dev: *mut net_device) -> i32 {
    if !(*dev).registered {
        return -ENODEV;
    }
    if (*dev).running {
        return -EBUSY;
    }
    let err = ((*(*dev).netdev_ops).ndo_open)(dev);
    if err == 0 {
        (*dev).running = true;
    }
    err
}
pub unsafe fn dev_close(dev: *mut net_device) {
    if (*dev).running {
        ((*(*dev).netdev_ops).ndo_stop)(dev);
        (*dev).running = false;
    }
}
pub unsafe fn unregister_netdev(dev: *mut net_device) {
    let _guard = lock_device(dev);
    assert!((*dev).registered, "注销未注册网卡");
    dev_close(dev);
    let mut devices = DEVICES.lock();
    let i = devices
        .iter()
        .position(|&p| p == dev as usize)
        .expect("网卡不在注册目录");
    devices.remove(i);
    (*dev).registered = false;
    while let Some(s) = (*dev).tx_queue.pop_front() {
        dev_kfree_skb_any(s);
    }
    while let Some(s) = (*dev).rx_queue.pop_front() {
        dev_kfree_skb_any(s);
    }
}
pub unsafe fn netif_queue_stopped(dev: *const net_device) -> bool {
    (*dev).stopped
}
pub unsafe fn netif_start_queue(dev: *mut net_device) {
    (*dev).stopped = false;
}
pub unsafe fn netif_stop_queue(dev: *mut net_device) {
    (*dev).stopped = true;
}
pub unsafe fn netif_wake_queue(dev: *mut net_device) {
    if (*dev).stopped {
        (*dev).stopped = false;
        (*dev).tx_scheduled = true;
    }
}
pub unsafe fn netif_carrier_on(dev: *mut net_device) {
    if !(*dev).carrier {
        (*dev).carrier = true;
        (*dev).link_events.push(true);
        (*dev).tx_scheduled = true;
    }
}
pub unsafe fn netif_carrier_off(dev: *mut net_device) {
    if (*dev).carrier {
        (*dev).carrier = false;
        (*dev).link_events.push(false);
    }
}
/// 发送核心持有积压 skb；BUSY 时保留原对象，由 TX 完成后调度重试。
pub unsafe fn enqueue_tx(dev: *mut net_device, skb: *mut sk_buff) -> i32 {
    if !(*dev).running {
        dev_kfree_skb_any(skb);
        return -ENODEV;
    }
    if (*dev).tx_queue.try_reserve(1).is_err() {
        dev_kfree_skb_any(skb);
        return -ENOMEM;
    }
    (*dev).tx_queue.push_back(skb);
    (*dev).tx_scheduled = true;
    run_scheduled_tx(dev);
    0
}
pub unsafe fn run_scheduled_tx(dev: *mut net_device) {
    if !(*dev).tx_scheduled || !(*dev).running || !(*dev).carrier {
        return;
    }
    (*dev).tx_scheduled = false;
    while !(*dev).stopped {
        let Some(skb) = (*dev).tx_queue.pop_front() else {
            break;
        };
        let result = ((*(*dev).netdev_ops).ndo_start_xmit)(skb, dev);
        if result == NETDEV_TX_BUSY {
            (*dev).tx_queue.push_front(skb);
            break;
        }
    }
}
pub unsafe fn dev_sw_netstats_rx_add(dev: *mut net_device, len: u32) {
    let s = &*(*dev).tstats.add((kernel::ops().cpu)());
    s.syncp.fetch_add(1, Ordering::AcqRel);
    s.rx_packets.fetch_add(1, Ordering::Relaxed);
    s.rx_bytes.fetch_add(len as u64, Ordering::Relaxed);
    s.syncp.fetch_add(1, Ordering::Release);
}
pub unsafe fn dev_sw_netstats_tx_add(dev: *mut net_device, packets: u32, len: u32) {
    let s = &*(*dev).tstats.add((kernel::ops().cpu)());
    s.syncp.fetch_add(1, Ordering::AcqRel);
    s.tx_packets.fetch_add(packets as u64, Ordering::Relaxed);
    s.tx_bytes.fetch_add(len as u64, Ordering::Relaxed);
    s.syncp.fetch_add(1, Ordering::Release);
}
pub unsafe fn netdev_stats_to_stats64(
    stats64: *mut rtnl_link_stats64,
    netdev_stats: *const net_device_stats,
) {
    ptr::write(stats64, rtnl_link_stats64::default());
    (*stats64).rx_packets = (*netdev_stats).rx_packets;
    (*stats64).tx_packets = (*netdev_stats).tx_packets;
    (*stats64).rx_bytes = (*netdev_stats).rx_bytes;
    (*stats64).tx_bytes = (*netdev_stats).tx_bytes;
    (*stats64).rx_errors = (*netdev_stats).rx_errors;
    (*stats64).tx_errors = (*netdev_stats).tx_errors;
    (*stats64).rx_dropped = (*netdev_stats).rx_dropped;
    (*stats64).tx_dropped = (*netdev_stats).tx_dropped;
    (*stats64).multicast = (*netdev_stats).multicast;
    (*stats64).collisions = (*netdev_stats).collisions;
    (*stats64).rx_length_errors = (*netdev_stats).rx_length_errors;
    (*stats64).rx_over_errors = (*netdev_stats).rx_over_errors;
    (*stats64).rx_crc_errors = (*netdev_stats).rx_crc_errors;
    (*stats64).rx_frame_errors = (*netdev_stats).rx_frame_errors;
    (*stats64).rx_fifo_errors = (*netdev_stats).rx_fifo_errors;
    (*stats64).rx_missed_errors = (*netdev_stats).rx_missed_errors;
    (*stats64).tx_aborted_errors = (*netdev_stats).tx_aborted_errors;
    (*stats64).tx_carrier_errors = (*netdev_stats).tx_carrier_errors;
    (*stats64).tx_fifo_errors = (*netdev_stats).tx_fifo_errors;
    (*stats64).tx_heartbeat_errors = (*netdev_stats).tx_heartbeat_errors;
    (*stats64).tx_window_errors = (*netdev_stats).tx_window_errors;
    (*stats64).rx_compressed = (*netdev_stats).rx_compressed;
    (*stats64).tx_compressed = (*netdev_stats).tx_compressed;
}
pub unsafe fn dev_fetch_sw_netstats(s: *mut rtnl_link_stats64, netstats: *const pcpu_sw_netstats) {
    for cpu in 0..(kernel::ops().cpus)().max(1) {
        let t = &*netstats.add(cpu);
        loop {
            let seq = t.syncp.load(Ordering::Acquire);
            if seq & 1 != 0 {
                continue;
            }
            let v = [
                t.rx_packets.load(Ordering::Relaxed),
                t.rx_bytes.load(Ordering::Relaxed),
                t.tx_packets.load(Ordering::Relaxed),
                t.tx_bytes.load(Ordering::Relaxed),
            ];
            if seq == t.syncp.load(Ordering::Acquire) {
                (*s).rx_packets += v[0];
                (*s).rx_bytes += v[1];
                (*s).tx_packets += v[2];
                (*s).tx_bytes += v[3];
                break;
            }
        }
    }
}
pub unsafe fn eth_hw_addr_random(dev: *mut net_device) {
    let mut addr = [0; 6];
    super::random::bytes(&mut addr);
    addr[0] = (addr[0] & !1) | 2;
    (*dev).dev_addr = addr;
    (*dev).addr_assign_type = 1;
}

/// 框架持锁访问设备，避免应用线程与 IRQ 回调并发修改 skb 队列和普通统计字段。
/// SAFETY: 调用者保证 dev 在 guard 释放前存活；等待时不可停止调度器。
pub unsafe fn lock_device<'a>(dev: *mut net_device) -> super::lock::Guard<'a, ()> {
    loop {
        if let Some(g) = (*dev).access.try_lock() {
            return g;
        }
        (kernel::ops().yield_now)();
    }
}
pub fn irq_gate(dev: *mut c_void) -> usize {
    let devices = DEVICES.lock();
    if devices.contains(&(dev as usize)) {
        unsafe { core::ptr::addr_of!((*(dev as *mut net_device)).access) as usize }
    } else {
        0
    }
}
