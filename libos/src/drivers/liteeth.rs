// SPDX-License-Identifier: GPL-2.0-or-later
//! 原生 Rust LiteEth 驱动；9 个原函数按 Linux C 文件的顺序集中排列。
//! 设备逻辑在 EL0 执行；linux_api 提供同名接口，exo 后端接 MMIO、IRQ 和 Notification。

pub use crate::linux_api::{io::Io, netdevice::rtnl_link_stats64};
use crate::linux_api::{
    io::*,
    netdevice::*,
    platform::{of_property_read_u32, platform_device},
    skbuff::*,
};
#[cfg(feature = "linux-driver-api")]
use crate::{
    drivers::platform::LiteEthInfo,
    linux_api::{
        exo::{ExoIrq, ExoMapper, Mapping},
        interrupt::{free_irq, request_irq, IrqRegistration},
        platform::{
            devm_platform_ioremap_resource_byname, platform_get_irq, platform_set_drvdata,
            NamedResource,
        },
    },
    runtime,
};
#[cfg(feature = "linux-driver-api")]
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};

// 与原 C 完全同名的寄存器偏移；WRITER 是 RX，READER 是 TX，不按软件收发方向改名。
pub const LITEETH_WRITER_SLOT: usize = 0x00;
pub const LITEETH_WRITER_LENGTH: usize = 0x04;
#[allow(dead_code)] // 原驱动保留但当前路径未使用的硬件定义。
pub const LITEETH_WRITER_ERRORS: usize = 0x08;
#[allow(dead_code)] // 原驱动保留但当前路径未使用的硬件定义。
pub const LITEETH_WRITER_EV_STATUS: usize = 0x0C;
pub const LITEETH_WRITER_EV_PENDING: usize = 0x10;
pub const LITEETH_WRITER_EV_ENABLE: usize = 0x14;
pub const LITEETH_READER_START: usize = 0x18;
pub const LITEETH_READER_READY: usize = 0x1C;
#[allow(dead_code)] // 原驱动保留但当前路径未使用的硬件定义。
pub const LITEETH_READER_LEVEL: usize = 0x20;
pub const LITEETH_READER_SLOT: usize = 0x24;
pub const LITEETH_READER_LENGTH: usize = 0x28;
#[allow(dead_code)] // 原驱动保留但当前路径未使用的硬件定义。
pub const LITEETH_READER_EV_STATUS: usize = 0x2C;
pub const LITEETH_READER_EV_PENDING: usize = 0x30;
pub const LITEETH_READER_EV_ENABLE: usize = 0x34;
#[allow(dead_code)] // 原驱动保留但当前路径未使用的硬件定义。
pub const LITEETH_PREAMBLE_CRC: usize = 0x38;
#[allow(dead_code)] // 原驱动保留但当前路径未使用的硬件定义。
pub const LITEETH_PREAMBLE_ERRORS: usize = 0x3C;
#[allow(dead_code)] // 原驱动保留但当前路径未使用的硬件定义。
pub const LITEETH_CRC_ERRORS: usize = 0x40;
#[allow(dead_code)] // 原驱动保留但当前路径未使用的硬件定义。
pub const LITEETH_PHY_CRG_RESET: usize = 0x00;
#[allow(dead_code)] // 原驱动保留但当前路径未使用的硬件定义。
pub const LITEETH_MDIO_W: usize = 0x04;
#[allow(dead_code)] // 原驱动保留但当前路径未使用的硬件定义。
pub const LITEETH_MDIO_R: usize = 0x0C;
pub const DRV_NAME: &str = "liteeth";
pub const MAX_FRAME: usize = 2048;
// TX 侧的固定 skb 容量多留一个字节，用于保留并检测超过设备槽容量的报文。
pub const MAX_TX_SKB_SIZE: usize = MAX_FRAME + 1;

// 对应原 C 的 struct liteeth；base 持有寄存器和设备 SRAM 两段映射。
#[allow(non_camel_case_types)]
pub struct liteeth<I: Io> {
    base: I,            // MMIO 访问后端
    netdev: net_device, // 网卡对象：队列、链路和统计
    slot_size: usize,   // 每槽字节数

    tx_slot: usize,      // 下一次发送槽
    num_tx_slots: usize, // 发送槽数
    tx_base: usize,      // TX 区在设备 SRAM 中的偏移

    #[allow(dead_code)]
    rx_slot: usize, // 初始接收槽；收包时以硬件寄存器为准
    num_rx_slots: usize, // 接收槽数
    rx_base: usize,      // RX 区在设备 SRAM 中的偏移

    opened: bool,                      // 已绑定 IRQ 并启用设备事件
    receive_queue: RxQueue<MAX_FRAME>, // 保存完整帧，等待上层取走
    pub delivery_drops: u64,           // 交付队列满时的丢包数
}

impl<I: Io> liteeth<I> {
    // 接收一帧：设备 SRAM → skb → 本地队列。
    pub(crate) fn liteeth_rx(&mut self) {
        // 读设备寄存器：这帧在哪个槽、有多少字节。
        let rx_slot = litex_read8(&self.base, LITEETH_WRITER_SLOT) as usize;
        let len = litex_read32(&self.base, LITEETH_WRITER_LENGTH) as usize;
        // 比原 C 多检查槽号、槽容量和以太网头，避免设备给出越界范围。
        if len < 14 || len > MAX_FRAME || len > self.slot_size || rx_slot >= self.num_rx_slots {
            self.netdev.stats.rx_errors += 1;
            self.netdev.stats.rx_dropped += 1;
            return;
        }
        // 目标侧队列满时保留先到的帧，仍累计本次有效收包。
        if self.receive_queue.is_full() {
            dev_sw_netstats_rx_add(&mut self.netdev, len);
            self.delivery_drops += 1;
            return;
        }
        // 准备 skb；内联容量固定，已校验 len。
        let mut skb =
            netdev_alloc_skb_ip_align::<_, MAX_FRAME>(&self.netdev, len).expect("已验证的 RX 容量");
        let data = skb_put(&mut skb, len).unwrap(); // 扩展有效区，取得写入位置
        memcpy_fromio(&self.base, self.rx_base + rx_slot * self.slot_size, data); // 设备 SRAM → skb
        eth_type_trans(&mut skb, &self.netdev).expect("已验证的以太网头"); // 处理协议头

        // 累计收包统计，再交给本地队列；不进入 Linux IP 协议栈。
        dev_sw_netstats_rx_add(&mut self.netdev, len);
        if netif_rx(&mut self.receive_queue, skb).is_err() {
            self.delivery_drops += 1;
        }
    }

    // 处理收发中断；保持原 C 的先 TX 后 RX 顺序。
    pub fn liteeth_interrupt(&mut self) {
        // 先处理 TX：恢复发送队列，再写 1 清 pending。
        let reg = litex_read8(&self.base, LITEETH_READER_EV_PENDING);
        if reg != 0 {
            if netif_queue_stopped(&self.netdev) {
                netif_wake_queue(&mut self.netdev);
            }
            litex_write8(&self.base, LITEETH_READER_EV_PENDING, reg);
        }
        // 再处理 RX：先复制帧，再清 pending；返回后由派发器 ACK 内核 IRQ。
        let reg = litex_read8(&self.base, LITEETH_WRITER_EV_PENDING);
        if reg != 0 {
            self.liteeth_rx();
            litex_write8(&self.base, LITEETH_WRITER_EV_PENDING, reg);
        }
    }
}

#[cfg(feature = "linux-driver-api")]
impl Driver {
    // 打开网卡：清 pending → 绑定 IRQ → 启用设备。
    pub fn liteeth_open(&mut self) -> Result<(), i32> {
        if self.irq_registration.is_bound() {
            return Err(-16);
        }
        self.r#priv.prepare_open()?; // 检查状态，清 RX/TX 旧 pending

        // IRQ_BIND：EL1 校验 INTID 授权并关联通知；回调函数仍保存在 EL0。
        request_irq(
            &mut self.irq_registration,
            self.irq,
            liteeth::liteeth_interrupt,
            0,
            DRV_NAME,
        )?;
        // 写设备 EV_ENABLE，标记链路连接并开放发送队列。
        self.r#priv.enable();
        Ok(())
    }

    // 停止网卡；所属线程串行执行，关闭时没有并行回调。
    pub fn liteeth_stop(&mut self) -> Result<(), i32> {
        if self.irq_registration.is_bound() {
            self.r#priv.stop_device_events(); // 停队列、断链路、关闭设备事件

            // IRQ_UNBIND：禁用 GIC 路由；失败时保留绑定，禁止继续回收资源。
            free_irq(&mut self.irq_registration)?;
        }
        Ok(())
    }
}

impl<I: Io> liteeth<I> {
    // 对应 ndo_start_xmit：BUSY 归还 skb；OK 时驱动消费或丢弃 skb。
    pub fn liteeth_start_xmit<const N: usize>(
        &mut self,
        skb: sk_buff<N>,
    ) -> Result<NetdevTxOutcome<N>, i32> {
        let data = skb.data();
        let len = data.len();
        // 仅已打开的网卡接受完整以太网帧。
        if !self.opened || len < 14 {
            return Err(-22);
        }
        // BUSY 时保留原帧和槽号；收到 TX 事件后再重试。
        if netif_queue_stopped(&self.netdev) {
            return Ok(NetdevTxOutcome::Busy(skb));
        }
        if litex_read8(&self.base, LITEETH_READER_READY) == 0 {
            netif_stop_queue(&mut self.netdev);
            return Ok(NetdevTxOutcome::Busy(skb));
        }
        // 超长帧按原 C 语义消费并丢弃，不提交硬件。
        if len > self.slot_size {
            self.netdev.stats.tx_errors += 1;
            self.netdev.stats.tx_dropped += 1;
            dev_kfree_skb_any(skb);
            return Ok(NetdevTxOutcome::Consumed);
        }
        // 先复制数据到设备 SRAM，再写槽号、长度和 START。
        let txbuffer = self.tx_base + self.tx_slot * self.slot_size;
        memcpy_toio(&self.base, txbuffer, data);
        litex_write8(&self.base, LITEETH_READER_SLOT, self.tx_slot as u8);
        litex_write16(&self.base, LITEETH_READER_LENGTH, len as u16);
        litex_write8(&self.base, LITEETH_READER_START, 1);
        // 累计发送统计，切换到下一个发送槽。
        dev_sw_netstats_tx_add(&mut self.netdev, 1, len);
        self.tx_slot = (self.tx_slot + 1) % self.num_tx_slots;
        dev_kfree_skb_any(skb);
        Ok(NetdevTxOutcome::Consumed)
    }

    // 汇总软件统计。
    pub fn liteeth_get_stats64(&self) -> rtnl_link_stats64 {
        // 基础错误/丢包统计 + 单所属线程的收发包/字节统计。
        let mut stats = rtnl_link_stats64::default();
        netdev_stats_to_stats64(&mut stats, &self.netdev.stats);
        dev_fetch_sw_netstats(&mut stats, &self.netdev.tstats);
        stats
    }
}

// 读取并检查 RX/TX 槽位布局。
pub fn liteeth_setup_slots<P>(
    pdev: &platform_device<'_, P>,
    buffer_size: usize,
) -> Result<Layout, i32> {
    // 取得槽数和槽大小；DTB 解析阶段已补默认值 2/2/2048。
    Layout {
        num_rx_slots: of_property_read_u32(pdev, "litex,rx-slots")? as usize,
        num_tx_slots: of_property_read_u32(pdev, "litex,tx-slots")? as usize,
        slot_size: of_property_read_u32(pdev, "litex,slot-size")? as usize,
    }
    .validate(buffer_size) // 检查槽数、槽大小及设备 SRAM 容量
}

#[cfg(feature = "linux-driver-api")]
impl Driver {
    // 准备资源并注册网卡；此时还没有启用收发中断。
    pub fn liteeth_probe(info: LiteEthInfo) -> Result<Self, i32> {
        // 将已解析的 DTB 资源整理成 pdev，下面按原 C 的顺序初始化。
        let resources = [
            NamedResource {
                name: "mac",
                base: info.mac.base,
                size: info.mac.size,
                user_va: MAC_VA,
            },
            NamedResource {
                name: "buffer",
                base: info.buffer.base,
                size: info.buffer.size,
                user_va: BUFFER_VA,
            },
        ];
        let properties = [
            ("litex,rx-slots", info.rx_slots),
            ("litex,tx-slots", info.tx_slots),
            ("litex,slot-size", info.slot_size),
        ];
        let irqs = [info.mac.intid];
        let mut pdev = platform_device {
            name: DRV_NAME,
            compatible: "litex,liteeth",
            resources: &resources,
            irqs: &irqs,
            properties: &properties,
            address: Some(info.address),
            drvdata: None,
        };

        // 创建网卡对象，关联父设备，将 netdev 交给 pdev 保存。
        let mut netdev = devm_alloc_etherdev(DRV_NAME, ());
        SET_NETDEV_DEV(&mut netdev, pdev.name);
        platform_set_drvdata(&mut pdev, netdev);

        // 初始化收发统计；当前只有一个所属线程，不实现 Linux per-CPU 分配。
        let netdev = pdev.drvdata.as_mut().ok_or(-22)?;
        netdev.tstats = devm_netdev_alloc_pcpu_stats();

        // 取得 GIC INTID，检查当前后端支持的资源范围。
        let irq = platform_get_irq(&pdev, 0)?;
        if info.mac.size < 0x44 || irq < 32 || info.mac.size > 4096 || info.buffer.size > 65536 {
            return Err(-22);
        }

        // 目标侧准备：独占固定 VA，创建 Notification；IRQ 仍在 open 时绑定。
        let claim = Claim::take()?;
        let irq_registration = IrqRegistration::new(ExoIrq::new()?);

        // MAP_MMIO：分别将寄存器区和设备 SRAM 的 PA 映射到 EL0 VA。
        let base = devm_platform_ioremap_resource_byname(&pdev, "mac", &ExoMapper)?;
        let buf_base = devm_platform_ioremap_resource_byname(&pdev, "buffer", &ExoMapper)?;

        // 读取槽位配置，再设置 RX/TX 在设备 SRAM 中的位置。
        let layout =
            liteeth_setup_slots(&pdev, usize::try_from(info.buffer.size).map_err(|_| -22)?)?;
        let netdev = pdev.drvdata.take().ok_or(-22)?; // 所有权从临时 pdev 移入驱动
        let mut r#priv = liteeth {
            base: Mmio { base, buf_base },
            netdev,
            slot_size: layout.slot_size,
            tx_slot: 0,
            num_tx_slots: layout.num_tx_slots,
            tx_base: layout.num_rx_slots * layout.slot_size,
            rx_slot: 0,
            num_rx_slots: layout.num_rx_slots,
            rx_base: 0,
            opened: false,
            receive_queue: RxQueue::new(),
            delivery_drops: 0,
        };

        // 原 C 的 MAC 地址读取/随机回退尚未接入；当前实验透传完整 L2 帧。

        // 本地注册网卡；原 C 的 netdev_ops 由下面的同名 Rust 方法承接。
        register_netdev(&mut r#priv.netdev)?;
        Ok(Self {
            r#priv,
            irq_registration,
            irq,
            _claim: claim,
            _owner: PhantomData,
        })
    }

    // 注销网卡并释放驱动资源。
    pub fn liteeth_remove(mut self) -> Result<(), i32> {
        self.liteeth_stop()?; // 先关设备事件并解绑 IRQ
        unregister_netdev(&mut self.r#priv.netdev) // 注销后由 Drop 回收映射和通知
    }
}

// 以下是 Rust 的资源封装；上面 9 个函数对应原 C。
#[cfg(feature = "linux-driver-api")]
mod target {
    use super::*;
    pub(super) const MAC_VA: u64 = 0x4050_0000;
    pub(super) const BUFFER_VA: u64 = 0x4060_0000;
    // Acquire/Release 保证旧映射回收先于再次占用固定 VA。
    static CLAIMED: AtomicBool = AtomicBool::new(false);
    pub(super) struct Claim;
    impl Claim {
        pub(super) fn take() -> Result<Self, i32> {
            CLAIMED
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .map(|_| Self)
                .map_err(|_| -16)
        }
    }
    impl Drop for Claim {
        fn drop(&mut self) {
            CLAIMED.store(false, Ordering::Release);
        }
    }
    pub(super) fn fatal() -> ! {
        runtime::puts(b"[liteeth] FAIL native resource cleanup\r\n");
        runtime::exit(0x703)
    }
    pub(super) struct Mmio {
        // CSR VA：32 位总线事务，低 8/16 位存放逻辑值。
        pub(super) base: Mapping,
        // 设备 SRAM VA：CPU volatile 复制，无 DMA 分配及缓存一致性操作。
        pub(super) buf_base: Mapping,
    }
    impl Io for Mmio {
        fn read(&self, offset: usize) -> u32 {
            self.base.region().read32(offset)
        }
        fn write(&self, offset: usize, value: u32) {
            self.base.region().write32(offset, value);
        }
        fn copy_from(&self, offset: usize, out: &mut [u8]) {
            self.buf_base.region().copy_from(offset, out);
        }
        fn copy_to(&self, offset: usize, bytes: &[u8]) {
            self.buf_base.region().copy_to(offset, bytes);
        }
    }
    pub struct Driver {
        // 设备状态机和 MMIO 映射，解绑后才随对象释放。
        pub(super) r#priv: liteeth<Mmio>,
        // 注册层持有 Notification 和函数指针；没有 pending 时 WAIT 阻塞当前线程。
        pub(super) irq_registration: IrqRegistration<liteeth<Mmio>, ExoIrq>,
        // DTB 给出的 GIC INTID，绑定仍由 EL1 grant 审核。
        pub(super) irq: u32,
        // 最后释放固定 VA 所有权，失败回滚由字段 Drop 自动完成。
        pub(super) _claim: Claim,
        pub(super) _owner: PhantomData<*mut ()>,
    }
}
#[cfg(feature = "linux-driver-api")]
pub use target::Driver;
#[cfg(feature = "linux-driver-api")]
use target::{fatal, Claim, Mmio, BUFFER_VA, MAC_VA};

#[cfg(feature = "linux-driver-api")]
impl Driver {
    // 应用调用转发到上方同名的设备实现。
    pub fn liteeth_start_xmit<const N: usize>(
        &mut self,
        skb: sk_buff<N>,
    ) -> Result<NetdevTxOutcome<N>, i32> {
        self.r#priv.liteeth_start_xmit(skb)
    }

    pub fn liteeth_get_stats64(&self) -> Result<rtnl_link_stats64, i32> {
        Ok(self.r#priv.liteeth_get_stats64())
    }

    // WAIT 返回后在 EL0 调用 liteeth_interrupt，再 ACK 内核 IRQ。
    pub fn wait_irq(&mut self) -> Result<(), i32> {
        self.irq_registration.wait_and_dispatch(&mut self.r#priv)
    }

    // 上层从本地队列取出完整帧。
    pub fn receive(&mut self, out: &mut [u8]) -> usize {
        self.r#priv.receive(out).unwrap_or_else(|_| fatal())
    }
}

#[cfg(feature = "linux-driver-api")]
impl Drop for Driver {
    fn drop(&mut self) {
        // 先关事件、解绑 IRQ，再允许字段自动释放映射和通知。
        if self.liteeth_stop().is_err() {
            fatal();
        }
    }
}

// 槽位布局；单位都是计数或字节，不是映射地址。
#[derive(Clone, Copy)]
pub struct Layout {
    // RX slot 个数，硬件 slot 寄存器为 8 位。
    pub num_rx_slots: usize,
    // TX slot 个数，硬件 slot 寄存器为 8 位。
    pub num_tx_slots: usize,
    // 每个 slot 的字节容量，当前限定不超过 2048。
    pub slot_size: usize,
}
impl Layout {
    pub fn validate(self, buffer_size: usize) -> Result<Self, i32> {
        // 禁止空槽、槽号溢出、单槽过大以及超出设备 SRAM。
        if self.num_rx_slots == 0
            || self.num_tx_slots == 0
            || self.num_rx_slots > 256
            || self.num_tx_slots > 256
            || self.slot_size == 0
            || self.slot_size > MAX_FRAME
            || (self.num_rx_slots + self.num_tx_slots)
                .checked_mul(self.slot_size)
                .ok_or(-22)?
                > buffer_size
        {
            return Err(-22);
        }
        Ok(self)
    }
}

impl<I: Io> liteeth<I> {
    // 测试构造：真实设备使用上方 probe；共用同一套布局校验和收发逻辑。
    #[cfg(test)]
    pub fn new(base: I, layout: Layout, buffer_size: usize) -> Result<Self, i32> {
        let layout = layout.validate(buffer_size)?;
        let mut netdev = devm_alloc_etherdev(DRV_NAME, ());
        register_netdev(&mut netdev)?;
        Ok(Self {
            base,
            slot_size: layout.slot_size,
            num_rx_slots: layout.num_rx_slots,
            num_tx_slots: layout.num_tx_slots,
            rx_base: 0,
            tx_base: layout.num_rx_slots * layout.slot_size,
            rx_slot: 0,
            tx_slot: 0,
            opened: false,
            netdev,
            receive_queue: RxQueue::new(),
            delivery_drops: 0,
        })
    }

    // open 的前半段：校验状态并清旧 pending。
    pub fn prepare_open(&mut self) -> Result<(), i32> {
        if self.opened {
            return Err(-16);
        }
        if !self.netdev.registered {
            return Err(-22);
        }
        litex_write8(&self.base, LITEETH_WRITER_EV_PENDING, 1);
        litex_write8(&self.base, LITEETH_READER_EV_PENDING, 1);
        Ok(())
    }

    // IRQ 绑定成功后，open 才能启用设备事件和队列。
    pub fn enable(&mut self) {
        litex_write8(&self.base, LITEETH_WRITER_EV_ENABLE, 1);
        litex_write8(&self.base, LITEETH_READER_EV_ENABLE, 1);
        self.opened = true;
        netif_carrier_on(&mut self.netdev);
        netif_start_queue(&mut self.netdev);
    }

    // stop 的设备侧步骤；之后才允许 free_irq。
    pub fn stop_device_events(&mut self) {
        netif_stop_queue(&mut self.netdev);
        netif_carrier_off(&mut self.netdev);
        litex_write8(&self.base, LITEETH_WRITER_EV_ENABLE, 0);
        litex_write8(&self.base, LITEETH_READER_EV_ENABLE, 0);
        self.opened = false;
    }

    // 输出空间不足时保留帧，让调用者重试。
    pub fn receive(&mut self, out: &mut [u8]) -> Result<usize, i32> {
        self.receive_queue.receive(out)
    }

    // 测试使用；调用前必须先 stop。
    #[cfg(test)]
    pub fn unregister(&mut self) -> Result<(), i32> {
        unregister_netdev(&mut self.netdev)
    }
}
