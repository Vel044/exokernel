// SPDX-License-Identifier: GPL-2.0-or-later
//! platform 注册/匹配及 remove 后的 devm 清理；入口函数保持 Linux 的 pdev/返回协议。
use super::{
    error::ENODEV,
    platform::{devres_release_all, platform_device},
};
pub struct of_device_id {
    pub compatible: &'static str,
}
pub struct device_driver {
    pub name: &'static str,
    pub of_match_table: &'static [of_device_id],
}
pub struct platform_driver {
    pub probe: unsafe fn(*mut platform_device) -> i32,
    pub remove_new: unsafe fn(*mut platform_device),
    pub driver: device_driver,
}
pub unsafe fn bind(driver: &platform_driver, pdev: *mut platform_device, compatible: &str) -> i32 {
    if !driver
        .driver
        .of_match_table
        .iter()
        .any(|id| id.compatible == compatible)
    {
        return -ENODEV;
    }
    let ret = (driver.probe)(pdev);
    if ret != 0 {
        devres_release_all(&mut (*pdev).dev);
    }
    ret
}
pub unsafe fn detach(driver: &platform_driver, pdev: *mut platform_device) {
    (driver.remove_new)(pdev);
    devres_release_all(&mut (*pdev).dev);
}
#[macro_export]
macro_rules! MODULE_DEVICE_TABLE {
    ($type:ident,$name:ident) => {
        const _: &[$crate::linux_api::module::of_device_id] = $name;
    };
}
#[macro_export]
macro_rules! module_platform_driver {
    ($driver:ident) => {
        pub(crate) fn registered_platform_driver(
        ) -> &'static $crate::linux_api::module::platform_driver {
            &$driver
        }
    };
}
#[macro_export]
macro_rules! MODULE_AUTHOR {
    ($value:expr) => {
        pub const MODULE_AUTHOR_VALUE: &str = $value;
    };
}
#[macro_export]
macro_rules! MODULE_DESCRIPTION {
    ($value:expr) => {
        pub const MODULE_DESCRIPTION_VALUE: &str = $value;
    };
}
#[macro_export]
macro_rules! MODULE_LICENSE {
    ($value:expr) => {
        pub const MODULE_LICENSE_VALUE: &str = $value;
    };
}
