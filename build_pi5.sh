#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"

source ./scripts/sudo_keychain.sh
sudo_keychain_prepare

echo "==> 1. 编译 libos (EL0) - Pi5 RP1 xHCI CDC ACM"
cd libos
LIBOS_FEATURES="${LIBOS_FEATURES:-pi5-xhci,scservo}"
echo "    libOS features: ${LIBOS_FEATURES}"
cargo build --release --target aarch64-unknown-none --no-default-features --features "${LIBOS_FEATURES}"
cd ..

echo ""
echo "==> 2. strip → libos.elf"
rust-objcopy --strip-debug target/aarch64-unknown-none/release/libos libos/libos.elf
echo "    libos.elf: $(wc -c < libos/libos.elf) bytes"

echo ""
echo "==> 3. 编译 exokernel (EL2) - Pi5 版本"
# Pi5 feature: 禁用默认的 qemu feature，启用 pi5；libOS 同时启用 RP1 xHCI。
# UART 和 RP1 USB 地址都由固件 DTB 发现，不依赖 QEMU 地址。
cargo build -p exokernel --target aarch64-unknown-uefi --no-default-features --features pi5

echo ""
echo "==> 4. 写入 SD 卡"
SD_MOUNT="/Volumes/BOOT"
KERNEL_EFI="target/aarch64-unknown-uefi/debug/BOOTAA64.efi"

if [ ! -f "$KERNEL_EFI" ]; then
    echo "ERROR: $KERNEL_EFI 不存在"
    exit 1
fi

if [ ! -d "$SD_MOUNT" ]; then
    echo "SD 卡未挂载 (${SD_MOUNT})，尝试挂载 /dev/disk4s1..."
    sudo mkdir -p "$SD_MOUNT"
    sudo mount -t msdos /dev/disk4s1 "$SD_MOUNT" || {
        echo "挂载失败，请确认 SD 卡已插入"
        exit 1
    }
fi

sudo mkdir -p "$SD_MOUNT/EFI/BOOT"
sudo cp "$KERNEL_EFI" "$SD_MOUNT/EFI/BOOT/BOOTAA64.EFI"

echo "    kernel: $KERNEL_EFI"
echo "    → SD:    $SD_MOUNT/EFI/BOOT/BOOTAA64.EFI"
echo "    size:   $(wc -c < "$KERNEL_EFI") bytes"

sudo diskutil eject /dev/disk4 2>/dev/null || true

echo ""
echo "==> 完成!"
echo ""
echo "    SD 卡已弹出，插回 Pi5 开机。"
echo "    UART 连接命令:"
echo "      screen /dev/cu.usbmodem* 115200"
echo ""
echo "    在 screen 里按 Ctrl-A k 退出。"
