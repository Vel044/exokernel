#!/bin/bash
set -e
cd "$(dirname "$0")"

echo "=== Exokernel QEMU Test ==="
echo ""

# 确保 ESP 存在
if [ ! -f esp.img ]; then
    echo "ERROR: esp.img not found. Run build.sh first."
    exit 1
fi

# 使用干净的 vars.fd 避免旧 boot entries
VARS=/tmp/exokernel_vars.fd
if [ ! -f "$VARS" ]; then
    dd if=/dev/zero of="$VARS" bs=1M count=64 2>/dev/null
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
echo "  qemu-system-aarch64 -M virt,virtualization=on ..."
echo ""


exec qemu-system-aarch64 \
  -M virt,virtualization=on -cpu cortex-a72 -m 4G \
  -drive if=pflash,format=raw,unit=0,file=/opt/homebrew/share/qemu/edk2-aarch64-code.fd,readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$VARS" \
  -drive file=esp.img,format=raw,if=none,id=drive0 \
  -device virtio-blk-device,drive=drive0 \
  $USB_DEV \
  -serial stdio -monitor none -display none \
  -nographic -no-reboot
