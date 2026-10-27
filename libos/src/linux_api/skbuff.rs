// SPDX-License-Identifier: GPL-2.0-or-later
//! 堆分配的线性 skb；NULL、尾指针、协议头和释放责任遵循 Linux 调用协议。
use super::{memory, netdevice::net_device};
use core::ptr;
pub const NET_RX_SUCCESS: i32 = 0;
pub const NET_RX_DROP: i32 = 1;
pub struct sk_buff {
    pub head: *mut u8,
    pub data: *mut u8,
    pub tail: usize,
    pub end: usize,
    pub len: u32,
    pub protocol: u16,
    pub dev: *mut net_device,
    pub mac_header: usize,
    pub pkt_type: u8,
    pub(crate) allocation_size: usize,
}
pub unsafe fn netdev_alloc_skb_ip_align(dev: *mut net_device, length: u32) -> *mut sk_buff {
    let size = match (length as usize).checked_add(2) {
        Some(s) => s,
        None => return ptr::null_mut(),
    };
    let head = memory::allocate(size, 4);
    if head.is_null() {
        return ptr::null_mut();
    }
    let skb = memory::value(sk_buff {
        head,
        data: head.add(2),
        tail: 2,
        end: size,
        len: 0,
        protocol: 0,
        dev,
        mac_header: 2,
        pkt_type: 0,
        allocation_size: size,
    });
    if skb.is_null() {
        memory::release(head, size, 4);
    }
    skb
}
pub unsafe fn skb_put(skb: *mut sk_buff, len: u32) -> *mut u8 {
    let tmp = (*skb).head.add((*skb).tail);
    let end = (*skb)
        .tail
        .checked_add(len as usize)
        .expect("skb尾指针溢出");
    (*skb).tail = end;
    (*skb).len = (*skb).len.checked_add(len).expect("skb长度溢出");
    if (*skb).tail > (*skb).end {
        panic!("skb_over_panic");
    }
    tmp
}
pub unsafe fn eth_type_trans(skb: *mut sk_buff, dev: *mut net_device) -> u16 {
    (*skb).dev = dev;
    (*skb).mac_header = (*skb).data.offset_from((*skb).head) as usize;
    assert!((*skb).len >= 14, "以太网头不足");
    let eth = core::slice::from_raw_parts((*skb).data, 14);
    (*skb).pkt_type = if eth[..6] == [255; 6] {
        1
    } else if eth[0] & 1 != 0 {
        2
    } else if eth[..6] != (*dev).dev_addr {
        3
    } else {
        0
    };
    let proto = u16::from_be_bytes([eth[12], eth[13]]);
    (*skb).data = (*skb).data.add(14);
    (*skb).len -= 14;
    if (*dev).dsa_without_ethertype {
        return 0x00f8u16.to_be();
    }
    if proto >= 0x600 {
        return proto.to_be();
    }
    if (*skb).len >= 2 && *(*skb).data == 255 && *(*skb).data.add(1) == 255 {
        1u16.to_be()
    } else {
        4u16.to_be()
    }
}
pub unsafe fn netif_rx(skb: *mut sk_buff) -> i32 {
    let dev = (*skb).dev;
    // 接收核心取得所有权；排队失败也消费 skb，不归还给驱动。
    if dev.is_null() || !(*dev).registered || (*dev).rx_queue.try_reserve(1).is_err() {
        dev_kfree_skb_any(skb);
        return NET_RX_DROP;
    }
    (*dev).rx_queue.push_back(skb);
    NET_RX_SUCCESS
}
pub unsafe fn dev_kfree_skb_any(skb: *mut sk_buff) {
    if !skb.is_null() {
        memory::release((*skb).head, (*skb).allocation_size, 4);
        memory::free_value(skb);
    }
}
pub unsafe fn frame(skb: *const sk_buff) -> &'static [u8] {
    core::slice::from_raw_parts(
        (*skb).head.add((*skb).mac_header),
        (*skb).tail - (*skb).mac_header,
    )
}
pub unsafe fn skb_from_bytes(dev: *mut net_device, bytes: &[u8]) -> *mut sk_buff {
    let skb = netdev_alloc_skb_ip_align(dev, bytes.len() as u32);
    if !skb.is_null() {
        ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            skb_put(skb, bytes.len() as u32),
            bytes.len(),
        );
    }
    skb
}
