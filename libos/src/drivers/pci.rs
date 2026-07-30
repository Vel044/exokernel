//! EL0中的最小PCI枚举器，只用于寻找QEMU提供的xHCI控制器。
//!
//! 资源边界如下：
//!
//! ```text
//! EL1解析DTB
//!   → 把ECAM物理范围和PCI MMIO window写入UserBootInfo及任务授权表
//! EL0调用SYS_MAP_MMIO
//!   → EL1校验授权并建立ECAM VA映射
//! EL0通过volatile直接读取PCI配置空间
//!   → 找到xHCI、解析BAR、打开Memory Space和Bus Master
//! ```
//!
//! 只有“建立映射”需要进入Kernel。映射成功后，每一次PCI配置寄存器访问
//! 都是EL0对ECAM虚拟地址的普通volatile load/store，不再逐次调用系统调用。

use exo_abi::PciHostInfo;

/// PCI class/subclass/programming-interface = 0x0c/0x03/0x30，表示xHCI。
const PCI_CLASS_XHCI: u32 = 0x0c03_30;

/// 枚举完成后交给USB模块的最小xHCI资源描述。
#[derive(Clone, Copy)]
pub struct XhciFunction {
    /// PCI总线、设备、功能号，用于日志和配置空间定位。
    pub bdf: Bdf,
    /// BAR经过DTB ranges转换后的CPU物理地址。
    pub bar_pa: u64,
    /// BAR覆盖的寄存器窗口大小。
    pub bar_size: u64,
    /// PCI INTx经过DTB interrupt-map解析后对应的GIC INTID。
    pub intid: u32,
}

/// PCI配置空间中一个function的地址：Bus/Device/Function。
#[derive(Clone, Copy)]
pub struct Bdf {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

pub fn find_xhci(info: &PciHostInfo) -> Result<XhciFunction, &'static str> {
    // UserBootInfo只描述EL1已经授权给当前libOS的PCI host。这里不能自行
    // 猜测ECAM物理地址；present=0或bus 0不可访问就立即拒绝。
    if info.present == 0 || info.bus_start != 0 {
        return Err("PCI host unavailable or bus 0 not accessible");
    }

    // 第一次跨越EL0/EL1边界：
    // x0=ECAM PA，x1=1MiB，x2=固定EL0 VA。1MiB恰好覆盖bus 0的
    // 32 devices × 8 functions × 每function 4KiB配置空间。
    // Kernel校验这段PA属于当前任务grant后，建立Device类型页表映射。
    crate::runtime::map_mmio(info.ecam_pa, 1024 * 1024, exo_abi::PCI_ECAM_VA)
        .map_err(|_| "failed to map PCI ECAM")?;

    // 从这里开始，ecam.base是EL0虚拟地址。下方read/write不再进入Kernel。
    let ecam = Ecam {
        base: exo_abi::PCI_ECAM_VA,
    };

    // PCI bus 0最多有32个device，每个device最多有8个function。
    let mut device = 0u8;
    while device < 32 {
        let mut function = 0u8;
        let function0 = Bdf {
            bus: 0,
            device,
            function: 0,
        };
        // Vendor ID为0xffff表示这个device/function不存在。
        let vendor0 = ecam.read16(function0, 0x00);
        if vendor0 == 0xffff {
            device += 1;
            continue;
        }
        // Header Type bit7表示multi-function。单功能设备只扫描function 0。
        let header = ecam.read8(function0, 0x0e);
        let function_count = if (header & 0x80) != 0 { 8 } else { 1 };
        while function < function_count {
            let bdf = Bdf {
                bus: 0,
                device,
                function,
            };
            if ecam.read16(bdf, 0x00) != 0xffff {
                // 配置寄存器0x08布局为Class/Subclass/ProgIF/Revision。
                // 右移8位丢掉Revision，得到用于匹配xHCI的0x0c0330。
                let class = ecam.read32(bdf, 0x08) >> 8;
                if class == PCI_CLASS_XHCI {
                    // BAR中的地址是PCI bus address，还不能直接拿去建CPU页表。
                    let (bar_bus, bar_size) = probe_bar0(&ecam, bdf)?;
                    // 使用EL1从DTB ranges裁剪后交来的窗口完成bus→CPU PA转换。
                    let bar_pa = translate_bar(info, bar_bus, bar_size)?;
                    // Interrupt Pin寄存器给出INTA..INTD，再查DTB interrupt-map。
                    let pin = ecam.read8(bdf, 0x3d) as u32;
                    let intid = route_intx(info, bdf, pin)?;

                    // PCI Command bit1=Memory Space Enable：允许BAR MMIO响应；
                    // bit2=Bus Master Enable：允许xHCI主动发起DMA访问内存。
                    // 这是EL0直接写ECAM寄存器，不是SYS_PCI_CFG_WRITE。
                    let command = ecam.read16(bdf, 0x04);
                    ecam.write16(bdf, 0x04, command | (1 << 1) | (1 << 2));
                    return Ok(XhciFunction {
                        bdf,
                        bar_pa,
                        bar_size,
                        intid,
                    });
                }
            }
            function += 1;
        }
        device += 1;
    }
    Err("xHCI PCI function not found")
}

fn probe_bar0(ecam: &Ecam, bdf: Bdf) -> Result<(u64, u64), &'static str> {
    // 探测BAR大小时必须暂时关闭Memory Space Decode，避免写全1的探测值
    // 被设备当成正在使用的新地址。
    let command = ecam.read16(bdf, 0x04);
    ecam.write16(bdf, 0x04, command & !(1 << 1));

    // BAR0位于配置空间0x10。bit0=1表示I/O BAR，本驱动只接受MMIO BAR。
    let original_low = ecam.read32(bdf, 0x10);
    if (original_low & 1) != 0 {
        ecam.write16(bdf, 0x04, command);
        return Err("xHCI BAR0 is an I/O BAR");
    }
    // Memory BAR bits[2:1]=0b10表示64位地址，BAR1保存高32位。
    let is_64 = ((original_low >> 1) & 0x3) == 0x2;
    let original_high = if is_64 { ecam.read32(bdf, 0x14) } else { 0 };

    // PCI标准BAR size probe：保存原值，写入全1，再读回设备硬连线的mask。
    ecam.write32(bdf, 0x10, u32::MAX);
    if is_64 {
        ecam.write32(bdf, 0x14, u32::MAX);
    }
    let mask_low = ecam.read32(bdf, 0x10);
    let mask_high = if is_64 { ecam.read32(bdf, 0x14) } else { 0 };
    // 探测后必须原样恢复UEFI已经分配的BAR地址。
    ecam.write32(bdf, 0x10, original_low);
    if is_64 {
        ecam.write32(bdf, 0x14, original_high);
    }
    ecam.write16(bdf, 0x04, command);

    // BAR低4位是属性位，不属于地址。
    let address = ((original_high as u64) << 32) | ((original_low & !0xf) as u64);
    // 对mask按位取反再加1，得到设备实际要求的2次幂窗口大小。
    let size = if is_64 {
        let mask = ((mask_high as u64) << 32) | ((mask_low & !0xf) as u64);
        (!mask).wrapping_add(1)
    } else {
        (!(mask_low & !0xf)).wrapping_add(1) as u64
    };
    if address == 0 || size == 0 || !size.is_power_of_two() {
        return Err("invalid xHCI BAR0");
    }
    Ok((address, size))
}

fn translate_bar(info: &PciHostInfo, address: u64, size: u64) -> Result<u64, &'static str> {
    // BAR保存的是PCI child bus address；AArch64 CPU页表需要CPU physical
    // address。DTB ranges同时给出child_base、parent_base和窗口长度。
    let end = address.checked_add(size).ok_or("BAR overflow")?;
    let mut index = 0usize;
    while index < info.range_count as usize {
        let range = info.ranges[index];
        let range_end = range.child_base.saturating_add(range.size);
        // 0x02000000=32-bit MMIO，0x03000000=64-bit MMIO。
        let space = range.flags & 0x0300_0000;
        if (space == 0x0200_0000 || space == 0x0300_0000)
            && address >= range.child_base
            && end <= range_end
        {
            return range
                .parent_base
                .checked_add(address - range.child_base)
                .ok_or("BAR translation overflow");
        }
        index += 1;
    }
    Err("xHCI BAR outside authorized PCI ranges")
}

fn route_intx(info: &PciHostInfo, bdf: Bdf, pin: u32) -> Result<u32, &'static str> {
    // PCI Interrupt Pin编码1..4分别表示INTA..INTD。
    if !(1..=4).contains(&pin) {
        return Err("invalid PCI interrupt pin");
    }
    // 按DTB PCI child address cell编码BDF，再应用interrupt-map-mask。
    let child_hi =
        ((bdf.bus as u32) << 16) | ((bdf.device as u32) << 11) | ((bdf.function as u32) << 8);
    let wanted_hi = child_hi & info.interrupt_map_mask_hi;
    let wanted_pin = pin & info.interrupt_map_mask_pin;
    let mut index = 0usize;
    while index < info.intx_route_count as usize {
        let route = info.intx_routes[index];
        if (route.child_address_hi & info.interrupt_map_mask_hi) == wanted_hi
            && (route.pin & info.interrupt_map_mask_pin) == wanted_pin
        {
            return Ok(route.intid);
        }
        index += 1;
    }
    Err("PCI INTx route not found")
}

struct Ecam {
    /// 已经映射到EL0的ECAM起始虚拟地址，不是物理地址。
    base: u64,
}

impl Ecam {
    fn address(&self, bdf: Bdf, offset: u16) -> u64 {
        // ECAM标准地址公式：
        // base + bus*1MiB + device*32KiB + function*4KiB + register offset。
        self.base
            + ((bdf.bus as u64) << 20)
            + ((bdf.device as u64) << 15)
            + ((bdf.function as u64) << 12)
            + offset as u64
    }

    fn read8(&self, bdf: Bdf, offset: u16) -> u8 {
        // volatile禁止编译器删除、合并或缓存这次访问。页表把该VA标记为
        // Device memory，所以最终AArch64 load会到达QEMU的PCI ECAM设备模型。
        unsafe { core::ptr::read_volatile(self.address(bdf, offset) as *const u8) }
    }

    fn read16(&self, bdf: Bdf, offset: u16) -> u16 {
        unsafe { core::ptr::read_volatile(self.address(bdf, offset) as *const u16) }
    }

    fn read32(&self, bdf: Bdf, offset: u16) -> u32 {
        unsafe { core::ptr::read_volatile(self.address(bdf, offset) as *const u32) }
    }

    fn write16(&self, bdf: Bdf, offset: u16, value: u16) {
        // 同理，这是EL0直接向PCI配置寄存器发出的store，不经过Kernel。
        unsafe { core::ptr::write_volatile(self.address(bdf, offset) as *mut u16, value) }
    }

    fn write32(&self, bdf: Bdf, offset: u16, value: u32) {
        unsafe { core::ptr::write_volatile(self.address(bdf, offset) as *mut u32, value) }
    }
}
