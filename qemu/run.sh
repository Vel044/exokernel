#!/bin/bash
set -e
cd "$(dirname "$0")"
source ./config.sh

ROOT_DIR="$(cd .. && pwd)"
if [ -n "${LIBOS_FEATURES:-}" ]; then
  echo "ERROR: LIBOS_FEATURES已不再是公开接口，请改用LIBOS_APP。" >&2
  exit 1
fi
LIBOS_APP="${LIBOS_APP:-scservo}"

# 纯ACT基准在Apple Silicon上优先使用Hypervisor Framework原生执行AArch64。
# 真实机器人闭环仍需TCG：当前QEMU HVF在EL0直接访问PCI ECAM/xHCI MMIO时
# 会触发hvf_handle_exception断言，不能承载本项目的用户态设备驱动。
if [ -z "${QEMU_ACCEL:-}" ]; then
  if [ "$LIBOS_APP" = "act-benchmark" ] && [ "$(uname -s)" = "Darwin" ]; then
    QEMU_ACCEL=hvf
  else
    QEMU_ACCEL=tcg
  fi
fi
if [ "$LIBOS_APP" = "robot-act-once" ] && [ "$QEMU_ACCEL" = "hvf" ]; then
  echo "ERROR: robot-act-once不能使用HVF：EL0 PCI/xHCI MMIO会触发QEMU HVF断言。" >&2
  echo "请使用TCG单实例闭环，或使用后续的TCG采集 -> HVF推理 -> TCG执行编排。" >&2
  exit 1
fi
if [ "$QEMU_ACCEL" = "hvf" ]; then
  QEMU_RUN_CPU=host
  # HVF不能向guest暴露嵌套EL2。UEFI直接在EL1启动时，BOOTAA64.EFI会
  # 跳过仅负责降级的EL2 shim，后续EL1 Kernel与EL0 libOS路径完全相同。
  QEMU_RUN_MACHINE="${QEMU_MACHINE%,virtualization=on}"
else
  QEMU_RUN_CPU="$QEMU_CPU"
  QEMU_RUN_MACHINE="$QEMU_MACHINE"
fi

# 每个场景都有唯一的默认设备拓扑。只有system-smoke允许通过ftdi拓扑
# 显式扩展为带xHCI的综合测试，其余场景不产生额外编译组合。
case "$LIBOS_APP" in
  scservo|scservo-move|robot-action-replay) DEFAULT_USB_MODE=host ;;
  usb-echo) DEFAULT_USB_MODE=ftdi ;;
  uvc-smoke) DEFAULT_USB_MODE=uvc-host ;;
  system-smoke|process-smoke|uart-echo|act-inference|act-benchmark) DEFAULT_USB_MODE=none ;;
  robot-act-once|robot-observation) DEFAULT_USB_MODE=robot-host ;;
  *)
    echo "ERROR: unknown LIBOS_APP: $LIBOS_APP" >&2
    exit 1
    ;;
esac
QEMU_USB_MODE="${QEMU_USB_MODE:-$DEFAULT_USB_MODE}"
# 三台真实USB设备在macOS上需要libusb脱离宿主驱动。机器人闭环默认复用
# Keychain askpass取得sudo ticket；调用者仍可显式设置QEMU_SUDO=0做诊断。
if { [ "$LIBOS_APP" = "robot-act-once" ] || [ "$LIBOS_APP" = "robot-observation" ] \
    || [ "$LIBOS_APP" = "robot-action-replay" ]; } && [ -z "${QEMU_SUDO+x}" ]; then
  QEMU_SUDO=1
fi
LIBOS_ENABLE_XHCI=0
if [ "$LIBOS_APP" = "system-smoke" ] && { [ "$QEMU_USB_MODE" = "ftdi" ] || [ "$QEMU_USB_MODE" = "hub-ftdi" ]; }; then
  LIBOS_ENABLE_XHCI=1
fi
case "$LIBOS_APP" in
  system-smoke)
    if [ "$QEMU_USB_MODE" != "none" ] && [ "$QEMU_USB_MODE" != "ftdi" ] && [ "$QEMU_USB_MODE" != "hub-ftdi" ]; then
      echo "ERROR: system-smoke只支持none、ftdi或hub-ftdi拓扑" >&2
      exit 1
    fi
    ;;
  usb-echo)
    if [ "$QEMU_USB_MODE" != "ftdi" ] && [ "$QEMU_USB_MODE" != "hub-ftdi" ]; then
      echo "ERROR: usb-echo只支持ftdi或hub-ftdi拓扑" >&2
      exit 1
    fi
    ;;
  scservo|scservo-move|robot-action-replay)
    if [ "$QEMU_USB_MODE" != "host" ] && [ "$QEMU_USB_MODE" != "hub-host" ] && [ "$QEMU_USB_MODE" != "serial-bridge" ]; then
      echo "ERROR: SCServo场景只支持host、hub-host或serial-bridge拓扑" >&2
      exit 1
    fi
    ;;
  uvc-smoke)
    if [ "$QEMU_USB_MODE" != "uvc-host" ]; then
      echo "ERROR: uvc-smoke只支持uvc-host拓扑" >&2
      exit 1
    fi
    ;;
  robot-act-once|robot-observation)
    if [ "$QEMU_USB_MODE" != "robot-host" ]; then
      echo "ERROR: robot-act-once只支持robot-host拓扑" >&2
      exit 1
    fi
    ;;
  *)
    if [ "$QEMU_USB_MODE" != "none" ]; then
      echo "ERROR: $LIBOS_APP场景不使用USB，QEMU_USB_MODE必须为none" >&2
      exit 1
    fi
    ;;
esac
export LIBOS_APP LIBOS_ENABLE_XHCI

# 同一个esp.img不能被两个本项目实例同时写入。只检查当前镜像的打开者，
# 避免把机器上运行的其他、互不相关的QEMU虚拟机误判为冲突。
EXISTING_QEMU="$(lsof -t "$PWD/esp.img" 2>/dev/null || true)"
if [ -n "$EXISTING_QEMU" ]; then
  echo "ERROR: exokernel esp.img is already in use (PID: $(echo "$EXISTING_QEMU" | tr '\n' ' '))" >&2
  echo "Stop the old exokernel QEMU instance before running this script again." >&2
  exit 1
fi

if [ "${EXOKERNEL_SKIP_BUILD:-0}" = "1" ]; then
  echo "==> 跳过编译，使用已有 Kernel/libOS 产物（EXOKERNEL_SKIP_BUILD=1）"
else
  echo "==> 编译 Kernel + libOS"
  (cd "$ROOT_DIR" && ./build.sh)
fi

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
MONITOR_SOCKET=/tmp/exokernel-monitor.sock
rm -f "$USB_SOCKET"
rm -f "$MONITOR_SOCKET"
QEMU_USB_ARGS=()
QEMU_MODEL_ARGS=()
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
  if [ "$LIBOS_APP" != "usb-echo" ] && [ "$LIBOS_APP" != "system-smoke" ]; then
    echo "ERROR: $QEMU_USB_MODE拓扑只适用于LIBOS_APP=usb-echo或system-smoke" >&2
    exit 1
  fi
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
  if [ "$LIBOS_APP" != "scservo" ] && [ "$LIBOS_APP" != "scservo-move" ]; then
    echo "ERROR: serial-bridge拓扑只适用于SCServo场景" >&2
    exit 1
  fi
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
elif [ "$QEMU_USB_MODE" = "robot-host" ]; then
  if [ "$LIBOS_APP" != "robot-act-once" ] && [ "$LIBOS_APP" != "robot-observation" ]; then
    echo "ERROR: robot-host拓扑只适用于机器人观测或闭环场景" >&2
    exit 1
  fi
  if [[ "${OSTYPE:-}" != darwin* ]]; then
    echo "ERROR: 当前robot-host只实现了macOS ioreg设备定位。" >&2
    exit 1
  fi
  # ioreg中的locationID是macOS USB拓扑稳定字段；USB Address会随插拔变化，
  # 因此每次启动动态查询，不把11/12写死进QEMU参数。
  usb_address_for_location() {
    local target="$1"
    ioreg -p IOUSB -l -w 0 | awk -v target="$target" '
      /"locationID" =/ {
        split($0, value, "= ");
        hit = (value[2] + 0 == target + 0)
      }
      hit && /"USB Address" =/ {
        split($0, value, "= ");
        print value[2] + 0;
        exit
      }
    '
  }
  usb_address_for_serial() {
    local target="$1"
    ioreg -p IOUSB -l -w 0 | awk -v target="$target" '
      /"USB Serial Number" =/ {
        split($0, value, "= ");
        hit = (value[2] == "\"" target "\"")
      }
      hit && /"USB Address" =/ {
        split($0, value, "= ");
        print value[2] + 0;
        exit
      }
    '
  }
  HAND_EYE_LOCATION="${QEMU_HAND_EYE_LOCATION:-0x121000}"
  FIXED_LOCATION="${QEMU_FIXED_LOCATION:-0x122000}"
  HAND_EYE_LOCATION_DEC=$((HAND_EYE_LOCATION))
  FIXED_LOCATION_DEC=$((FIXED_LOCATION))
  HAND_EYE_BUS="${QEMU_HAND_EYE_HOSTBUS:-0}"
  FIXED_BUS="${QEMU_FIXED_HOSTBUS:-0}"
  HAND_EYE_ADDR="${QEMU_HAND_EYE_HOSTADDR:-$(usb_address_for_location "$HAND_EYE_LOCATION_DEC")}" 
  FIXED_ADDR="${QEMU_FIXED_HOSTADDR:-$(usb_address_for_location "$FIXED_LOCATION_DEC")}" 
  QEMU_USB_SERIAL="${QEMU_USB_SERIAL:-5A7C119177}"
  SCSERVO_BUS="${QEMU_SCSERVO_HOSTBUS:-0}"
  SCSERVO_ADDR="${QEMU_SCSERVO_HOSTADDR:-$(usb_address_for_serial "$QEMU_USB_SERIAL")}" 
  if [ -z "$HAND_EYE_ADDR" ] || [ -z "$FIXED_ADDR" ] || [ -z "$SCSERVO_ADDR" ]; then
    echo "ERROR: 找不到机器人USB设备；handeye location=$HAND_EYE_LOCATION fixed location=$FIXED_LOCATION SCServo serial=$QEMU_USB_SERIAL" >&2
    echo "可用 ioreg -p IOUSB -l -w 0 检查locationID、USB Serial Number和USB Address。" >&2
    exit 1
  fi
  echo "USB topology: connector1/root-port5 handeye=$HAND_EYE_BUS:$HAND_EYE_ADDR (location=$HAND_EYE_LOCATION)"
  echo "USB topology: connector2/root-port6 fixed=$FIXED_BUS:$FIXED_ADDR (location=$FIXED_LOCATION)"
  echo "USB topology: connector3/root-port7 SCServo=$SCSERVO_BUS:$SCSERVO_ADDR serial=$QEMU_USB_SERIAL"
  QEMU_USB_ARGS+=(
    # qemu-xhci先提供4个USB3端口，再提供USB2 companion端口。三个USB2
    # connector在guest中对应xHCI root port 5/6/7；应用按该硬件编号识别角色。
    -device "qemu-xhci,id=xhci,addr=02.0,p2=3,msi=off,msix=off"
    -device "usb-host,bus=xhci.0,port=1,hostbus=$HAND_EYE_BUS,hostaddr=$HAND_EYE_ADDR,pipeline=off,guest-reset=off"
    -device "usb-host,bus=xhci.0,port=2,hostbus=$FIXED_BUS,hostaddr=$FIXED_ADDR,pipeline=off,guest-reset=off"
    -device "usb-host,bus=xhci.0,port=3,hostbus=$SCSERVO_BUS,hostaddr=$SCSERVO_ADDR,pipeline=off,guest-reset=off"
  )
elif [ "$QEMU_USB_MODE" = "host" ] || [ "$QEMU_USB_MODE" = "hub-host" ] || [ "$QEMU_USB_MODE" = "uvc-host" ]; then
  if [ "$QEMU_USB_MODE" = "uvc-host" ]; then
    if [ "$LIBOS_APP" != "uvc-smoke" ]; then
      echo "ERROR: uvc-host拓扑只适用于LIBOS_APP=uvc-smoke" >&2
      exit 1
    fi
  elif [ "$LIBOS_APP" != "scservo" ] && [ "$LIBOS_APP" != "scservo-move" ] \
      && [ "$LIBOS_APP" != "robot-action-replay" ]; then
    echo "ERROR: USB直通拓扑只适用于SCServo场景" >&2
    exit 1
  fi
  if [ "$QEMU_USB_MODE" = "uvc-host" ]; then
    QEMU_USB_VENDOR_ID="${QEMU_USB_VENDOR_ID:-0x1bcf}"
    QEMU_USB_PRODUCT_ID="${QEMU_USB_PRODUCT_ID:-0x2281}"
    QEMU_USB_SERIAL="${QEMU_USB_SERIAL:-}"
  else
    QEMU_USB_VENDOR_ID="${QEMU_USB_VENDOR_ID:-0x1a86}"
    QEMU_USB_PRODUCT_ID="${QEMU_USB_PRODUCT_ID:-0x55d3}"
    # 两个 CH34x CDC ACM 转接器可能具有相同 VID/PID，必须用 serial
    # 选择实际连接 SO101 舵机总线的设备。
    QEMU_USB_SERIAL="${QEMU_USB_SERIAL:-5A7C119177}"
  fi
  # 文件名包含调用者UID，避免不同用户或sudo/non-sudo运行互相继承权限。
  # 启动QEMU前先以普通用户身份创建文件；随后即使root QEMU执行O_TRUNC，
  # 现有inode的所有者仍是当前用户，下一次普通运行也可以直接删除。
  QEMU_USB_PCAP="${QEMU_USB_PCAP:-/tmp/exokernel-usb-${UID}.pcap}"
  echo "USB host passthrough: VID=$QEMU_USB_VENDOR_ID PID=$QEMU_USB_PRODUCT_ID"
  if [ "$QEMU_USB_MODE" = "uvc-host" ]; then
    echo "The physical USB device must expose UVC VideoControl/VideoStreaming interfaces."
  else
    echo "The physical USB device must be a CDC ACM device."
  fi
  # SCServo 是严格的半双工请求/应答协议。关闭 QEMU usb-host 的 Bulk
  # pipeline，保证物理 libusb 后端按 guest 提交顺序推进 IN/OUT；同时避免
  # macOS 上 guest reset 让 CDC 设备短暂消失或丢失线路配置。
  USB_HOST_DEVICE="usb-host,bus=xhci.0,vendorid=$QEMU_USB_VENDOR_ID,productid=$QEMU_USB_PRODUCT_ID,pipeline=off,guest-reset=off"
  # 两台无serial的同型号摄像头必须按libusb bus/address精确选择。当前Mac上
  # `info usbhost`显示Bus 0；默认取地址8，可通过环境变量切换另一台。
  if [ -n "${QEMU_USB_HOSTBUS:-}" ] || [ -n "${QEMU_USB_HOSTADDR:-}" ]; then
    if [ -z "${QEMU_USB_HOSTBUS:-}" ] || [ -z "${QEMU_USB_HOSTADDR:-}" ]; then
      echo "ERROR: QEMU_USB_HOSTBUS和QEMU_USB_HOSTADDR必须同时设置" >&2
      exit 1
    fi
    echo "USB host bus/address filter: ${QEMU_USB_HOSTBUS}:${QEMU_USB_HOSTADDR}"
    USB_HOST_DEVICE+=",hostbus=${QEMU_USB_HOSTBUS},hostaddr=${QEMU_USB_HOSTADDR}"
  fi
  if [ -n "${QEMU_USB_SERIAL:-}" ]; then
    echo "USB host serial filter: $QEMU_USB_SERIAL"
    USB_HOST_DEVICE+=",serial=$QEMU_USB_SERIAL"
  fi
  if [ -n "$QEMU_USB_PCAP" ]; then
    rm -f "$QEMU_USB_PCAP"
    : > "$QEMU_USB_PCAP"
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
  echo "ERROR: QEMU_USB_MODE must be none, ftdi, hub-ftdi, serial-bridge, host, hub-host, uvc-host, or robot-host" >&2
  exit 1
fi

if [ "$LIBOS_APP" = "robot-action-replay" ]; then
  ACTION_FILE="${ACT_ACTION_FILE:-$ROOT_DIR/qemu/robot-actions.f32le}"
  ACTION_FS_IMAGE="${ACT_ACTION_FS_IMAGE:-$PWD/robot-actions.ext4}"
  ACT_ACTION_FILE="$ACTION_FILE" ACT_ACTION_FS_IMAGE="$ACTION_FS_IMAGE" \
    bash "$ROOT_DIR/scripts/build_robot_actions_ext4.sh"
  echo "ACT action volume: $ACTION_FS_IMAGE (read-only ext4)"
  QEMU_MODEL_ARGS+=(
    -drive "file=$ACTION_FS_IMAGE,format=raw,if=none,id=actions,readonly=on"
    -device "virtio-blk-device,drive=actions,bootindex=1"
  )
fi

# ACT权重独立于64MiB ESP：脚本生成ext4镜像并挂成只读virtio-blk。
# Kernel只授权virtio-mmio窗口，路径解析与文件读取全部由EL0 libOS完成。
if [ "$LIBOS_APP" = "act-inference" ] || [ "$LIBOS_APP" = "act-benchmark" ] || [ "$LIBOS_APP" = "robot-act-once" ]; then
  WORKSPACE_ROOT="$(cd "$ROOT_DIR/.." && pwd)"
  ACT_MODEL_DIR="${ACT_MODEL_DIR:-$WORKSPACE_ROOT/data/act/model}"
  ACT_CASES_DIR="${ACT_CASES_DIR:-$WORKSPACE_ROOT/data/act/correctness}"
  ACT_MODEL_FILE="$ACT_MODEL_DIR/model.safetensors"
  ACT_STATS_FILE="$ACT_MODEL_DIR/policy_preprocessor_step_3_normalizer_processor.safetensors"
  if [ ! -f "$ACT_MODEL_FILE" ] || [ ! -f "$ACT_STATS_FILE" ]; then
    echo "ERROR: ACT model files are missing from $ACT_MODEL_DIR" >&2
    echo "Run: bash $ROOT_DIR/scripts/fetch_act_model.sh" >&2
    exit 1
  fi
  if [ "$LIBOS_APP" = "robot-act-once" ]; then
    # 机器人闭环镜像与ACT正确性实验分开，避免复用旧镜像时把测试样本
    # 或其他文件带进真实运动场景；该镜像只允许包含模型和normalizer。
    ACT_FS_IMAGE="${ACT_FS_IMAGE:-$PWD/robot-act-model.ext4}"
    ACT_MODEL_DIR="$ACT_MODEL_DIR" ACT_FS_IMAGE="$ACT_FS_IMAGE" \
      bash "$ROOT_DIR/scripts/build_robot_act_ext4.sh"
  elif [ "$LIBOS_APP" = "act-benchmark" ]; then
    ACT_FS_IMAGE="${ACT_FS_IMAGE:-$PWD/act-benchmark.ext4}"
    ACT_MODEL_DIR="$ACT_MODEL_DIR" ACT_OBSERVATION_DIR="${ACT_OBSERVATION_DIR:-$WORKSPACE_ROOT/data/act/current-observation}" ACT_FS_IMAGE="$ACT_FS_IMAGE" \
      bash "$ROOT_DIR/scripts/build_act_benchmark_ext4.sh"
  else
    ACT_FS_IMAGE="${ACT_FS_IMAGE:-$PWD/act-model.ext4}"
    ACT_MODEL_DIR="$ACT_MODEL_DIR" ACT_CASES_DIR="$ACT_CASES_DIR" ACT_FS_IMAGE="$ACT_FS_IMAGE" \
      bash "$ROOT_DIR/scripts/build_act_ext4.sh"
  fi
  echo "ACT model volume: $ACT_FS_IMAGE (read-only ext4)"
  QEMU_MODEL_ARGS+=(
    -drive "file=$ACT_FS_IMAGE,format=raw,if=none,id=actmodel,readonly=on"
    -device "virtio-blk-device,drive=actmodel,bootindex=1"
  )
fi

# UVC smoke把单帧写入用户态ext4。该盘必须可写；Kernel只授权virtio-mmio，
# 文件定位和扇区写入都由EL0完成。
if [ "$LIBOS_APP" = "uvc-smoke" ]; then
  UVC_FS_IMAGE="${UVC_FS_IMAGE:-$PWD/uvc-capture.ext4}"
  UVC_FS_IMAGE="$UVC_FS_IMAGE" bash "$ROOT_DIR/scripts/build_uvc_ext4.sh"
  echo "UVC capture volume: $UVC_FS_IMAGE (writable ext4)"
  echo "After QEMU exits: UVC_FS_IMAGE=$UVC_FS_IMAGE bash $ROOT_DIR/scripts/extract_uvc_frame.sh"
  QEMU_MODEL_ARGS+=(
    -drive "file=$UVC_FS_IMAGE,format=raw,if=none,id=uvccapture"
    -device "virtio-blk-device,drive=uvccapture,bootindex=1"
  )
fi

# 固定观测盘同时保存两张MJPEG和六轴位置，供后续纯推理基准重复使用。
if [ "$LIBOS_APP" = "robot-observation" ]; then
  OBSERVATION_FS_IMAGE="${OBSERVATION_FS_IMAGE:-$PWD/robot-observation.ext4}"
  OBSERVATION_FS_IMAGE="$OBSERVATION_FS_IMAGE" bash "$ROOT_DIR/scripts/build_robot_observation_ext4.sh"
  echo "Robot observation volume: $OBSERVATION_FS_IMAGE (writable ext4)"
  echo "After QEMU exits: OBSERVATION_FS_IMAGE=$OBSERVATION_FS_IMAGE bash $ROOT_DIR/scripts/extract_robot_observation.sh"
  QEMU_MODEL_ARGS+=(
    -drive "file=$OBSERVATION_FS_IMAGE,format=raw,if=none,id=observation"
    -device "virtio-blk-device,drive=observation,bootindex=1"
  )
fi

echo ""
echo "QEMU command:"
echo "  qemu-system-aarch64 -accel $QEMU_ACCEL -M $QEMU_RUN_MACHINE -cpu $QEMU_RUN_CPU -m $QEMU_MEMORY ..."
echo ""


"${QEMU_LAUNCH[@]}" \
  -accel "$QEMU_ACCEL" -M "$QEMU_RUN_MACHINE" -cpu "$QEMU_RUN_CPU" -smp 4 -m "$QEMU_MEMORY" \
  -drive if=pflash,format=raw,unit=0,file="$QEMU_CODE",readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$VARS" \
  -drive file=esp.img,format=raw,if=none,id=drive0 \
  -device virtio-blk-device,drive=drive0,bootindex=0 \
  "${QEMU_MODEL_ARGS[@]}" \
  "${QEMU_USB_ARGS[@]}" \
  -serial stdio -monitor "unix:$MONITOR_SOCKET,server=on,wait=off" -display none \
  -nographic -no-reboot
