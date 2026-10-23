// SPDX-License-Identifier: GPL-2.0-or-later
//! 单所属线程的网卡对象、发送背压、链路和统计同名封装。
//! 这里保存状态，不提供 Linux qdisc/NAPI、per-CPU 并发或完整网络栈。
use super::error::{EBUSY, EINVAL};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtnlLinkStats64 {
    /// 单线程拥有的已累计报文/字节及异常计数，不与 IRQ 线程并发修改。
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
}
#[derive(Clone, Copy, Default)]
pub struct SwNetStats {
    /// 本后端只有一个所属线程；不是 Linux 每 CPU 分配器。
    pub rx_packets: u64,
    pub rx_bytes: u64,
    pub tx_packets: u64,
    pub tx_bytes: u64,
}
pub struct NetDevice<P = ()> {
    /// 诊断名及父设备名，不是内核 handle。
    pub name: &'static str,
    pub parent: Option<&'static str>,
    /// Rust 拥有的私有数据，替代 C 私有指针的生命周期。
    pub private: P,
    /// 发送暂停、链路发布和本地注册状态，只由所属线程修改。
    pub stopped: bool,
    pub carrier: bool,
    pub registered: bool,
    /// 网卡身份，不把它隐式写入设备硬件寄存器。
    pub address: [u8; 6],
    /// 基础异常计数和单所属线程软件收发计数。
    pub stats: RtnlLinkStats64,
    pub tstats: SwNetStats,
}
pub fn devm_alloc_etherdev<P>(name: &'static str, private: P) -> NetDevice<P> {
    NetDevice {
        name,
        private,
        parent: None,
        stopped: true,
        carrier: false,
        registered: false,
        address: [0; 6],
        stats: RtnlLinkStats64::default(),
        tstats: devm_netdev_alloc_pcpu_stats(),
    }
}
/// 值类型随 NetDevice 回收；单线程实现不冒充 Linux per-CPU 并发语义。
pub fn devm_netdev_alloc_pcpu_stats() -> SwNetStats {
    SwNetStats::default()
}
pub fn netdev_priv<P>(dev: &mut NetDevice<P>) -> &mut P {
    &mut dev.private
}
pub fn SET_NETDEV_DEV<P>(dev: &mut NetDevice<P>, parent: &'static str) {
    dev.parent = Some(parent);
}
/// 本地网卡对象注册状态，不会创建 Linux socket/网络命名空间或发布 rtnetlink。
pub fn register_netdev<P>(dev: &mut NetDevice<P>) -> Result<(), i32> {
    if dev.registered {
        return Err(EBUSY);
    }
    dev.registered = true;
    Ok(())
}
/// 调用者必须已执行设备 stop/free_irq。拒绝带活跃发送状态注销，避免假装完成异步停机。
pub fn unregister_netdev<P>(dev: &mut NetDevice<P>) -> Result<(), i32> {
    if !dev.registered {
        return Err(EINVAL);
    }
    if !dev.stopped {
        return Err(EBUSY);
    }
    dev.registered = false;
    Ok(())
}
pub fn netif_queue_stopped<P>(dev: &NetDevice<P>) -> bool {
    dev.stopped
}
pub fn netif_start_queue<P>(dev: &mut NetDevice<P>) {
    dev.stopped = false;
}
pub fn netif_stop_queue<P>(dev: &mut NetDevice<P>) {
    dev.stopped = true;
}
pub fn netif_wake_queue<P>(dev: &mut NetDevice<P>) {
    netif_start_queue(dev);
}
pub fn netif_carrier_on<P>(dev: &mut NetDevice<P>) {
    dev.carrier = true;
}
pub fn netif_carrier_off<P>(dev: &mut NetDevice<P>) {
    dev.carrier = false;
}
pub fn dev_sw_netstats_rx_add<P>(dev: &mut NetDevice<P>, len: usize) {
    dev.tstats.rx_packets += 1;
    dev.tstats.rx_bytes += len as u64;
}
pub fn dev_sw_netstats_tx_add<P>(dev: &mut NetDevice<P>, count: usize, len: usize) {
    dev.tstats.tx_packets += count as u64;
    dev.tstats.tx_bytes += len as u64;
}
pub fn netdev_stats_to_stats64(to: &mut RtnlLinkStats64, from: &RtnlLinkStats64) {
    *to = *from;
}
pub fn dev_fetch_sw_netstats(to: &mut RtnlLinkStats64, from: &SwNetStats) {
    to.rx_packets += from.rx_packets;
    to.rx_bytes += from.rx_bytes;
    to.tx_packets += from.tx_packets;
    to.tx_bytes += from.tx_bytes;
}
/// 调用者提供熵，封装不以固定地址或计数器冒充系统随机源；产生本地管理单播地址。
pub fn eth_hw_addr_random<P>(dev: &mut NetDevice<P>, mut random: [u8; 6]) {
    random[0] = (random[0] & !1) | 2;
    dev.address = random;
}
