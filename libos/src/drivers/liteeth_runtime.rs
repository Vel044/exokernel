// SPDX-License-Identifier: GPL-2.0-or-later
//! platform/网络框架的应用入口；与逐函数翻译的 liteeth.rs 分开。
//! Box 保持 pdev/DTB 节点稳定；remove 返回后框架逆序回收 devm 资源。
use super::{liteeth, platform::LiteEthInfo};
use crate::linux_api::{error::*, interrupt, module, netdevice::*, platform::*, skbuff::*};
use alloc::{boxed::Box, vec};
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};
static CLAIMED: AtomicBool = AtomicBool::new(false);
pub struct Driver {
    pdev: Box<platform_device>,
    _node: Box<device_node>,
    attached: bool,
    _owner: PhantomData<*mut ()>,
}
impl Driver {
    pub fn attach(info: LiteEthInfo) -> Result<Self, i32> {
        if CLAIMED
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(-EBUSY);
        }
        let mut properties = vec![];
        for (bit, name, value) in [
            (1, "litex,rx-slots", info.rx_slots),
            (2, "litex,tx-slots", info.tx_slots),
            (4, "litex,slot-size", info.slot_size),
        ] {
            if info.property_presence & bit != 0 {
                properties.push((name, value));
            }
        }
        let mut node = Box::new(device_node {
            properties,
            addresses: [Some(info.address), None, None, None],
        });
        let mut pdev = Box::new(platform_device {
            dev: device::new("liteeth", &mut *node),
            resources: vec![
                NamedResource {
                    name: "mac",
                    base: info.mac.base,
                    size: info.mac.size,
                    user_va: 0x40500000,
                },
                NamedResource {
                    name: "buffer",
                    base: info.buffer.base,
                    size: info.buffer.size,
                    user_va: 0x40600000,
                },
            ],
            irqs: vec![info.mac.intid],
        });
        // SAFETY: pdev/node 在 Box 中地址稳定，probe 失败时 bind 回收全部 devm 记录。
        let err = unsafe {
            module::bind(
                liteeth::registered_platform_driver(),
                &mut *pdev,
                "litex,liteeth",
            )
        };
        if err != 0 {
            CLAIMED.store(false, Ordering::Release);
            return Err(err);
        }
        Ok(Self {
            pdev,
            _node: node,
            attached: true,
            _owner: PhantomData,
        })
    }
    fn netdev(&mut self) -> *mut net_device {
        self.pdev.dev.drvdata.cast()
    }
    pub fn open(&mut self) -> Result<(), i32> {
        let dev = self.netdev();
        let err = unsafe {
            let _guard = lock_device(dev);
            dev_open(dev)
        };
        if err == 0 {
            Ok(())
        } else {
            Err(err)
        }
    }
    pub fn stop(&mut self) -> Result<(), i32> {
        unsafe {
            let dev = self.netdev();
            let _guard = lock_device(dev);
            dev_close(dev);
        }
        Ok(())
    }
    pub fn send(&mut self, frame: &[u8]) -> Result<bool, i32> {
        let dev = self.netdev();
        // 网络框架拒绝关闭状态的发送；原 ndo_start_xmit 不添加这条分支。
        if !unsafe { (*dev).running } {
            return Err(-ENODEV);
        }
        let skb = unsafe { skb_from_bytes(dev, frame) };
        if skb.is_null() {
            return Err(-ENOMEM);
        }
        // 网络核心持有 skb：BUSY 时保留原对象，IRQ 恢复队列后自动重试。
        let ret = unsafe {
            let _guard = lock_device(dev);
            enqueue_tx(dev, skb)
        };
        if ret != 0 {
            return Err(ret);
        }
        loop {
            let queued = unsafe {
                let _guard = lock_device(dev);
                (*dev).tx_queue.contains(&skb)
            };
            if !queued {
                return Ok(true);
            }
            self.wait_irq()?;
        }
    }

    pub fn wait_irq(&mut self) -> Result<(), i32> {
        let dev = self.netdev();
        unsafe {
            interrupt::wait_completion((*dev).irq)?;
        }
        Ok(())
    }
    pub fn receive(&mut self, out: &mut [u8]) -> usize {
        unsafe {
            let dev = self.netdev();
            let _guard = lock_device(dev);
            let Some(skb) = (*dev).rx_queue.pop_front() else {
                return 0;
            };
            let frame = crate::linux_api::skbuff::frame(skb);
            assert!(frame.len() <= out.len());
            out[..frame.len()].copy_from_slice(frame);
            let len = frame.len();
            dev_kfree_skb_any(skb);
            len
        }
    }
    pub fn stats(&mut self) -> Result<rtnl_link_stats64, i32> {
        let mut stats = Default::default();
        unsafe {
            let dev = self.netdev();
            let _guard = lock_device(dev);
            ((*(*dev).netdev_ops).ndo_get_stats64)(dev, &mut stats);
        }
        Ok(stats)
    }
    pub fn remove(mut self) -> Result<(), i32> {
        self.detach();
        Ok(())
    }
    fn detach(&mut self) {
        if self.attached {
            unsafe {
                module::detach(liteeth::registered_platform_driver(), &mut *self.pdev);
            }
            self.attached = false;
            CLAIMED.store(false, Ordering::Release);
        }
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        self.detach();
    }
}
