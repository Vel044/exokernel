// SPDX-License-Identifier: GPL-2.0-or-later
//! 根据 Linux 6.12.75 litex_liteeth.c 定向重写的设备状态机。
//! 保留 CSR/SRAM 协议、slot 轮转、忙时重试和事件确认顺序；通用行为调用 linux_api 同名接口。
//! 本模块只操作设备和有界帧，MMIO 映射、Notification、ACK 由 liteeth.rs 提供。
//! 原驱动函数名保留：liteeth_rx、liteeth_interrupt、liteeth_start_xmit、liteeth_get_stats64。
//! 源码对应关系与主动收紧的边界见 docs/LiteEth/翻译契约.json。

pub use crate::linux_api::{io::Io, netdevice::RtnlLinkStats64 as Stats};
use crate::linux_api::{
    io::*,
    netdevice::*,
    platform::{of_property_read_u32, PlatformDevice},
    skbuff::*,
};

pub const RX_SLOT: usize = 0x00;
pub const RX_LENGTH: usize = 0x04;
pub const RX_PENDING: usize = 0x10;
pub const RX_ENABLE: usize = 0x14;
pub const TX_START: usize = 0x18;
pub const TX_READY: usize = 0x1c;
pub const TX_SLOT: usize = 0x24;
pub const TX_LENGTH: usize = 0x28;
pub const TX_PENDING: usize = 0x30;
pub const TX_ENABLE: usize = 0x34;
pub const MAX_FRAME: usize = 2048;

/// 已验证的设备 SRAM 布局；先 RX slots，随后 TX slots，不使用 DMA 主存。
#[derive(Clone, Copy)]
pub struct Layout {
    /// RX slot 个数，硬件 slot 寄存器为 8 位。
    pub rx_slots: usize,
    /// TX slot 个数，硬件 slot 寄存器为 8 位。
    pub tx_slots: usize,
    /// 每个 slot 的字节容量，当前限定不超过 2048。
    pub slot_size: usize,
}
impl Layout {
    pub fn validate(self, buffer_size: usize) -> Result<Self, i32> {
        if self.rx_slots == 0
            || self.tx_slots == 0
            || self.rx_slots > 256
            || self.tx_slots > 256
            || self.slot_size == 0
            || self.slot_size > MAX_FRAME
            || (self.rx_slots + self.tx_slots)
                .checked_mul(self.slot_size)
                .ok_or(-22)?
                > buffer_size
        {
            return Err(-22);
        }
        Ok(self)
    }
}
/// 对应原 liteeth_setup_slots：从已解析属性取得槽布局，并验证资源容量。
/// 原驱动 2/2/2048 默认值由 dtb::discover 在进入这里之前补齐；错误不能静默扩大资源。
pub fn liteeth_setup_slots<P>(
    platform: &PlatformDevice<'_, P>,
    buffer_size: usize,
) -> Result<Layout, i32> {
    Layout {
        rx_slots: of_property_read_u32(platform, "litex,rx-slots")? as usize,
        tx_slots: of_property_read_u32(platform, "litex,tx-slots")? as usize,
        slot_size: of_property_read_u32(platform, "litex,slot-size")? as usize,
    }
    .validate(buffer_size)
}

/// 单所属线程的驱动状态；不含全局 C 对象、回调指针或 Linux 队列。
pub struct Device<I: Io> {
    /// MMIO 访问对象，真实后端同时拥有两个映射。
    io: I,
    /// 已核查的 SRAM 分区。
    layout: Layout,
    /// 下一个 TX slot，发送成功后才轮转，BUSY 保持不变。
    tx_slot: usize,
    /// 仅 IRQ 绑定成功并开放设备事件后才为 true。
    opened: bool,
    /// 通用网卡对象持有队列、链路和统计；不是 Linux C struct 的内存布局。
    netdev: NetDevice,
    /// 同名 skb/交付队列拥有完整帧；默认单槽，满时保留先到帧。
    receive_queue: RxQueue<MAX_FRAME>,
    /// 本地交付槽满的次数，与硬件 RX 错误分开，避免伪造原驱动统计。
    pub delivery_drops: u64,
}
impl<I: Io> Device<I> {
    pub fn new(io: I, layout: Layout, buffer_size: usize) -> Result<Self, i32> {
        let layout = layout.validate(buffer_size)?;
        let mut netdev = devm_alloc_etherdev("liteeth", ());
        register_netdev(&mut netdev)?;
        Ok(Self {
            io,
            layout,
            tx_slot: 0,
            opened: false,
            netdev,
            receive_queue: RxQueue::new(),
            delivery_drops: 0,
        })
    }
    /// 对照 liteeth_open：先 W1C 清旧事件，调用者随后绑定 IRQ，最后 enable。
    pub fn prepare_open(&mut self) -> Result<(), i32> {
        if self.opened {
            return Err(-16);
        }
        if !self.netdev.registered {
            return Err(-22);
        }
        litex_write8(&self.io, RX_PENDING, 1);
        litex_write8(&self.io, TX_PENDING, 1);
        Ok(())
    }
    /// 调用前必须已成功绑定内核 IRQ；与原 open 中两次事件使能的顺序一致。
    pub fn enable(&mut self) {
        litex_write8(&self.io, RX_ENABLE, 1);
        litex_write8(&self.io, TX_ENABLE, 1);
        self.opened = true;
        netif_carrier_on(&mut self.netdev);
        netif_start_queue(&mut self.netdev);
    }
    /// 对照 liteeth_stop：先停队列和关设备事件，再由上层解绑 GIC 路由。
    pub fn stop_device_events(&mut self) {
        netif_stop_queue(&mut self.netdev);
        netif_carrier_off(&mut self.netdev);
        litex_write8(&self.io, RX_ENABLE, 0);
        litex_write8(&self.io, TX_ENABLE, 0);
        self.opened = false;
    }
    /// 对照 liteeth_start_xmit。true 包含原驱动的“超长帧已消费但丢弃”语义。
    /// BUSY 返回 false，不保存 bytes 借用，不改 slot 和发送统计，调用者持有原帧。
    pub fn liteeth_start_xmit(&mut self, bytes: &[u8]) -> Result<bool, i32> {
        if !self.opened || bytes.len() < 14 {
            return Err(-22);
        }
        if netif_queue_stopped(&self.netdev) {
            return Ok(false);
        }
        if litex_read8(&self.io, TX_READY) == 0 {
            netif_stop_queue(&mut self.netdev);
            return Ok(false);
        }
        if bytes.len() > self.layout.slot_size {
            self.netdev.stats.tx_errors += 1;
            self.netdev.stats.tx_dropped += 1;
            return Ok(true);
        }
        let offset = (self.layout.rx_slots + self.tx_slot) * self.layout.slot_size;
        memcpy_toio(&self.io, offset, bytes);
        litex_write8(&self.io, TX_SLOT, self.tx_slot as u8);
        litex_write16(&self.io, TX_LENGTH, bytes.len() as u16);
        litex_write8(&self.io, TX_START, 1);
        dev_sw_netstats_tx_add(&mut self.netdev, 1, bytes.len());
        self.tx_slot = (self.tx_slot + 1) % self.layout.tx_slots;
        Ok(true)
    }
    /// 对照 liteeth_interrupt：TX pending → 唤醒发送 → W1C；再 RX → 复制 → W1C。
    /// 与 xHCI 不同，本设备没有 Event Ring；这里处理的是 LiteEth pending 和 SRAM。
    pub fn liteeth_interrupt(&mut self) {
        let tx = litex_read8(&self.io, TX_PENDING);
        if tx != 0 {
            netif_wake_queue(&mut self.netdev);
            litex_write8(&self.io, TX_PENDING, tx);
        }
        let rx = litex_read8(&self.io, RX_PENDING);
        if rx != 0 {
            self.liteeth_rx();
            litex_write8(&self.io, RX_PENDING, rx);
        }
    }
    pub(crate) fn liteeth_rx(&mut self) {
        let slot = litex_read8(&self.io, RX_SLOT) as usize;
        let length = litex_read32(&self.io, RX_LENGTH) as usize;
        // 原 C 只检查长度 0/2048；Rust 显式验证 slot、容量及 L2 头，禁止越界读取。
        if length < 14
            || length > MAX_FRAME
            || length > self.layout.slot_size
            || slot >= self.layout.rx_slots
        {
            self.netdev.stats.rx_errors += 1;
            self.netdev.stats.rx_dropped += 1;
            return;
        }
        dev_sw_netstats_rx_add(&mut self.netdev, length);
        if self.receive_queue.is_full() {
            self.delivery_drops += 1;
            return;
        }
        // 同名接口保留原 skb 生命周期；当前容量在类型中固定，已验证 length 后不会分配失败。
        let mut skb = netdev_alloc_skb_ip_align::<_, MAX_FRAME>(&self.netdev, length)
            .expect("已验证的 RX 容量");
        memcpy_fromio(
            &self.io,
            slot * self.layout.slot_size,
            skb_put(&mut skb, length).unwrap(),
        );
        eth_type_trans(&mut skb, &self.netdev).expect("已验证的以太网头");
        if netif_rx(&mut self.receive_queue, skb).is_err() {
            self.delivery_drops += 1;
        }
    }
    /// 不截断帧；缓冲不足时返回错误，仍保留该帧以便调用者重新读取。
    pub fn receive(&mut self, out: &mut [u8]) -> Result<usize, i32> {
        self.receive_queue.receive(out)
    }
    pub fn liteeth_get_stats64(&self) -> Stats {
        let mut result = Stats::default();
        netdev_stats_to_stats64(&mut result, &self.netdev.stats);
        dev_fetch_sw_netstats(&mut result, &self.netdev.tstats);
        result
    }
    /// 设备事件和 IRQ 已关闭后注销本地网络对象；不模拟 Linux 自动 stop 回调。
    pub fn unregister(&mut self) -> Result<(), i32> {
        unregister_netdev(&mut self.netdev)
    }
}
