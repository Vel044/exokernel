// SPDX-License-Identifier: GPL-2.0-or-later
//! 保留模块宏的同名元信息构造和显式 platform 绑定，不实现 Linux 动态模块加载器。
use super::{error::ENXIO, platform::PlatformDevice};
pub struct Metadata {
    /// 编译进程序的来源信息，不取得硬件资源或执行系统调用。
    pub key: &'static str,
    pub value: &'static str,
}
pub const fn MODULE_AUTHOR(value: &'static str) -> Metadata {
    Metadata {
        key: "author",
        value,
    }
}
pub const fn MODULE_DESCRIPTION(value: &'static str) -> Metadata {
    Metadata {
        key: "description",
        value,
    }
}
pub const fn MODULE_LICENSE(value: &'static str) -> Metadata {
    Metadata {
        key: "license",
        value,
    }
}
pub struct DeviceTable {
    /// 目前仅支持 OF compatible 匹配；数组借用静态存储。
    pub kind: &'static str,
    pub compatibles: &'static [&'static str],
}
pub const fn MODULE_DEVICE_TABLE(
    kind: &'static str,
    compatibles: &'static [&'static str],
) -> DeviceTable {
    DeviceTable { kind, compatibles }
}
pub struct PlatformDriver<P, D> {
    /// 编译期匹配表以及设备初始化/退出函数指针，D 拥有已取得的资源。
    pub table: DeviceTable,
    pub probe: fn(&PlatformDevice<'_, P>) -> Result<D, i32>,
    pub remove: fn(D) -> Result<(), i32>,
}
pub struct PlatformBinding<D> {
    /// 拥有一次成功 probe 的设备，显式 remove 或 Drop 恰好执行一次退出。
    device: Option<D>,
    remove: fn(D) -> Result<(), i32>,
}
impl<D> PlatformBinding<D> {
    pub fn device(&mut self) -> &mut D {
        self.device.as_mut().unwrap()
    }
    pub fn remove(mut self) -> Result<(), i32> {
        (self.remove)(self.device.take().unwrap())
    }
}
impl<D> Drop for PlatformBinding<D> {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            (self.remove)(device).expect("platform remove 失败");
        }
    }
}
pub fn module_platform_driver<P, D>(
    driver: PlatformDriver<P, D>,
    device: &PlatformDevice<'_, P>,
) -> Result<PlatformBinding<D>, i32> {
    if driver.table.kind != "of" {
        return Err(super::error::EOPNOTSUPP);
    }
    if !driver.table.compatibles.contains(&device.compatible) {
        return Err(ENXIO);
    }
    let instance = (driver.probe)(device)?;
    Ok(PlatformBinding {
        device: Some(instance),
        remove: driver.remove,
    })
}
