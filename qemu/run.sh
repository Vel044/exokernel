#!/bin/bash
set -e
cd "$(dirname "$0")"
source ./config.sh

ROOT_DIR="$(cd .. && pwd)"

# 同一个 esp.img 和物理 USB 设备不能被多个 QEMU 实例同时占用。终端被
# 中断时 sudo 子进程可能继续存活，因此启动前显式拒绝重复实例。
EXISTING_QEMU="$(pgrep -x qemu-system-aarch64 2>/dev/null || true)"
if [ -n "$EXISTING_QEMU" ]; then
  echo "ERROR: qemu-system-aarch64 is already running (PID: $(echo "$EXISTING_QEMU" | tr '\n' ' '))" >&2
  echo "Stop the old QEMU instance before running this script again." >&2
  exit 1
fi

echo "==> 编译 Kernel + libOS"
(cd "$ROOT_DIR" && ./build.sh)

sync_efi_to_esp() {
    local mount_point="/tmp/exokernel-esp-$$"
    local disk=""
    mkdir -p "$mount_point"

    if [[ "${OSTYPE:-}" != darwin* ]]; then
        echo "ERROR: 自动更新 esp.img 目前只支持 macOS 的 hdiutil。"
        rmdir "$mount_point"
        exit 1
    fi

    local attach_output
    attach_output="$(hdiutil attach -readwrite -nobrowse -mountpoint "$mount_point" esp.img)"
    disk="$(printf '%s\n' "$attach_output" | awk '$1 ~ /^\/dev\/disk/ { print $1; exit }')"
    cleanup_esp() {
        if [ -n "$disk" ]; then
            hdiutil detach "$disk" >/dev/null 2>&1 || true
        fi
        rmdir "$mount_point" 2>/dev/null || true
    }
    trap cleanup_esp EXIT INT TERM

    cp "$ROOT_DIR/target/aarch64-unknown-uefi/debug/BOOTAA64.efi" \
        "$mount_point/EFI/BOOT/BOOTAA64.EFI"
    sync
    cleanup_esp
    trap - EXIT INT TERM
    echo "==> 已更新 qemu/esp.img 中的 BOOTAA64.EFI"
}

echo "=== Exokernel QEMU Test ==="
echo ""

# 确保 ESP 存在
if [ ! -f esp.img ]; then
    echo "ERROR: esp.img not found. Please create qemu/esp.img first."
    exit 1
fi

sync_efi_to_esp

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

USB_SOCKET=/tmp/exokernel-usb.sock
rm -f "$USB_SOCKET"
QEMU_USB_MODE="${QEMU_USB_MODE:-host}"
QEMU_USB_ARGS=()
QEMU_LAUNCH=(qemu-system-aarch64)
BRIDGE_PID=""
BRIDGE_SOCKET=""

cleanup_usb_bridge() {
  if [ -n "$BRIDGE_PID" ]; then
    kill "$BRIDGE_PID" >/dev/null 2>&1 || true
    wait "$BRIDGE_PID" >/dev/null 2>&1 || true
  fi
  if [ -n "$BRIDGE_SOCKET" ]; then
    rm -f "$BRIDGE_SOCKET"
  fi
}
trap cleanup_usb_bridge EXIT INT TERM

if [ "${QEMU_SUDO:-0}" = "1" ]; then
  source "$ROOT_DIR/scripts/sudo_keychain.sh"
  sudo_keychain_prepare
  echo "QEMU launch: sudo qemu-system-aarch64 (Keychain ticket)"
  QEMU_LAUNCH=(sudo qemu-system-aarch64)
fi
if [ "$QEMU_USB_MODE" = "none" ]; then
  echo "USB topology: none (Frame/VSpace or UART-only test)"
elif [ "$QEMU_USB_MODE" = "ftdi" ] || [ "$QEMU_USB_MODE" = "hub-ftdi" ]; then
  echo "USB app must be built with: LIBOS_FEATURES=qemu-xhci,usb-echo"
  echo "FTDI socket: $USB_SOCKET"
  echo "Connect with: nc -U $USB_SOCKET"
  QEMU_USB_ARGS+=(
    -chardev "socket,id=usbserial,path=$USB_SOCKET,server=on,wait=off"
    -device "qemu-xhci,id=xhci,addr=02.0,msi=off,msix=off"
  )
  if [ "$QEMU_USB_MODE" = "hub-ftdi" ]; then
    echo "USB topology: xHCI -> emulated USB 2.0 Hub -> emulated FTDI"
    QEMU_USB_ARGS+=(
      -device "usb-hub,id=hub,bus=xhci.0,port=1,ports=8"
      -device "usb-serial,bus=xhci.0,port=1.1,chardev=usbserial,always-plugged=on"
    )
  else
    QEMU_USB_ARGS+=(
      -device "usb-serial,bus=xhci.0,chardev=usbserial,always-plugged=on"
    )
  fi
elif [ "$QEMU_USB_MODE" = "serial-bridge" ]; then
  SERIAL_PATH="${QEMU_SERIAL_PATH:-/dev/cu.usbmodem5A7C1191771}"
  BRIDGE_SOCKET="${QEMU_SERIAL_BRIDGE_SOCKET:-/tmp/exokernel-scservo-bridge.sock}"
  if [ ! -e "$SERIAL_PATH" ]; then
    echo "ERROR: serial bridge path does not exist: $SERIAL_PATH" >&2
    exit 1
  fi

  HOST_CARGO="${HOST_CARGO:-$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo}"
  HOST_RUSTC="${HOST_RUSTC:-$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/rustc}"
  if [ ! -x "$HOST_CARGO" ]; then
    HOST_CARGO="$(command -v cargo)"
  fi
  rm -f "$BRIDGE_SOCKET"
  RUSTC="$HOST_RUSTC" "$HOST_CARGO" build --quiet \
    --manifest-path "$ROOT_DIR/tools/scservo-serial-bridge/Cargo.toml"
  "$ROOT_DIR/tools/scservo-serial-bridge/target/debug/scservo-serial-bridge" \
    --port "$SERIAL_PATH" --socket "$BRIDGE_SOCKET" &
  BRIDGE_PID=$!
  for _ in $(seq 1 100); do
    [ -S "$BRIDGE_SOCKET" ] && break
    if ! kill -0 "$BRIDGE_PID" >/dev/null 2>&1; then
      echo "ERROR: serial bridge exited before creating its socket." >&2
      exit 1
    fi
    sleep 0.05
  done
  if [ ! -S "$BRIDGE_SOCKET" ]; then
    echo "ERROR: serial bridge socket was not created." >&2
    exit 1
  fi

  QEMU_USB_ARGS+=(
    -chardev "socket,id=scservo,path=$BRIDGE_SOCKET"
    -device "qemu-xhci,id=xhci,addr=02.0,msi=off,msix=off"
    -device "usb-serial,bus=xhci.0,chardev=scservo,always-plugged=on"
  )
  echo "USB serial bridge: EL0 -> QEMU FTDI -> $BRIDGE_SOCKET -> $SERIAL_PATH"
elif [ "$QEMU_USB_MODE" = "host" ] || [ "$QEMU_USB_MODE" = "hub-host" ]; then
  QEMU_USB_VENDOR_ID="${QEMU_USB_VENDOR_ID:-0x1a86}"
  QEMU_USB_PRODUCT_ID="${QEMU_USB_PRODUCT_ID:-0x55d3}"
  # 两个 CH34x CDC ACM 转接器可能具有相同 VID/PID，必须用 serial
  # 选择实际连接 SO101 舵机总线的设备。
  QEMU_USB_SERIAL="${QEMU_USB_SERIAL:-5A7C119177}"
  QEMU_USB_PCAP="${QEMU_USB_PCAP:-/tmp/exokernel-usb.pcap}"
  echo "USB host passthrough: VID=$QEMU_USB_VENDOR_ID PID=$QEMU_USB_PRODUCT_ID"
  echo "The physical USB device must be a CDC ACM device."
  # SCServo 是严格的半双工请求/应答协议。关闭 QEMU usb-host 的 Bulk
  # pipeline，保证物理 libusb 后端按 guest 提交顺序推进 IN/OUT；同时避免
  # macOS 上 guest reset 让 CDC 设备短暂消失或丢失线路配置。
  USB_HOST_DEVICE="usb-host,bus=xhci.0,vendorid=$QEMU_USB_VENDOR_ID,productid=$QEMU_USB_PRODUCT_ID,pipeline=off,guest-reset=off"
  if [ -n "${QEMU_USB_SERIAL:-}" ]; then
    echo "USB host serial filter: $QEMU_USB_SERIAL"
    USB_HOST_DEVICE+=",serial=$QEMU_USB_SERIAL"
  fi
  if [ -n "$QEMU_USB_PCAP" ]; then
    if [ "${QEMU_SUDO:-0}" = "1" ]; then
      sudo rm -f "$QEMU_USB_PCAP"
    else
      rm -f "$QEMU_USB_PCAP"
    fi
    echo "USB packet capture: $QEMU_USB_PCAP"
    USB_HOST_DEVICE+=",pcap=$QEMU_USB_PCAP"
  fi
  QEMU_USB_ARGS+=(
    -device "qemu-xhci,id=xhci,addr=02.0,msi=off,msix=off"
  )
  if [ "$QEMU_USB_MODE" = "hub-host" ]; then
    echo "USB topology: xHCI -> emulated USB 2.0 Hub -> physical USB device"
    # Hub class 驱动必须执行下游端口 reset。macOS 上如果同时让 QEMU 对
    # 真实设备执行 libusb reset，设备可能短暂消失并导致 Hub 端口无法
    # 重新 enable；关闭物理 reset 不影响 guest 内的 Hub 端口状态机。
    USB_HOST_DEVICE+=",port=1.1"
    QEMU_USB_ARGS+=(
      -device "usb-hub,id=hub,bus=xhci.0,port=1,ports=8"
      -device "$USB_HOST_DEVICE"
    )
  else
    QEMU_USB_ARGS+=(
      -device "$USB_HOST_DEVICE"
    )
  fi
else
  echo "ERROR: QEMU_USB_MODE must be none, ftdi, hub-ftdi, serial-bridge, host, or hub-host" >&2
  exit 1
fi

echo ""
echo "QEMU command:"
echo "  qemu-system-aarch64 -M $QEMU_MACHINE -cpu $QEMU_CPU -m $QEMU_MEMORY ..."
echo ""


"${QEMU_LAUNCH[@]}" \
  -M "$QEMU_MACHINE" -cpu "$QEMU_CPU" -m "$QEMU_MEMORY" \
  -drive if=pflash,format=raw,unit=0,file="$QEMU_CODE",readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$VARS" \
  -drive file=esp.img,format=raw,if=none,id=drive0 \
  -device virtio-blk-device,drive=drive0 \
  "${QEMU_USB_ARGS[@]}" \
  -serial stdio -monitor none -display none \
  -nographic -no-reboot
