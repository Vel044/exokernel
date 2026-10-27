// SPDX-License-Identifier: GPL-2.0-or-later
//! platform 资源目录和 devm 生命周期；驱动只传 pdev/name，映射授权仍由 EL1 校验。
use super::{error::*, kernel, memory, netdevice::net_device};
use alloc::vec::Vec;
use core::{
    ffi::{c_char, c_void, CStr},
    ptr,
};
#[derive(Clone, Copy)]
pub struct NamedResource {
    pub name: &'static str,
    pub base: u64,
    pub size: u64,
    pub user_va: u64,
}
pub struct device_node {
    pub properties: Vec<(&'static str, u32)>,
    pub addresses: [Option<[u8; 6]>; 4],
}
pub enum Devres {
    Memory {
        ptr: *mut u8,
        size: usize,
        align: usize,
        drop: Option<unsafe fn(*mut u8)>,
    },
    Mapping {
        addr: *mut c_void,
        size: u64,
        pa: u64,
        region_size: u64,
    },
}
pub struct device {
    pub name: &'static str,
    pub parent: *mut device,
    pub of_node: *mut device_node,
    pub drvdata: *mut c_void,
    pub devres: Vec<Devres>,
}
impl device {
    pub fn new(name: &'static str, of_node: *mut device_node) -> Self {
        Self {
            name,
            parent: ptr::null_mut(),
            of_node,
            drvdata: ptr::null_mut(),
            devres: Vec::new(),
        }
    }
}
pub struct platform_device {
    pub dev: device,
    pub resources: Vec<NamedResource>,
    pub irqs: Vec<u32>,
}
/// 保存一条 devm 记录；资源对象必须保持稳定地址，直到框架执行 release_all。
pub unsafe fn manage_memory(
    dev: *mut device,
    p: *mut u8,
    size: usize,
    align: usize,
    drop: Option<unsafe fn(*mut u8)>,
) -> bool {
    if (*dev).devres.try_reserve(1).is_err() {
        return false;
    }
    (*dev).devres.push(Devres::Memory {
        ptr: p,
        size,
        align,
        drop,
    });
    true
}
pub unsafe fn platform_set_drvdata(pdev: *mut platform_device, data: *mut c_void) {
    (*pdev).dev.drvdata = data;
}
pub unsafe fn platform_get_drvdata(pdev: *const platform_device) -> *mut c_void {
    (*pdev).dev.drvdata
}
pub unsafe fn platform_get_irq(dev: *mut platform_device, num: u32) -> i32 {
    match (&(*dev).irqs).get(num as usize) {
        Some(&irq) => irq as i32,
        None => -ENXIO,
    }
}
pub unsafe fn of_property_read_u32(
    np: *const device_node,
    propname: *const c_char,
    out_value: *mut u32,
) -> i32 {
    if np.is_null() {
        return -EINVAL;
    }
    for &(name, value) in &(*np).properties {
        if name.as_bytes() == CStr::from_ptr(propname).to_bytes() {
            *out_value = value;
            return 0;
        }
    }
    -22
}
pub unsafe fn of_get_ethdev_address(np: *mut device_node, dev: *mut net_device) -> i32 {
    if np.is_null() {
        return -ENODEV;
    }
    for address in (*np).addresses.iter().flatten() {
        if *address != [0; 6] && address[0] & 1 == 0 {
            (*dev).dev_addr = *address;
            return 0;
        }
    }
    -ENODEV
}
static REGIONS: super::lock::Lock<Vec<(u64, u64, usize)>> = super::lock::Lock::new(Vec::new());
pub unsafe fn devm_platform_ioremap_resource_byname(
    pdev: *mut platform_device,
    name: *const c_char,
) -> *mut c_void {
    let r = match (*pdev)
        .resources
        .iter()
        .find(|r| r.name.as_bytes() == CStr::from_ptr(name).to_bytes())
    {
        Some(r) => *r,
        None => return ERR_PTR(-EINVAL as i64),
    };
    if r.user_va & 4095 != 0 || r.size == 0 {
        return ERR_PTR(-EINVAL as i64);
    }
    let mut regions = REGIONS.lock();
    let end = match r.base.checked_add(r.size) {
        Some(e) => e,
        None => return ERR_PTR(-EINVAL as i64),
    };
    let map_pa = r.base & !4095;
    let map_size = match end.checked_add(4095) {
        Some(e) => (e & !4095) - map_pa,
        None => return ERR_PTR(-EINVAL as i64),
    };
    if regions
        .iter()
        .any(|&(pa, size, _)| r.base < pa + size && pa < end)
    {
        return ERR_PTR(-EBUSY as i64);
    }
    if regions.try_reserve(1).is_err() {
        return ERR_PTR(-ENOMEM as i64);
    }
    if (*pdev).dev.devres.try_reserve(1).is_err() {
        return ERR_PTR(-ENOMEM as i64);
    }
    let addr = match (kernel::ops().map)(map_pa, map_size, r.user_va) {
        Ok(addr) => addr,
        Err(e) => return ERR_PTR(e as i64),
    };
    regions.push((r.base, r.size, addr as usize));
    (*pdev).dev.devres.push(Devres::Mapping {
        addr,
        size: map_size,
        pa: r.base,
        region_size: r.size,
    });
    addr.byte_add((r.base - map_pa) as usize)
}
pub unsafe fn devres_release_all(dev: *mut device) {
    while let Some(r) = (*dev).devres.pop() {
        match r {
            Devres::Mapping {
                addr,
                size,
                pa,
                region_size,
            } => {
                if (kernel::ops().unmap)(addr, size).is_err() {
                    panic!("解除仍被使用的MMIO失败");
                }
                let mut regions = REGIONS.lock();
                let i = regions
                    .iter()
                    .position(|&r| r == (pa, region_size, addr as usize))
                    .expect("缺少设备区域占用记录");
                regions.remove(i);
            }
            Devres::Memory {
                ptr,
                size,
                align,
                drop,
            } => {
                if let Some(f) = drop {
                    f(ptr);
                }
                memory::release(ptr, size, align);
            }
        }
    }
    (*dev).drvdata = ptr::null_mut();
}
/// 类型宏由 Rust 宏保留第二个类型参数，不把类型改成运行时参数。
#[macro_export]
macro_rules! devm_netdev_alloc_pcpu_stats {
    ($dev:expr,$type:ty) => {{
        $crate::linux_api::netdevice::alloc_pcpu_stats::<$type>($dev)
    }};
}
