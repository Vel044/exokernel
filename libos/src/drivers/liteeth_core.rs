// SPDX-License-Identifier: GPL-2.0-or-later
//! 根据 Linux 6.12.75 litex_liteeth.c 定向重写的设备状态机。
//! 保留 CSR/SRAM 协议、slot 轮转、忙时重试和事件确认顺序；不实现 Linux API。
//! 本模块只操作设备和有界帧，MMIO 映射、Notification、ACK 由 liteeth.rs 提供。
//! 源码对应关系与主动收紧的边界见 docs/兼容性研究/LiteEth/翻译契约.json。

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
/// 数据路径统计；字段含义对照原驱动，不依赖 Linux struct 布局。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
/// 设备访问边界。真实实现是 Device 映射上的 volatile 操作；宿主测试实现记录事务。
/// read/write 必须保持 CSR32 事务，copy 使用 CPU VA，不得替换为 DMA 地址。
pub trait Io {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
    fn copy_from(&self, offset: usize, out: &mut [u8]);
    fn copy_to(&self, offset: usize, bytes: &[u8]);
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
    /// 上次发送发现硬件忙；TX 完成事件到达后重新允许提交。
    stopped: bool,
    /// 收包副本由本对象拥有，不指向设备 SRAM；避免 skb 和每包堆分配。
    frame: [u8; MAX_FRAME],
    /// 零表示接收槽空闲；上层读取后归零。
    length: usize,
    /// 与原驱动对应的收发及丢弃计数。
    stats: Stats,
    /// 本地交付槽满的次数，与硬件 RX 错误分开，避免伪造原驱动统计。
    pub delivery_drops: u64,
}
impl<I: Io> Device<I> {
    pub fn new(io: I, layout: Layout, buffer_size: usize) -> Result<Self, i32> {
        let layout = layout.validate(buffer_size)?;
        Ok(Self {
            io,
            layout,
            tx_slot: 0,
            opened: false,
            stopped: true,
            frame: [0; MAX_FRAME],
            length: 0,
            stats: Stats::default(),
            delivery_drops: 0,
        })
    }
    /// 对照 liteeth_open：先 W1C 清旧事件，调用者随后绑定 IRQ，最后 enable。
    pub fn prepare_open(&mut self) -> Result<(), i32> {
        if self.opened {
            return Err(-16);
        }
        self.io.write(RX_PENDING, 1);
        self.io.write(TX_PENDING, 1);
        Ok(())
    }
    /// 调用前必须已成功绑定内核 IRQ；与原 open 中两次事件使能的顺序一致。
    pub fn enable(&mut self) {
        self.io.write(RX_ENABLE, 1);
        self.io.write(TX_ENABLE, 1);
        self.opened = true;
        self.stopped = false;
    }
    /// 对照 liteeth_stop：先停队列和关设备事件，再由上层解绑 GIC 路由。
    pub fn stop(&mut self) {
        self.stopped = true;
        self.io.write(RX_ENABLE, 0);
        self.io.write(TX_ENABLE, 0);
        self.opened = false;
    }
    /// 对照 liteeth_start_xmit。true 包含原驱动的“超长帧已消费但丢弃”语义。
    /// BUSY 返回 false，不保存 bytes 借用，不改 slot 和发送统计，调用者持有原帧。
    pub fn send(&mut self, bytes: &[u8]) -> Result<bool, i32> {
        if !self.opened || bytes.len() < 14 {
            return Err(-22);
        }
        if self.stopped {
            return Ok(false);
        }
        if self.io.read(TX_READY) as u8 == 0 {
            self.stopped = true;
            return Ok(false);
        }
        if bytes.len() > self.layout.slot_size {
            self.stats.tx_errors += 1;
            self.stats.tx_dropped += 1;
            return Ok(true);
        }
        let offset = (self.layout.rx_slots + self.tx_slot) * self.layout.slot_size;
        self.io.copy_to(offset, bytes);
        self.io.write(TX_SLOT, self.tx_slot as u32);
        self.io.write(TX_LENGTH, bytes.len() as u32);
        self.io.write(TX_START, 1);
        self.stats.tx_packets += 1;
        self.stats.tx_bytes += bytes.len() as u64;
        self.tx_slot = (self.tx_slot + 1) % self.layout.tx_slots;
        Ok(true)
    }
    /// 对照 liteeth_interrupt：TX pending → 唤醒发送 → W1C；再 RX → 复制 → W1C。
    /// 与 xHCI 不同，本设备没有 Event Ring；这里处理的是 LiteEth pending 和 SRAM。
    pub fn handle_irq(&mut self) {
        let tx = self.io.read(TX_PENDING) as u8;
        if tx != 0 {
            self.stopped = false;
            self.io.write(TX_PENDING, tx as u32);
        }
        let rx = self.io.read(RX_PENDING) as u8;
        if rx != 0 {
            self.receive_slot();
            self.io.write(RX_PENDING, rx as u32);
        }
    }
    fn receive_slot(&mut self) {
        let slot = self.io.read(RX_SLOT) as u8 as usize;
        let length = self.io.read(RX_LENGTH) as usize;
        // 原 C 只检查长度 0/2048；Rust 显式验证 slot、容量及 L2 头，禁止越界读取。
        if length < 14
            || length > MAX_FRAME
            || length > self.layout.slot_size
            || slot >= self.layout.rx_slots
        {
            self.stats.rx_errors += 1;
            self.stats.rx_dropped += 1;
            return;
        }
        self.stats.rx_packets += 1;
        self.stats.rx_bytes += length as u64;
        if self.length != 0 {
            self.delivery_drops += 1;
            return;
        }
        self.io
            .copy_from(slot * self.layout.slot_size, &mut self.frame[..length]);
        self.length = length;
    }
    /// 不截断帧；缓冲不足时返回错误，仍保留该帧以便调用者重新读取。
    pub fn receive(&mut self, out: &mut [u8]) -> Result<usize, i32> {
        if out.len() < self.length {
            return Err(-22);
        }
        let n = self.length;
        out[..n].copy_from_slice(&self.frame[..n]);
        self.length = 0;
        Ok(n)
    }
    pub fn stats(&self) -> Stats {
        self.stats
    }
}
