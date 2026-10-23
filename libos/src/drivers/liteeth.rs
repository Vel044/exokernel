// SPDX-License-Identifier: GPL-2.0-or-later
//! AI 辅助翻译的原生 Rust LiteEth 驱动：通过公共同名 Rust 封装使用已有内核接口。
//! 不链接 Linux C 驱动或 C/Rust FFI；Linux 同名 Rust 封装由 linux_api 复用。
//! liteeth_core 保留设备协议；本层独占 MMIO 映射、同名资源接口与 IRQ 生命周期。
use super::{
    liteeth_core::{Device, Io, Layout},
    platform::LiteEthInfo,
};
use crate::{
    linux_api::{
        exo::{ExoIrq, ExoMapper, Mapping},
        interrupt::{free_irq, request_irq, IrqRegistration},
        platform::{
            devm_platform_ioremap_resource_byname, of_property_read_u32, platform_get_drvdata,
            platform_get_irq, platform_set_drvdata, NamedResource, PlatformDevice,
        },
    },
    runtime,
};
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
/// 两段映射的拥有者；字段顺序不影响解绑，因为 Driver::drop 总是先 close。
struct Mmio {
    /// CSR VA：32 位总线事务，低 8/16 位存放逻辑值。
    registers: Mapping,
    /// 设备 SRAM VA：CPU volatile 复制，无 DMA 分配及缓存一致性操作。
    buffer: Mapping,
}
impl Io for Mmio {
    fn read(&self, offset: usize) -> u32 {
        self.registers.region().read32(offset)
    }
    fn write(&self, offset: usize, value: u32) {
        self.registers.region().write32(offset, value);
    }
    fn copy_from(&self, offset: usize, out: &mut [u8]) {
        self.buffer.region().copy_from(offset, out);
    }
    fn copy_to(&self, offset: usize, bytes: &[u8]) {
        self.buffer.region().copy_to(offset, bytes);
    }
}
/// 单线程独占设备，裸指针标记禁止 Send/Sync；没有 Linux 全局对象或异步 C 回调。
pub struct Driver {
    /// 设备状态机和 MMIO 映射，解绑后才随对象释放。
    device: Device<Mmio>,
    /// 注册层持有 Notification 和函数指针；没有 pending 时 WAIT 阻塞当前线程。
    irq: IrqRegistration<Device<Mmio>, ExoIrq>,
    /// DTB 给出的 GIC INTID，绑定仍由 EL1 grant 审核。
    intid: u32,
    /// 最后释放固定 VA 所有权，失败回滚由字段 Drop 自动完成。
    _claim: Claim,
    _owner: PhantomData<*mut ()>,
}
impl Driver {
    pub fn attach(info: LiteEthInfo) -> Result<Self, i32> {
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
        let mut platform = PlatformDevice {
            name: "liteeth",
            compatible: "litex,liteeth",
            resources: &resources,
            irqs: &irqs,
            properties: &properties,
            address: Some(info.address),
            drvdata: None,
        };
        platform_set_drvdata(&mut platform, info);
        let info = *platform_get_drvdata(&platform)?;
        let layout = Layout {
            rx_slots: of_property_read_u32(&platform, "litex,rx-slots")? as usize,
            tx_slots: of_property_read_u32(&platform, "litex,tx-slots")? as usize,
            slot_size: of_property_read_u32(&platform, "litex,slot-size")? as usize,
        };
        layout.validate(usize::try_from(info.buffer.size).map_err(|_| -22)?)?;
        let intid = platform_get_irq(&platform, 0)?;
        if info.mac.size < 0x44 || intid < 32 || info.mac.size > 4096 || info.buffer.size > 65536 {
            return Err(-22);
        }
        let claim = Claim::take()?;
        let irq = IrqRegistration::new(ExoIrq::new()?);
        let io = Mmio {
            registers: devm_platform_ioremap_resource_byname(&platform, "mac", &ExoMapper)?,
            buffer: devm_platform_ioremap_resource_byname(&platform, "buffer", &ExoMapper)?,
        };
        let device = Device::new(io, layout, info.buffer.size as usize)?;
        Ok(Self {
            device,
            irq,
            intid,
            _claim: claim,
            _owner: PhantomData,
        })
    }
    /// 对照原 liteeth_open 中 request_irq(..., liteeth_interrupt, ..., netdev)。
    /// Linux 把函数指针和 netdev cookie 注册到 IRQ 核心；本实现不向 EL1 传回调，
    /// 而把 DTB 解析得到、仍须 EL1 授权的 GIC INTID 接到本 Driver 的 Notification。
    /// 原 Linux irq 是 IRQ domain 映射后的逻辑号，不能普遍当作 GIC INTID 直接照搬。
    pub fn open(&mut self) -> Result<(), i32> {
        if self.irq.is_bound() {
            return Err(-16);
        }
        self.device.prepare_open()?;
        // EL0 参数：INTID、attach 时创建的通知 handle、badge=1、当前目标 CPU。
        // 经 runtime 发 SYS_IRQ_BIND；EL1 校验 IRQ grant、通知归属和重复绑定，
        // 保存 INTID→通知/badge 的路由并开放 GIC SPI；失败时不继续开放设备事件。
        request_irq(&mut self.irq, self.intid, Device::handle_irq, 0, "liteeth")?;
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
        // 注册层保存函数指针；WAIT 返回后在 EL0 调用，再执行 ACK。
        self.irq.wait_and_dispatch(&mut self.device)
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
        if self.irq.is_bound() {
            self.device.stop();
            // SYS_IRQ_UNBIND：EL1 禁用 SPI 并移除当前任务的绑定，成功返回后清除注册层 binding。
            // 本步不释放 MMIO 或 Notification；之后 Driver 的字段 Drop 才分别回收。
            // 解绑失败则保持 binding，禁止把仍可能关联中断的资源当作已安全回收。
            free_irq(&mut self.irq)?;
        }
        Ok(())
    }
    pub fn detach(mut self) -> Result<(), i32> {
        self.close()?;
        self.device.unregister()
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
