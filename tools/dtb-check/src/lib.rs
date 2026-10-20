//! 在宿主机验证生产 libOS 设备发现代码，无需进入 EL1 或操作真实设备。
extern crate alloc;
#[path = "../../../libos/src/drivers/dtb.rs"]
pub mod dtb;
#[path = "../../../libos/src/drivers/pci_host.rs"]
pub mod pci_host;
#[path = "../../../libos/src/drivers/platform.rs"]
pub mod platform;

#[cfg(test)]
mod tests {
    use super::*;
    const QEMU: &[u8] = include_bytes!("../../../platform/qemu/qemu-virt.dtb");
    const PI5: &[u8] = include_bytes!("../../../platform/pi5/bcm2712-rpi-5-b.dtb");
    #[test]
    fn qemu_uart_pci_routes_and_virtio() {
        let info = dtb::discover(QEMU, 4).unwrap();
        assert_eq!(
            (info.uart.base, info.uart.size, info.uart.intid),
            (0x9000000, 0x1000, 33)
        );
        assert_eq!(info.xhci_transport, platform::XHCI_TRANSPORT_PCI);
        assert_eq!(
            (info.pci.ecam_pa, info.pci.ecam_size),
            (0x4010000000, 0x10000000)
        );
        assert_eq!((info.pci.range_count, info.pci.intx_route_count), (3, 16));
        assert_eq!(info.pci.intx_routes[0].intid, 35);
        assert_eq!(info.pci.intx_routes[8].intid, 37);
        assert_eq!(
            (info.virtio_mmio.base, info.virtio_mmio.size),
            (0xa000000, 0x4000)
        );
    }
    #[test]
    fn pi5_alias_ranges_and_rp1_irq() {
        let info = dtb::discover(PI5, 4).unwrap();
        assert_eq!(
            (info.uart.base, info.uart.size, info.uart.intid),
            (0x107d001000, 0x200, 153)
        );
        assert_eq!(info.xhci_transport, platform::XHCI_TRANSPORT_DIRECT);
        assert_eq!(
            (info.xhci.base, info.xhci.size, info.xhci.intid),
            (0x1f00200000, 0x100000, 158)
        );
        assert_eq!(info.pci.present, 0);
    }
    #[test]
    fn malformed_headers_and_node_boundaries_fail() {
        for length in [0, 8, 39, 40, QEMU.len() - 1] {
            assert!(dtb::discover(&QEMU[..length], 4).is_err());
        }
        for (offset, value) in [
            (4, u32::MAX),
            (8, u32::MAX),
            (12, 0),
            (32, u32::MAX),
            (36, u32::MAX),
        ] {
            let mut data = QEMU.to_vec();
            data[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
            assert!(dtb::discover(&data, 4).is_err());
        }
        let mut data = QEMU.to_vec();
        let start = u32::from_be_bytes(data[8..12].try_into().unwrap()) as usize;
        data[start..start + 4].copy_from_slice(&3u32.to_be_bytes());
        assert!(dtb::discover(&data, 4).is_err());
    }
    #[test]
    fn device_addresses_come_from_blob_and_cpu_count_from_kernel() {
        let mut data = QEMU.to_vec();
        let original = [0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0];
        let index = data
            .windows(original.len())
            .position(|bytes| bytes == original)
            .unwrap();
        data[index + 7] = 0x10;
        let info = dtb::discover(&data, 2).unwrap();
        assert_eq!(info.uart.base, 0x9000010);
        assert_eq!(info.cpu_count, 2);
    }
}
