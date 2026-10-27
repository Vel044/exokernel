#![allow(dead_code)]
//! 宿主硬件模型：保存真实 VA、模拟 CSR W1C/READY；不替代 QEMU 的实际 SVC 验证。
use crate::{
    driver,
    linux_api::{self, kernel::KernelOps, memory, netdevice::*, platform::*, skbuff::*},
};
use std::{cell::RefCell, ffi::c_void, ptr};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Read(usize),
    Write(usize, u32),
    From(usize, usize),
    To(usize, usize),
    Bind(u32),
    Wait,
    Callback,
    Ack(u32),
    Unbind(u32),
    Destroy,
    Unmap,
}
pub struct State {
    pub csr: Box<[u32; 1024]>,
    pub buffer: Box<[u8; 8192]>,
    pub ops: Vec<Op>,
    pub maps: usize,
    pub map_fail: Option<u64>,
    pub fail_bind: bool,
    pub bound: usize,
    pub logs: Vec<String>,
    pub cpu: usize,
    pub tick: u64,
}
thread_local! {pub static STATE:RefCell<State>=RefCell::new(State{csr:Box::new([0;1024]),buffer:Box::new([0;8192]),ops:vec![],maps:0,map_fail:None,fail_bind:false,bound:0,logs:vec![],cpu:0,tick:1});}
pub fn reset() {
    assert_eq!(memory::live(), 0, "上一个场景泄漏分配");
    assert_eq!(linux_api::interrupt::bindings(), 0);
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.csr.fill(0);
        s.csr[0x1c / 4] = 1;
        s.buffer.fill(0);
        s.ops.clear();
        s.maps = 0;
        s.map_fail = None;
        s.fail_bind = false;
        s.bound = 0;
        s.logs.clear();
        s.cpu = 0;
        s.tick = 1;
    });
    memory::fail_after(usize::MAX);
    linux_api::random::seed(&[0; 32]);
    unsafe {
        linux_api::kernel::install(&OPS);
    }
}
unsafe fn map(pa: u64, _size: u64, _va: u64) -> Result<*mut c_void, i32> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.map_fail == Some(pa) {
            return Err(-5);
        }
        let p = if pa == 0x1000 || pa == 0x5000 {
            s.csr.as_mut_ptr().cast()
        } else if pa == 0x2000 || pa == 0x6000 {
            s.buffer.as_mut_ptr().cast()
        } else {
            return Err(-5);
        };
        s.maps += 1;
        Ok(p)
    })
}
unsafe fn unmap(_p: *mut c_void, _size: u64) -> Result<(), i32> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.maps -= 1;
        s.ops.push(Op::Unmap);
    });
    Ok(())
}
fn bind(irq: u32) -> Result<u64, i32> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.fail_bind {
            return Err(-5);
        }
        s.bound += 1;
        s.ops.push(Op::Bind(irq));
        Ok(1)
    })
}
fn wait(_h: u64) -> Result<u64, i32> {
    STATE.with(|s| s.borrow_mut().ops.push(Op::Wait));
    Ok(1)
}
fn ack(irq: u32) -> Result<(), i32> {
    STATE.with(|s| s.borrow_mut().ops.push(Op::Ack(irq)));
    Ok(())
}
fn unbind(irq: u32) -> Result<(), i32> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.bound -= 1;
        s.ops.push(Op::Unbind(irq));
    });
    Ok(())
}
fn destroy(_h: u64) -> Result<(), i32> {
    STATE.with(|s| s.borrow_mut().ops.push(Op::Destroy));
    Ok(())
}
unsafe fn read32(p: *const c_void) -> u32 {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let off = (p as usize).checked_sub(s.csr.as_ptr() as usize).unwrap();
        assert!(off + 4 <= 4096 && off % 4 == 0);
        s.ops.push(Op::Read(off));
        s.csr[off / 4]
    })
}
unsafe fn write32(p: *mut c_void, v: u32) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let off = (p as usize).checked_sub(s.csr.as_ptr() as usize).unwrap();
        assert!(off + 4 <= 4096 && off % 4 == 0);
        s.ops.push(Op::Write(off, v));
        if off == 0x10 || off == 0x30 {
            s.csr[off / 4] &= !v;
        } else {
            s.csr[off / 4] = v;
        }
        if off == 0x18 {
            s.csr[0x1c / 4] = 0;
        }
    })
}
unsafe fn fromio(to: *mut u8, from: *const c_void, len: usize) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let off = (from as usize)
            .checked_sub(s.buffer.as_ptr() as usize)
            .unwrap();
        assert!(off + len <= 8192);
        s.ops.push(Op::From(off, len));
        ptr::copy_nonoverlapping(s.buffer.as_ptr().add(off), to, len);
    });
}
unsafe fn toio(to: *mut c_void, from: *const u8, len: usize) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let off = (to as usize)
            .checked_sub(s.buffer.as_ptr() as usize)
            .unwrap();
        assert!(off + len <= 8192);
        s.ops.push(Op::To(off, len));
        ptr::copy_nonoverlapping(from, s.buffer.as_mut_ptr().add(off), len);
    });
}
static OPS: KernelOps = KernelOps {
    map,
    unmap,
    bind,
    wait,
    ack,
    unbind,
    destroy,
    cancel_wait: |_| Ok(()),
    yield_now: || {},
    start_worker: |_| Ok(0),
    wait_worker: |_| Ok(()),
    stop_worker: |_| {},
    read32,
    write32,
    fromio,
    toio,
    log: |m| STATE.with(|s| s.borrow_mut().logs.push(m.into())),
    ticks: || STATE.with(|s| s.borrow().tick),
    frequency: || 1000,
    cpu: || STATE.with(|s| s.borrow().cpu),
    cpus: || 4,
};
pub struct Fixture {
    pub node: Box<device_node>,
    pub pdev: Box<platform_device>,
    attached: bool,
}
impl Fixture {
    pub fn new(properties: Vec<(&'static str, u32)>, address: Option<[u8; 6]>) -> Self {
        let mut node = Box::new(device_node {
            properties,
            addresses: [address, None, None, None],
        });
        let pdev = Box::new(platform_device {
            dev: device::new("model", &mut *node),
            resources: vec![
                NamedResource {
                    name: "mac",
                    base: 0x1000,
                    size: 4096,
                    user_va: 0x400000,
                },
                NamedResource {
                    name: "buffer",
                    base: 0x2000,
                    size: 8192,
                    user_va: 0x500000,
                },
            ],
            irqs: vec![42],
        });
        Self {
            node,
            pdev,
            attached: false,
        }
    }
    pub fn attach(&mut self) -> i32 {
        let err = unsafe {
            linux_api::module::bind(
                driver::registered_platform_driver(),
                &mut *self.pdev,
                "litex,liteeth",
            )
        };
        self.attached = err == 0;
        err
    }
    pub fn dev(&self) -> *mut net_device {
        self.pdev.dev.drvdata.cast()
    }
    pub fn open(&mut self) {
        assert_eq!(unsafe { dev_open(self.dev()) }, 0);
    }
    pub fn stats(&self) -> rtnl_link_stats64 {
        let mut s = Default::default();
        unsafe { driver::liteeth_get_stats64(self.dev(), &mut s) }
        s
    }
    pub fn close(&mut self) {
        unsafe { dev_close(self.dev()) }
    }
    pub fn detach(&mut self) {
        if self.attached {
            unsafe {
                linux_api::module::detach(driver::registered_platform_driver(), &mut *self.pdev);
            }
            self.attached = false;
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.detach();
    }
}
pub fn fixture() -> Fixture {
    let mut f = Fixture::new(vec![], Some([2, 1, 2, 3, 4, 5]));
    assert_eq!(f.attach(), 0);
    f
}
pub fn packet(dev: *mut net_device, len: usize) -> *mut sk_buff {
    let mut bytes = vec![7; len];
    if len >= 14 {
        bytes[12] = 8;
        bytes[13] = 0;
    }
    let p = unsafe { skb_from_bytes(dev, &bytes) };
    assert!(!p.is_null());
    p
}
pub fn rx(slot: usize, len: usize) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.csr[0] = slot as u32;
        s.csr[1] = len as u32;
        s.csr[0x10 / 4] = 1;
        s.buffer[slot * 2048..slot * 2048 + len.min(2048)].fill(9);
        if len >= 14 && len <= 2048 {
            s.buffer[slot * 2048 + 12] = 8;
            s.buffer[slot * 2048 + 13] = 0;
        }
    });
}
pub fn tx_done() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.csr[0x1c / 4] = 1;
        s.csr[0x30 / 4] = 1;
    });
}
