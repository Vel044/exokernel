// SPDX-License-Identifier: GPL-2.0-or-later
//! 命名资源、设备属性与私有数据的同名访问。资源描述本身不授予硬件权限。
//! 实际映射由 Mapper 后端校验并建立，返回值拥有映射，Drop 负责 devm 式回收。
use super::{
    error::{EINVAL, ENXIO},
    netdevice::net_device,
};
#[derive(Clone, Copy)]
pub struct NamedResource {
    /// 设备树资源名，不与特定网卡绑定。
    pub name: &'static str,
    /// 硬件 PA/字节数，以及 libOS 选择的 CPU VA；VA 不是 DMA address。
    pub base: u64,
    pub size: u64,
    pub user_va: u64,
}
pub struct platform_device<'a, P = ()> {
    /// 本地设备名与 compatible，用于明确匹配，不实现 Linux 总线自动扫描。
    pub name: &'static str,
    pub compatible: &'a str,
    /// EL0 从 DTB 提取的命名资源、中断和 u32 属性。中断为后端接受的编号。
    pub resources: &'a [NamedResource],
    pub irqs: &'a [u32],
    pub properties: &'a [(&'a str, u32)],
    /// 可选设备身份和拥有的驱动数据；不保存无生命周期的 void*。
    pub address: Option<[u8; 6]>,
    pub drvdata: Option<P>,
}
pub trait Mapper {
    type Mapping;
    fn map(&self, resource: NamedResource) -> Result<Self::Mapping, i32>;
}
pub fn devm_platform_ioremap_resource_byname<P, M: Mapper>(
    dev: &platform_device<'_, P>,
    name: &str,
    mapper: &M,
) -> Result<M::Mapping, i32> {
    let resource = dev.resources.iter().find(|r| r.name == name).ok_or(ENXIO)?;
    mapper.map(*resource)
}
pub fn platform_get_irq<P>(dev: &platform_device<'_, P>, index: usize) -> Result<u32, i32> {
    dev.irqs.get(index).copied().ok_or(ENXIO)
}
/// 与 Linux setter 一样允许替换；Rust 自动释放被替换的拥有值。
pub fn platform_set_drvdata<P>(dev: &mut platform_device<'_, P>, value: P) {
    dev.drvdata = Some(value);
}
pub fn platform_get_drvdata<'a, P>(dev: &'a platform_device<'_, P>) -> Result<&'a P, i32> {
    dev.drvdata.as_ref().ok_or(ENXIO)
}
pub fn of_property_read_u32<P>(dev: &platform_device<'_, P>, name: &str) -> Result<u32, i32> {
    dev.properties
        .iter()
        .find_map(|(key, value)| (*key == name).then_some(*value))
        .ok_or(ENXIO)
}
pub fn of_get_ethdev_address<P, N>(
    dev: &platform_device<'_, P>,
    net: &mut net_device<N>,
) -> Result<(), i32> {
    let address = dev.address.ok_or(ENXIO)?;
    if address == [0; 6] || address[0] & 1 != 0 {
        return Err(EINVAL);
    }
    net.address = address;
    Ok(())
}
