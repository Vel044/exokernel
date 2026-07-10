#!/bin/bash
set -e
cd "$(dirname "$0")"
source ./config.sh

echo "=== Exokernel QEMU Test ==="
echo ""

# 确保 ESP 存在
if [ ! -f esp.img ]; then
    echo "ERROR: esp.img not found. Run build.sh first."
    exit 1
fi

# 首次从 EDK2 提供的有效 NVRAM 模板开始，之后保留启动项。
#
# 不能用 dd 创建全零文件：第一次启动时 EDK2 需要先格式化变量存储并创建
# Boot#### 启动项，常常要到第二次运行才能自动找到 BOOTAA64.EFI。
# 使用官方 VARS 模板后，首次启动会完成设备扫描；后续复用生成的 Boot####
# 启动项，避免每次都重新扫描。
VARS=/tmp/exokernel_vars.fd
VARS_TEMPLATE="$QEMU_VARS_TEMPLATE"
if [ ! -f "$VARS_TEMPLATE" ]; then
    echo "ERROR: EDK2 VARS template not found: $VARS_TEMPLATE"
    exit 1
fi
if [ ! -f "$VARS" ] || [ "$(wc -c < "$VARS")" -ne "$(wc -c < "$VARS_TEMPLATE")" ]; then
    cp "$VARS_TEMPLATE" "$VARS"
    echo "Initialized UEFI NVRAM; first boot may scan devices for about 10 seconds."
fi

USB_DEV=""
# ═══════════════════════════════════════════════════════════════
# USB 串口设备透传
#   如果插了 USB 串口 (如 CH340/CP2102/FTDI), QEMU 能把它传进
#   虚拟机, 外核可以直接操作它。
#
#   检测: ls /dev/cu.usbmodem* 或 ls /dev/cu.usbserial-*
#
#   QEMU 在 macOS 上不支持 usb-host 透传, 所以用 chardev +
#   usb-serial 模拟一个 USB 串口, 绑定到宿主机的设备文件。
# ═══════════════════════════════════════════════════════════════
USB_SERIAL=$(ls /dev/cu.usbmodem* 2>/dev/null | head -1)
if [ -n "$USB_SERIAL" ]; then
    echo "USB serial found: $USB_SERIAL"
    USB_DEV="-device qemu-xhci -chardev serial,path=$USB_SERIAL,id=usb0 -device usb-serial,chardev=usb0"
    echo "  → attached to VM as USB serial device"
fi

echo ""
echo "QEMU command:"
echo "  qemu-system-aarch64 -M $QEMU_MACHINE -cpu $QEMU_CPU -m $QEMU_MEMORY ..."
echo ""


exec qemu-system-aarch64 \
  -M "$QEMU_MACHINE" -cpu "$QEMU_CPU" -m "$QEMU_MEMORY" \
  -drive if=pflash,format=raw,unit=0,file="$QEMU_CODE",readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$VARS" \
  -drive file=esp.img,format=raw,if=none,id=drive0 \
  -device virtio-blk-device,drive=drive0 \
  $USB_DEV \
  -serial stdio -monitor none -display none \
  -nographic -no-reboot
