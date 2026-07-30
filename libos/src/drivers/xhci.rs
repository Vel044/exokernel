//! xHCI资源映射与CrabUSB Host初始化。
//!
//! QEMU后端通过用户态PCI枚举得到BAR和INTx；Pi5后端直接使用EL1从DTB
//! 裁剪出的RP1资源。两条路径汇合后，本模块只负责MMIO、IRQ和Host生命周期。

use core::ptr::NonNull;

use crab_usb::{EventHandler, USBHost};

const XHCI_IRQ_BADGE: u64 = 1;

#[derive(Clone, Copy)]
struct HostResource {
    /// xHCI寄存器物理起始地址：QEMU来自BAR，Pi5来自DTB ranges翻译。
    base: u64,
    /// 必须覆盖Capability/Operational/Runtime/Doorbell寄存器的完整范围。
    size: u64,
    /// GIC INTID；QEMU为PCI INTx路由，Pi5为RP1 USB中断。
    intid: u32,
}

/// 已经复位、运行并开启中断的xHCI上下文。
pub(crate) struct XhciContext {
    /// CrabUSB Host状态机，拥有command ring和所有设备上下文。
    pub host: USBHost,
    /// xHCI Event Ring消费者，可从IRQ路径唤醒command/transfer Future。
    pub handler: EventHandler,
    /// IRQ ACK时使用的实际GIC INTID。
    pub intid: u32,
    /// 与该INTID绑定的Kernel异步通知对象。
    pub notification: crate::notification::Notification,
}

/// 取得授权资源并初始化xHCI控制器。
pub(crate) fn initialize(info: &exo_abi::UserBootInfo) -> Result<XhciContext, (&'static str, u64)> {
    let resource = discover_resource(info)?;

    // Kernel只建立Device-nGnRE映射；之后CrabUSB直接访问XHCI_VA寄存器。
    // 该SVC会校验PA是否属于当前任务grant，不能借此映射GIC或任意设备。
    crate::runtime::map_mmio(resource.base, resource.size, exo_abi::XHCI_VA)
        .map_err(|_| ("failed to map xHCI MMIO", 0x203))?;

    // Notification用于异步投递IRQ；它与同步Endpoint IPC不是同一对象。
    let notification = crate::notification::Notification::create()
        .map_err(|_| ("failed to create xHCI Notification", 0x204))?;
    // Kernel验证INTID授权，配置GIC触发类型，并建立IRQ→Notification关系。
    notification
        .bind_irq(resource.intid, XHCI_IRQ_BADGE)
        .map_err(|_| ("failed to bind xHCI IRQ", 0x204))?;

    // 这里只把固定EL0 VA包装为NonNull，不读取物理地址，也不产生SVC。
    let mmio =
        NonNull::new(exo_abi::XHCI_VA as *mut u8).ok_or(("invalid mapped xHCI VA", 0x205))?;
    // 问kernel要了一块DMA 剩下就是创建CrabUSB xHCI对象了
    let mut host = USBHost::new_xhci(mmio, &crate::dma::USB_KERNEL)
        .map_err(|_| ("CrabUSB xHCI construction failed", 0x205))?;
    // 调用 CrabUSB库create_event_handler 创建用户态xHCI事件处理器。
    let handler = host.create_event_handler();

    // init执行控制器halt/reset、DCBAA/CRCR/ERST编程、Run/Stop置位以及端口复位。
    crate::runtime::usb_executor::block_on_usb(
        host.init(),
        &handler,
        resource.intid,
        &notification,
    )
    .map_err(|_| ("CrabUSB xHCI initialization failed", 0x206))?;
    // 清理由初始化末尾遗留的Event Ring事件，再打开primary interrupter。
    handler.handle_event();
    host.enable_irq()
        .map_err(|_| ("CrabUSB failed to enable IRQ", 0x207))?;

    Ok(XhciContext {
        host,
        handler,
        intid: resource.intid,
        notification,
    })
}

fn discover_resource(info: &exo_abi::UserBootInfo) -> Result<HostResource, (&'static str, u64)> {
    // Pi5：EL1已经从RP1 snps,dwc3节点提取并授权标准xHCI寄存器窗口。
    if info.xhci_transport == exo_abi::XHCI_TRANSPORT_DIRECT {
        // SPI INTID从32开始；零地址、零长度或PPI/SGI都不是合法设备资源。
        if info.xhci.base == 0 || info.xhci.size == 0 || info.xhci.intid < 32 {
            return Err(("invalid direct RP1 xHCI resource", 0x200));
        }
        crate::runtime::puts(b"[libos] Pi5 direct RP1 xHCI backend\r\n");
        return Ok(HostResource {
            base: info.xhci.base,
            size: info.xhci.size,
            intid: info.xhci.intid,
        });
    }

    // QEMU：Kernel只给PCI Host资源，具体function枚举和BAR解析由EL0完成。
    if info.xhci_transport == exo_abi::XHCI_TRANSPORT_PCI {
        crate::runtime::puts(b"[libos] QEMU PCI xHCI backend\r\n");
        // find_xhci映射bus 0 ECAM，寻找class 0x0c0330并开启Bus Master。
        let xhci = crate::pci::find_xhci(&info.pci).map_err(|message| (message, 0x201))?;
        crate::runtime::puts(b"[libos] xHCI BDF=");
        crate::runtime::hex(
            ((xhci.bdf.bus as u64) << 16)
                | ((xhci.bdf.device as u64) << 8)
                | xhci.bdf.function as u64,
        );
        crate::runtime::puts(b" BAR=");
        crate::runtime::hex(xhci.bar_pa);
        crate::runtime::puts(b" IRQ=");
        crate::runtime::hex(xhci.intid as u64);
        crate::runtime::puts(b"\r\n");
        return Ok(HostResource {
            base: xhci.bar_pa,
            size: xhci.bar_size,
            intid: xhci.intid,
        });
    }

    Err(("no xHCI transport in UserBootInfo", 0x202))
}
