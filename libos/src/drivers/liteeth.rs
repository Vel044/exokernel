// SPDX-License-Identifier: GPL-2.0-or-later
//! AI 辅助定向翻译的原生 Rust LiteEth 驱动：直接使用已有 kernel_api/runtime。
//! 不链接 Linux C 驱动、compat.h、skb、net_device 或 C/Rust FFI。
//! liteeth_core 保留设备协议；本层独占 MMIO 映射、Notification 与 IRQ 生命周期。
use super::{
    liteeth_core::{Device, Io, Layout},
    platform::{DeviceResource, LiteEthInfo},
};
use crate::{notification::Notification, runtime};
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};
const MAC_VA: u64 = 0x4050_0000;
const BUFFER_VA: u64 = 0x4060_0000;
static CLAIMED: AtomicBool = AtomicBool::new(false);
/// 固定 VA 只允许一个所属实例；Acquire/Release 保证前一实例回收先于再次映射。
struct Claim;
impl Claim {
    fn take() -> Result<Self, i32> {
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
fn fatal() -> ! {
    runtime::puts(b"[liteeth] FAIL native resource cleanup\r\n");
    runtime::exit(0x703)
}
/// 一个成功建立的 Device-nGnRE 映射；失败构造不取得所有权，Drop 精确撤销。
struct Mapping {
    /// 固定 EL0 CPU VA，不能作为设备 DMA address 使用。
    va: u64,
    /// EL1 记录的映射字节数，撤销时必须完全相等。
    size: u64,
}
impl Mapping {
    fn map(resource: DeviceResource, va: u64, max: u64) -> Result<Self, i32> {
        if resource.base & 4095 != 0
            || resource.size == 0
            || resource.size & 4095 != 0
            || resource.size > max
        {
            return Err(-22);
        }
        // EL0 传 PA/长度/VA；SVC 后 EL1 核对 grant、对齐和冲突，成功建立 Device PTE。
        runtime::map_mmio(resource.base, resource.size, va).map_err(|_| -5)?;
        Ok(Self {
            va,
            size: resource.size,
        })
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        if runtime::unmap_mmio(self.va, self.size).is_err() {
            fatal();
        }
    }
}
/// 通知对象不能早于 IRQ 解绑回收；Driver::close 负责先撤销路由。
struct OwnedNotification(Notification);
impl Drop for OwnedNotification {
    fn drop(&mut self) {
        if self.0.destroy().is_err() {
            fatal();
        }
    }
}
/// 两段映射的拥有者；字段顺序不影响解绑，因为 Driver::drop 总是先 close。
struct Mmio {
    /// CSR VA：32 位总线事务，低 8/16 位存放逻辑值。
    registers: Mapping,
    /// 设备 SRAM VA：CPU volatile 复制，无 DMA 分配及缓存一致性操作。
    buffer: Mapping,
}
fn barrier() {
    // SAFETY: AArch64 EL0 允许 DMB；osh 对设备可见顺序排序，编译器 memory clobber 禁止重排。
    unsafe {
        core::arch::asm!("dmb osh", options(nostack, preserves_flags));
    }
}
impl Io for Mmio {
    fn read(&self, offset: usize) -> u32 {
        assert!(offset % 4 == 0 && offset as u64 + 4 <= self.registers.size);
        // SAFETY: 映射存活，CSR 偏移已按 32 位对齐并检查范围；volatile 读取真实设备状态。
        let value = unsafe {
            core::ptr::read_volatile((self.registers.va as usize + offset) as *const u32)
        };
        barrier();
        u32::from_le(value)
    }
    fn write(&self, offset: usize, value: u32) {
        assert!(offset % 4 == 0 && offset as u64 + 4 <= self.registers.size);
        barrier();
        // SAFETY: 与 read 相同的映射和对齐前提；事务最终设置 CSR 或 W1C 清 pending。
        unsafe {
            core::ptr::write_volatile(
                (self.registers.va as usize + offset) as *mut u32,
                value.to_le(),
            );
        }
        barrier();
    }
    fn copy_from(&self, offset: usize, out: &mut [u8]) {
        assert!(offset
            .checked_add(out.len())
            .is_some_and(|end| end <= self.buffer.size as usize));
        // SAFETY: 源为存活的设备 SRAM 映射，目标由独占切片持有；逐字节 volatile 不越界。
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = unsafe {
                core::ptr::read_volatile((self.buffer.va as usize + offset + i) as *const u8)
            };
        }
        barrier();
    }
    fn copy_to(&self, offset: usize, bytes: &[u8]) {
        assert!(offset
            .checked_add(bytes.len())
            .is_some_and(|end| end <= self.buffer.size as usize));
        // SAFETY: 调用者保留报文所有权；目的为已经核查的 TX slot VA，后续 barrier 先于 START。
        for (i, byte) in bytes.iter().enumerate() {
            unsafe {
                core::ptr::write_volatile((self.buffer.va as usize + offset + i) as *mut u8, *byte);
            }
        }
        barrier();
    }
}
/// 单线程独占设备，裸指针标记禁止 Send/Sync；没有 Linux 全局对象或异步 C 回调。
pub struct Driver {
    /// 设备状态机和 MMIO 映射，解绑后才随对象释放。
    device: Device<Mmio>,
    /// 接收 EL1 投递的 badge；没有 pending 时 WAIT 阻塞当前线程。
    notification: OwnedNotification,
    /// DTB 给出的 GIC INTID，绑定仍由 EL1 grant 审核。
    intid: u32,
    /// 绑定成功后才为 true，失败解绑时保留 true，防止继续回收。
    bound: bool,
    /// 最后释放固定 VA 所有权，失败回滚由字段 Drop 自动完成。
    _claim: Claim,
    _owner: PhantomData<*mut ()>,
}
impl Driver {
    pub fn attach(info: LiteEthInfo) -> Result<Self, i32> {
        let layout = Layout {
            rx_slots: info.rx_slots as usize,
            tx_slots: info.tx_slots as usize,
            slot_size: info.slot_size as usize,
        };
        layout.validate(usize::try_from(info.buffer.size).map_err(|_| -22)?)?;
        if info.mac.size < 0x44 || info.mac.intid < 32 {
            return Err(-22);
        }
        let claim = Claim::take()?;
        let notification = OwnedNotification(Notification::create().map_err(|_| -5)?);
        let io = Mmio {
            registers: Mapping::map(info.mac, MAC_VA, 4096)?,
            buffer: Mapping::map(info.buffer, BUFFER_VA, 65536)?,
        };
        let device = Device::new(io, layout, info.buffer.size as usize)?;
        Ok(Self {
            device,
            notification,
            intid: info.mac.intid,
            bound: false,
            _claim: claim,
            _owner: PhantomData,
        })
    }
    /// 对照原 liteeth_open 中 request_irq(..., liteeth_interrupt, ..., netdev)。
    /// Linux 把函数指针和 netdev cookie 注册到 IRQ 核心；本实现不向 EL1 传回调，
    /// 而把 DTB 解析得到、仍须 EL1 授权的 GIC INTID 接到本 Driver 的 Notification。
    /// 原 Linux irq 是 IRQ domain 映射后的逻辑号，不能普遍当作 GIC INTID 直接照搬。
    pub fn open(&mut self) -> Result<(), i32> {
        if self.bound {
            return Err(-16);
        }
        self.device.prepare_open()?;
        // EL0 参数：INTID、attach 时创建的通知 handle、badge=1、当前目标 CPU。
        // 经 runtime 发 SYS_IRQ_BIND；EL1 校验 IRQ grant、通知归属和重复绑定，
        // 保存 INTID→通知/badge 的路由并开放 GIC SPI；失败时不继续开放设备事件。
        self.notification
            .0
            .bind_irq(self.intid, 1)
            .map_err(|_| -5)?;
        self.bound = true;
        // 与 Linux 的 request_irq 一样，内核侧接入不负责开启网卡自身的事件位。
        // 因此必须再写 LiteEth RX/TX EV_ENABLE；两者分别控制 GIC 路由与设备中断源。
        self.device.enable();
        Ok(())
    }
    pub fn send(&mut self, bytes: &[u8]) -> Result<bool, i32> {
        self.device.send(bytes)
    }
    /// 替代 Linux IRQ 核心调用 liteeth_interrupt(irq, dev_id) 的接入过程。
    /// 本函数在所属 EL0 线程运行；self 直接持有设备状态，不需要 C 的 dev_id cookie。
    pub fn wait_irq(&mut self) -> Result<(), i32> {
        if !self.bound {
            return Err(-22);
        }
        // SYS_NOTIFICATION_WAIT：原子取走 pending badge；为空才阻塞当前线程。
        let badge = self.notification.0.wait().map_err(|_| -5)?;
        if badge & 1 != 0 {
            self.device.handle_irq();
            if runtime::svc(exo_abi::SYS_IRQ_ACK, self.intid as u64, 0, 0) != 0 {
                return Err(-5);
            }
        }
        Ok(())
    }
    pub fn receive(&mut self, out: &mut [u8]) -> usize {
        self.device.receive(out).unwrap_or_else(|_| fatal())
    }
    pub fn stats(&self) -> Result<super::liteeth_core::Stats, i32> {
        Ok(self.device.stats())
    }
    /// 对照 liteeth_stop 的关闭设备事件→free_irq 顺序。
    /// Linux free_irq 会撤销回调并等待在途处理完成；这里靠单所属线程串行调用，
    /// 保证进入 close 时没有另一个 EL0 handle_irq 同时执行，不是通用多核回调同步。
    pub fn close(&mut self) -> Result<(), i32> {
        if self.bound {
            self.device.stop();
            // SYS_IRQ_UNBIND：EL1 禁用 SPI 并移除当前任务的绑定，成功返回后清 bound。
            // 本步不释放 MMIO 或 Notification；之后 Driver 的字段 Drop 才分别回收。
            // 解绑失败则保持 bound，禁止把仍可能关联中断的资源当作已安全回收。
            runtime::irq_unbind(self.intid).map_err(|_| -5)?;
            self.bound = false;
        }
        Ok(())
    }
    pub fn detach(mut self) -> Result<(), i32> {
        self.close()
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        // 此任务串行处理 IRQ；close 返回后不可能再进入回调，随后字段才自动回收。
        if self.close().is_err() {
            fatal();
        }
    }
}
