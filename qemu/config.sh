#!/bin/bash

# 尽量贴近 Raspberry Pi 5 的 CPU/内存规模。设备模型仍是 QEMU virt，
# MMIO/GIC/PCIe 地址必须由 QEMU DTB 描述，不能硬改成 Pi5 地址。
QEMU_MACHINE="virt,virtualization=on"
QEMU_CPU="cortex-a76"
QEMU_MEMORY="4G"

QEMU_CODE="/opt/homebrew/share/qemu/edk2-aarch64-code.fd"
QEMU_VARS_TEMPLATE="/opt/homebrew/share/qemu/edk2-arm-vars.fd"
