// SPDX-License-Identifier: GPL-2.0-or-later
//! Linux LiteEth 逐函数翻译：入口、参数名、返回协议和设备分支保持原顺序。
//! 所有入口由网络/platform 框架调用；unsafe 前提是 pdev/netdev/skb 指针有效且资源仍存活。
//! EL0 设备访问经 linux_api，EL1 只负责映射授权、IRQ 路由和通知。
#![allow(dead_code, non_camel_case_types, non_upper_case_globals)]
use crate::linux_api::{
    error::*, interrupt::*, io::*, module::*, netdevice::*, platform::*, printk::*, skbuff::*,
};
use crate::{
    dev_dbg, dev_err, devm_netdev_alloc_pcpu_stats, module_platform_driver, netdev_err,
    netdev_info, MODULE_AUTHOR, MODULE_DESCRIPTION, MODULE_DEVICE_TABLE, MODULE_LICENSE,
};
use core::{ffi::c_void, ptr};
pub const LITEETH_WRITER_SLOT: usize = 0x00;
pub const LITEETH_WRITER_LENGTH: usize = 0x04;
pub const LITEETH_WRITER_ERRORS: usize = 0x08;
pub const LITEETH_WRITER_EV_STATUS: usize = 0x0C;
pub const LITEETH_WRITER_EV_PENDING: usize = 0x10;
pub const LITEETH_WRITER_EV_ENABLE: usize = 0x14;
pub const LITEETH_READER_START: usize = 0x18;
pub const LITEETH_READER_READY: usize = 0x1C;
pub const LITEETH_READER_LEVEL: usize = 0x20;
pub const LITEETH_READER_SLOT: usize = 0x24;
pub const LITEETH_READER_LENGTH: usize = 0x28;
pub const LITEETH_READER_EV_STATUS: usize = 0x2C;
pub const LITEETH_READER_EV_PENDING: usize = 0x30;
pub const LITEETH_READER_EV_ENABLE: usize = 0x34;
pub const LITEETH_PREAMBLE_CRC: usize = 0x38;
pub const LITEETH_PREAMBLE_ERRORS: usize = 0x3C;
pub const LITEETH_CRC_ERRORS: usize = 0x40;
pub const LITEETH_PHY_CRG_RESET: usize = 0x00;
pub const LITEETH_MDIO_W: usize = 0x04;
pub const LITEETH_MDIO_R: usize = 0x0C;
pub const DRV_NAME: &str = "liteeth";

// 与原 C 的字段、名称和用途一致；地址均为已映射 EL0 VA。
pub struct liteeth {
    pub base: *mut c_void,       // CSR MMIO 基址
    pub netdev: *mut net_device, // 所属网卡
    pub dev: *mut device,        // 父设备，拥有 devm 资源
    pub slot_size: u32,          // 单槽字节数
    pub tx_slot: u32,            // 下一发送槽
    pub num_tx_slots: u32,       // TX 槽数
    pub tx_base: *mut c_void,    // TX SRAM 基址
    pub rx_slot: u32,            // 初始 RX 槽号
    pub num_rx_slots: u32,       // RX 槽数
    pub rx_base: *mut c_void,    // RX SRAM 基址
}

// 接收一帧并交给网络栈。
pub unsafe fn liteeth_rx(netdev: *mut net_device) -> i32 {
    let r#priv = netdev_priv(netdev).cast::<liteeth>();
    // 读取当前接收槽号和数据长度。
    let rx_slot = litex_read8((*r#priv).base.byte_add(LITEETH_WRITER_SLOT));
    let len = litex_read32((*r#priv).base.byte_add(LITEETH_WRITER_LENGTH)) as i32;
    'rx_drop: {
        // 长度为 0 或超过 2048 字节时丢弃。
        if len == 0 || len > 2048 {
            break 'rx_drop;
        }
        // 分配 skb；失败时记录日志并走统一丢弃路径。
        let skb = netdev_alloc_skb_ip_align(netdev, len as u32);
        if skb.is_null() {
            netdev_err!(netdev, "couldn't get memory\n");
            break 'rx_drop;
        }
        // 扩展 skb 的数据区，再把 RX slot 的数据复制到 skb 缓冲区。
        let data = skb_put(skb, len as u32);
        memcpy_fromio(
            data,
            (*r#priv)
                .rx_base
                .byte_add(rx_slot as usize * (*r#priv).slot_size as usize),
            len as usize,
        );
        // 解析以太网头并更新接收统计，再把 skb 交给本地接收队列。
        (*skb).protocol = eth_type_trans(skb, netdev);
        dev_sw_netstats_rx_add(netdev, len as u32);
        // 上交后队列取得 skb 所有权；当前目标尚未实现完整 Linux IP/socket。
        return netif_rx(skb);
    }
    (*netdev).stats.rx_dropped += 1;
    (*netdev).stats.rx_errors += 1;
    NET_RX_DROP
}

// 处理中断并清除收发事件。
pub unsafe fn liteeth_interrupt(irq: i32, dev_id: *mut c_void) -> irqreturn_t {
    let netdev = dev_id.cast::<net_device>();
    let r#priv = netdev_priv(netdev).cast::<liteeth>();
    // READER 是发送完成事件；缓冲区空出后唤醒发送队列，再清事件。
    let reg = litex_read8((*r#priv).base.byte_add(LITEETH_READER_EV_PENDING));
    if reg != 0 {
        if netif_queue_stopped(netdev) {
            netif_wake_queue(netdev);
        }
        litex_write8((*r#priv).base.byte_add(LITEETH_READER_EV_PENDING), reg);
    }
    // WRITER 是接收事件；有包时取出数据，再清事件。
    let reg = litex_read8((*r#priv).base.byte_add(LITEETH_WRITER_EV_PENDING));
    if reg != 0 {
        liteeth_rx(netdev);
        litex_write8((*r#priv).base.byte_add(LITEETH_WRITER_EV_PENDING), reg);
    }
    let _ = irq; // 原驱动不使用 irq 参数，仍保留定义。
    IRQ_HANDLED
}

// 打开网卡并启用中断和发送队列。
pub unsafe fn liteeth_open(netdev: *mut net_device) -> i32 {
    let r#priv = netdev_priv(netdev).cast::<liteeth>();
    // 清除旧的收发事件。
    litex_write8((*r#priv).base.byte_add(LITEETH_WRITER_EV_PENDING), 1);
    litex_write8((*r#priv).base.byte_add(LITEETH_READER_EV_PENDING), 1);
    // 注册 IRQ 和中断处理函数；失败时返回错误，不打开设备侧中断。
    let err = request_irq(
        (*netdev).irq,
        liteeth_interrupt,
        0,
        (*netdev).name.as_ptr(),
        netdev.cast(),
    );
    if err != 0 {
        netdev_err!(netdev, "failed to request irq {}\n", (*netdev).irq);
        return err;
    }
    // 打开设备侧收发中断，再标记链路可用并开放发送队列。
    litex_write8((*r#priv).base.byte_add(LITEETH_WRITER_EV_ENABLE), 1);
    litex_write8((*r#priv).base.byte_add(LITEETH_READER_EV_ENABLE), 1);
    netif_carrier_on(netdev);
    netif_start_queue(netdev);
    0
}

// 停止网卡并释放 IRQ。
pub unsafe fn liteeth_stop(netdev: *mut net_device) -> i32 {
    let r#priv = netdev_priv(netdev).cast::<liteeth>();
    // 停止发送队列、标记链路不可用，再关闭设备中断并释放 IRQ。
    netif_stop_queue(netdev);
    netif_carrier_off(netdev);
    litex_write8((*r#priv).base.byte_add(LITEETH_WRITER_EV_ENABLE), 0);
    litex_write8((*r#priv).base.byte_add(LITEETH_READER_EV_ENABLE), 0);
    free_irq((*netdev).irq, netdev.cast());
    0
}

// 将一帧写入发送缓冲区并启动发送。
pub unsafe fn liteeth_start_xmit(skb: *mut sk_buff, netdev: *mut net_device) -> netdev_tx_t {
    let r#priv = netdev_priv(netdev).cast::<liteeth>();
    // 读取发送就绪寄存器；未就绪时停队列并返回 BUSY，保留 skb 供重试。
    if litex_read8((*r#priv).base.byte_add(LITEETH_READER_READY)) == 0 {
        if net_ratelimit() != 0 {
            netdev_err!(netdev, "LITEETH_READER_READY not ready\n");
        }
        netif_stop_queue(netdev);
        return NETDEV_TX_BUSY;
    }
    // 数据包超过 TX slot 容量时丢弃并记录错误。
    if unlikely((*skb).len > (*r#priv).slot_size) {
        if net_ratelimit() != 0 {
            netdev_err!(netdev, "tx packet too big\n");
        }
        dev_kfree_skb_any(skb);
        (*netdev).stats.tx_dropped += 1;
        (*netdev).stats.tx_errors += 1;
        return NETDEV_TX_OK;
    }
    // 先把数据复制到 TX slot，再写槽号、长度并启动发送。
    let txbuffer = (*r#priv)
        .tx_base
        .byte_add((*r#priv).tx_slot as usize * (*r#priv).slot_size as usize);
    memcpy_toio(txbuffer, (*skb).data, (*skb).len as usize);
    litex_write8(
        (*r#priv).base.byte_add(LITEETH_READER_SLOT),
        (*r#priv).tx_slot as u8,
    );
    litex_write16(
        (*r#priv).base.byte_add(LITEETH_READER_LENGTH),
        (*skb).len as u16,
    );
    litex_write8((*r#priv).base.byte_add(LITEETH_READER_START), 1);
    dev_sw_netstats_tx_add(netdev, 1, (*skb).len);
    (*r#priv).tx_slot = ((*r#priv).tx_slot + 1) % (*r#priv).num_tx_slots;
    dev_kfree_skb_any(skb);
    NETDEV_TX_OK
}

// 汇总网卡统计。
pub unsafe fn liteeth_get_stats64(netdev: *mut net_device, stats: *mut rtnl_link_stats64) {
    netdev_stats_to_stats64(stats, ptr::addr_of!((*netdev).stats));
    dev_fetch_sw_netstats(stats, (*netdev).tstats);
}
static liteeth_netdev_ops: net_device_ops = net_device_ops {
    ndo_open: liteeth_open,
    ndo_stop: liteeth_stop,
    ndo_get_stats64: liteeth_get_stats64,
    ndo_start_xmit: liteeth_start_xmit,
};

// 读取设备树中的槽位数量和大小，失败时使用默认值。
pub unsafe fn liteeth_setup_slots(r#priv: *mut liteeth) {
    let np = (*(*r#priv).dev).of_node;
    let err = of_property_read_u32(
        np,
        c"litex,rx-slots".as_ptr(),
        ptr::addr_of_mut!((*r#priv).num_rx_slots),
    );
    if err != 0 {
        dev_dbg!((*r#priv).dev, "unable to get litex,rx-slots, using 2\n");
        (*r#priv).num_rx_slots = 2;
    }
    let err = of_property_read_u32(
        np,
        c"litex,tx-slots".as_ptr(),
        ptr::addr_of_mut!((*r#priv).num_tx_slots),
    );
    if err != 0 {
        dev_dbg!((*r#priv).dev, "unable to get litex,tx-slots, using 2\n");
        (*r#priv).num_tx_slots = 2;
    }
    let err = of_property_read_u32(
        np,
        c"litex,slot-size".as_ptr(),
        ptr::addr_of_mut!((*r#priv).slot_size),
    );
    if err != 0 {
        dev_dbg!(
            (*r#priv).dev,
            "unable to get litex,slot-size, using 0x800\n"
        );
        (*r#priv).slot_size = 0x800;
    }
}

// 初始化并注册网卡。
pub unsafe fn liteeth_probe(pdev: *mut platform_device) -> i32 {
    // 分配网卡对象，并预留 LiteEth 私有数据空间。
    let netdev = devm_alloc_etherdev(
        ptr::addr_of_mut!((*pdev).dev),
        core::mem::size_of::<liteeth>() as i32,
    );
    if netdev.is_null() {
        return -ENOMEM;
    }
    // 关联父设备，并保存 netdev 供移除时取回。
    SET_NETDEV_DEV(netdev, ptr::addr_of_mut!((*pdev).dev));
    platform_set_drvdata(pdev, netdev.cast());
    // 取得私有数据空间，保存所属网卡和父设备指针。
    let r#priv = netdev_priv(netdev).cast::<liteeth>();
    (*r#priv).netdev = netdev;
    (*r#priv).dev = ptr::addr_of_mut!((*pdev).dev);
    // 为每个 CPU 分配软件网络统计数据。
    (*netdev).tstats =
        devm_netdev_alloc_pcpu_stats!(ptr::addr_of_mut!((*pdev).dev), pcpu_sw_netstats);
    if (*netdev).tstats.is_null() {
        return -ENOMEM;
    }
    // 读取 IRQ 号
    let irq = platform_get_irq(pdev, 0);
    if irq < 0 {
        return irq;
    }
    (*netdev).irq = irq as u32;
    // 分别MMIO映射控制器寄存器区和 RX/TX 共用缓冲区
    (*r#priv).base = devm_platform_ioremap_resource_byname(pdev, c"mac".as_ptr());
    if IS_ERR((*r#priv).base) {
        return PTR_ERR((*r#priv).base) as i32;
    }
    let buf_base = devm_platform_ioremap_resource_byname(pdev, c"buffer".as_ptr());
    if IS_ERR(buf_base) {
        return PTR_ERR(buf_base) as i32;
    }
    liteeth_setup_slots(r#priv);
    // RX 从 buffer 起始处开始，TX 槽紧随全部 RX 槽。
    (*r#priv).rx_base = buf_base;
    (*r#priv).rx_slot = 0;
    (*r#priv).tx_base =
        buf_base.byte_add((*r#priv).num_rx_slots as usize * (*r#priv).slot_size as usize);
    (*r#priv).tx_slot = 0;
    // 读取设备树中的 MAC 地址；没有有效地址时生成随机地址。
    let err = of_get_ethdev_address((*pdev).dev.of_node, netdev);
    if err != 0 {
        eth_hw_addr_random(netdev);
    }
    // 设置网络操作入口并注册网卡。
    (*netdev).netdev_ops = ptr::addr_of!(liteeth_netdev_ops);
    let err = register_netdev(netdev);
    if err != 0 {
        dev_err!(
            ptr::addr_of!((*pdev).dev),
            "Failed to register netdev {}\n",
            err
        );
        return err;
    }
    netdev_info!(
        netdev,
        "irq {} slots: tx {} rx {} size {}\n",
        (*netdev).irq,
        (*r#priv).num_tx_slots,
        (*r#priv).num_rx_slots,
        (*r#priv).slot_size
    );
    0
}

// 注销网卡；platform 框架在本函数返回后回收 devm 资源。
pub unsafe fn liteeth_remove(pdev: *mut platform_device) {
    let netdev = platform_get_drvdata(pdev).cast::<net_device>();
    unregister_netdev(netdev);
}
static liteeth_of_match: &[of_device_id] = &[
    of_device_id {
        compatible: "litex,liteeth",
    },
    of_device_id { compatible: "" },
];
MODULE_DEVICE_TABLE!(of, liteeth_of_match);
static liteeth_driver: platform_driver = platform_driver {
    probe: liteeth_probe,
    remove_new: liteeth_remove,
    driver: device_driver {
        name: DRV_NAME,
        of_match_table: liteeth_of_match,
    },
};
module_platform_driver!(liteeth_driver);
MODULE_AUTHOR!("Joel Stanley <joel@jms.id.au>");
MODULE_DESCRIPTION!("LiteX Liteeth Ethernet driver");
MODULE_LICENSE!("GPL");

#[cfg(feature = "linux-driver-api")]
pub use super::liteeth_runtime::Driver;
