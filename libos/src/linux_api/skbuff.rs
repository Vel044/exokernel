// SPDX-License-Identifier: GPL-2.0-or-later
//! 有界线性 skb 与有界接收交付队列。容量由调用者指定，不依赖任何 LiteEth 寄存器。
//! 报文按值转移所有权；不支持分片 skb、GSO 或完整 Linux 协议栈。
use super::{
    error::{EBUSY, EINVAL},
    netdevice::NetDevice,
};
#[repr(C, align(4))]
struct Storage<const N: usize> {
    /// Linux NET_IP_ALIGN 的两个头部填充字节，使 Ethernet 后的 IP 头 4 字节对齐。
    _headroom: [u8; 2],
    /// 内联拥有的线性帧，容量在实例类型中明确，不做每包堆分配。
    bytes: [u8; N],
}
pub struct SkBuff<const N: usize> {
    /// 存储所有权、完整帧长度和协议解析后的 data 偏移。
    storage: Storage<N>,
    length: usize,
    capacity: usize,
    pulled: usize,
    /// 以太网协议号以主机整数表示；不承诺 C __be16 ABI。
    pub protocol: u16,
}
impl<const N: usize> SkBuff<N> {
    pub fn frame(&self) -> &[u8] {
        &self.storage.bytes[..self.length]
    }
    pub fn data(&self) -> &[u8] {
        &self.storage.bytes[self.pulled..self.length]
    }
}
pub fn netdev_alloc_skb_ip_align<P, const N: usize>(
    _: &NetDevice<P>,
    size: usize,
) -> Result<SkBuff<N>, i32> {
    if size > N {
        return Err(EINVAL);
    }
    Ok(SkBuff {
        storage: Storage {
            _headroom: [0; 2],
            bytes: [0; N],
        },
        length: 0,
        capacity: size,
        pulled: 0,
        protocol: 0,
    })
}
pub fn skb_put<const N: usize>(skb: &mut SkBuff<N>, length: usize) -> Result<&mut [u8], i32> {
    let start = skb.length;
    let end = start
        .checked_add(length)
        .filter(|end| *end <= skb.capacity)
        .ok_or(EINVAL)?;
    skb.length = end;
    Ok(&mut skb.storage.bytes[start..end])
}
/// 复用 Linux Ethernet II/802.2/原始 802.3 的协议判断，不支持 DSA 标签变体。
pub fn eth_type_trans<P, const N: usize>(
    skb: &mut SkBuff<N>,
    _: &NetDevice<P>,
) -> Result<u16, i32> {
    if skb.length < 14 || skb.pulled != 0 {
        return Err(EINVAL);
    }
    let protocol = u16::from_be_bytes([skb.storage.bytes[12], skb.storage.bytes[13]]);
    skb.pulled = 14;
    skb.protocol = if protocol >= 0x600 {
        protocol
    } else if skb.data().starts_with(&[0xff, 0xff]) {
        0x0001
    } else {
        0x0004
    };
    Ok(skb.protocol)
}
pub fn dev_kfree_skb_any<const N: usize>(skb: SkBuff<N>) {
    drop(skb);
}
/// 有界 FIFO，满时丢弃新包；实例之间没有全局队列或硬编码设备名。
pub struct RxQueue<const N: usize, const Q: usize = 1> {
    /// 每个槽拥有 skb；head/count 只由所属线程修改。
    slots: [Option<SkBuff<N>>; Q],
    head: usize,
    count: usize,
}
impl<const N: usize, const Q: usize> RxQueue<N, Q> {
    pub fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| None),
            head: 0,
            count: 0,
        }
    }
    pub fn is_full(&self) -> bool {
        self.count == Q
    }
    /// 缓冲太小则保留队首包，调用者可重试；交付完整 L2 帧而非仅 skb.data。
    pub fn receive(&mut self, out: &mut [u8]) -> Result<usize, i32> {
        if self.count == 0 {
            return Ok(0);
        }
        let frame = self.slots[self.head].as_ref().unwrap().frame();
        if out.len() < frame.len() {
            return Err(EINVAL);
        }
        let n = frame.len();
        out[..n].copy_from_slice(frame);
        dev_kfree_skb_any(self.slots[self.head].take().unwrap());
        self.head = (self.head + 1) % Q;
        self.count -= 1;
        Ok(n)
    }
}
/// 消费 skb，即使满队列也不把所有权交还驱动；只提供 L2 交付，不进入 IP/socket。
pub fn netif_rx<const N: usize, const Q: usize>(
    queue: &mut RxQueue<N, Q>,
    skb: SkBuff<N>,
) -> Result<(), i32> {
    if queue.count == Q {
        dev_kfree_skb_any(skb);
        return Err(EBUSY);
    }
    queue.slots[(queue.head + queue.count) % Q] = Some(skb);
    queue.count += 1;
    Ok(())
}
