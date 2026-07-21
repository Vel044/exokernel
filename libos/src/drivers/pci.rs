use exo_abi::PciHostInfo;

const PCI_CLASS_XHCI: u32 = 0x0c03_30;

#[derive(Clone, Copy)]
pub struct XhciFunction {
    pub bdf: Bdf,
    pub bar_pa: u64,
    pub bar_size: u64,
    pub intid: u32,
}

#[derive(Clone, Copy)]
pub struct Bdf {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

pub fn find_xhci(info: &PciHostInfo) -> Result<XhciFunction, &'static str> {
    if info.present == 0 || info.bus_start != 0 {
        return Err("PCI host unavailable or bus 0 not accessible");
    }
    crate::runtime::map_mmio(info.ecam_pa, 1024 * 1024, exo_abi::PCI_ECAM_VA)
        .map_err(|_| "failed to map PCI ECAM")?;
    let ecam = Ecam {
        base: exo_abi::PCI_ECAM_VA,
    };

    let mut device = 0u8;
    while device < 32 {
        let mut function = 0u8;
        let function0 = Bdf {
            bus: 0,
            device,
            function: 0,
        };
        let vendor0 = ecam.read16(function0, 0x00);
        if vendor0 == 0xffff {
            device += 1;
            continue;
        }
        let header = ecam.read8(function0, 0x0e);
        let function_count = if (header & 0x80) != 0 { 8 } else { 1 };
        while function < function_count {
            let bdf = Bdf {
                bus: 0,
                device,
                function,
            };
            if ecam.read16(bdf, 0x00) != 0xffff {
                let class = ecam.read32(bdf, 0x08) >> 8;
                if class == PCI_CLASS_XHCI {
                    let (bar_bus, bar_size) = probe_bar0(&ecam, bdf)?;
                    let bar_pa = translate_bar(info, bar_bus, bar_size)?;
                    let pin = ecam.read8(bdf, 0x3d) as u32;
                    let intid = route_intx(info, bdf, pin)?;
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
    let command = ecam.read16(bdf, 0x04);
    ecam.write16(bdf, 0x04, command & !(1 << 1));
    let original_low = ecam.read32(bdf, 0x10);
    if (original_low & 1) != 0 {
        ecam.write16(bdf, 0x04, command);
        return Err("xHCI BAR0 is an I/O BAR");
    }
    let is_64 = ((original_low >> 1) & 0x3) == 0x2;
    let original_high = if is_64 { ecam.read32(bdf, 0x14) } else { 0 };
    ecam.write32(bdf, 0x10, u32::MAX);
    if is_64 {
        ecam.write32(bdf, 0x14, u32::MAX);
    }
    let mask_low = ecam.read32(bdf, 0x10);
    let mask_high = if is_64 { ecam.read32(bdf, 0x14) } else { 0 };
    ecam.write32(bdf, 0x10, original_low);
    if is_64 {
        ecam.write32(bdf, 0x14, original_high);
    }
    ecam.write16(bdf, 0x04, command);

    let address = ((original_high as u64) << 32) | ((original_low & !0xf) as u64);
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
    let end = address.checked_add(size).ok_or("BAR overflow")?;
    let mut index = 0usize;
    while index < info.range_count as usize {
        let range = info.ranges[index];
        let range_end = range.child_base.saturating_add(range.size);
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
    if !(1..=4).contains(&pin) {
        return Err("invalid PCI interrupt pin");
    }
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
    base: u64,
}

impl Ecam {
    fn address(&self, bdf: Bdf, offset: u16) -> u64 {
        self.base
            + ((bdf.bus as u64) << 20)
            + ((bdf.device as u64) << 15)
            + ((bdf.function as u64) << 12)
            + offset as u64
    }

    fn read8(&self, bdf: Bdf, offset: u16) -> u8 {
        unsafe { core::ptr::read_volatile(self.address(bdf, offset) as *const u8) }
    }

    fn read16(&self, bdf: Bdf, offset: u16) -> u16 {
        unsafe { core::ptr::read_volatile(self.address(bdf, offset) as *const u16) }
    }

    fn read32(&self, bdf: Bdf, offset: u16) -> u32 {
        unsafe { core::ptr::read_volatile(self.address(bdf, offset) as *const u32) }
    }

    fn write16(&self, bdf: Bdf, offset: u16, value: u16) {
        unsafe { core::ptr::write_volatile(self.address(bdf, offset) as *mut u16, value) }
    }

    fn write32(&self, bdf: Bdf, offset: u16, value: u32) {
        unsafe { core::ptr::write_volatile(self.address(bdf, offset) as *mut u32, value) }
    }
}
