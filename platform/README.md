# 平台设备树快照

这里保存开发和调试使用的设备树快照：

- `qemu/qemu-virt.dtb`：QEMU 没有通过 UEFI 提供 FDT 时，内核使用的 fallback DTB。
- `qemu/qemu-virt.dts`：上述 DTB 的可读版本。
- `pi5/bcm2712-rpi-5-b.dtb`：树莓派 5 真机设备树快照。
- `pi5/bcm2712-rpi-5-b.dts`：上述 Pi5 DTB 的可读版本。

Pi5 正常启动时仍使用 UEFI config table 传入的实时 DTB。这里的 Pi5
快照用于开发、比对和测试，不应覆盖固件实际提供的设备树。
