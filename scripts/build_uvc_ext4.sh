#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT="${UVC_FS_IMAGE:-$ROOT_DIR/qemu/uvc-capture.ext4}"
CAPACITY="${UVC_CAPTURE_CAPACITY:-2097152}"

MKE2FS="${MKE2FS:-/opt/homebrew/opt/e2fsprogs/sbin/mke2fs}"
DEBUGFS="${DEBUGFS:-/opt/homebrew/opt/e2fsprogs/sbin/debugfs}"
if [ ! -x "$MKE2FS" ] || [ ! -x "$DEBUGFS" ]; then
    echo "ERROR: 找不到e2fsprogs；macOS可执行 brew install e2fsprogs" >&2
    exit 1
fi
if [ $((CAPACITY % 4096)) -ne 0 ]; then
    echo "ERROR: UVC_CAPTURE_CAPACITY必须按4096字节对齐" >&2
    exit 1
fi

mkdir -p "$(dirname "$OUTPUT")"
STAGING="$(mktemp -d /tmp/exokernel-uvc-ext4.XXXXXX)"
TEMP="$OUTPUT.tmp.$$"
trap 'rm -rf "$STAGING"; rm -f "$TEMP"' EXIT INT TERM

# 文件必须含非零字节，否则mke2fs会把全零文件做成稀疏inode而不分配extent。
# 预填0xff只发生在宿主构建阶段；EL0会从文件开头覆盖真实MJPEG数据。
python3 - "$STAGING/uvc-frame.mjpg" "$CAPACITY" <<'PY'
import sys

path, capacity = sys.argv[1], int(sys.argv[2])
chunk = b"\xff" * (64 * 1024)
with open(path, "wb") as output:
    for _ in range(capacity // len(chunk)):
        output.write(chunk)
    output.write(chunk[: capacity % len(chunk)])
PY
qemu-img create -q -f raw "$TEMP" 16M
"$MKE2FS" -q -F -t ext4 -b 4096 -m 0 -L UVCCAP -d "$STAGING" "$TEMP"

EXTENTS="$("$DEBUGFS" -R 'stat /uvc-frame.mjpg' "$TEMP" 2>/dev/null |
    awk '/^EXTENTS:/{getline; print; exit}')"
# 预分配文件必须是一个连续extent，例如“(0-511):1172-1683”。
if [[ ! "$EXTENTS" =~ \(0-([0-9]+)\):([0-9]+)-([0-9]+) ]]; then
    echo "ERROR: 无法获得/uvc-frame.mjpg连续extent: $EXTENTS" >&2
    exit 1
fi
LAST_LOGICAL="${BASH_REMATCH[1]}"
FIRST_BLOCK="${BASH_REMATCH[2]}"
LAST_BLOCK="${BASH_REMATCH[3]}"
EXPECTED_BLOCKS=$((CAPACITY / 4096))
if [ $((LAST_LOGICAL + 1)) -ne "$EXPECTED_BLOCKS" ] ||
   [ $((LAST_BLOCK - FIRST_BLOCK + 1)) -ne "$EXPECTED_BLOCKS" ]; then
    echo "ERROR: 捕获文件extent不连续或容量不匹配: $EXTENTS" >&2
    exit 1
fi

# ext4保留前1024字节作为boot area。第0扇区保存实验私有定位元数据：
# magic[8], start_lba(u64), capacity(u64), valid_length(u64)。
python3 - "$TEMP" "$FIRST_BLOCK" "$CAPACITY" <<'PY'
import struct
import sys

path, first_block, capacity = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
metadata = bytearray(512)
metadata[:8] = b"EXOUVC01"
struct.pack_into("<Q", metadata, 8, first_block * 4096 // 512)
struct.pack_into("<Q", metadata, 16, capacity)
struct.pack_into("<Q", metadata, 24, 0)
with open(path, "r+b") as image:
    image.write(metadata)
PY

mv "$TEMP" "$OUTPUT"
trap - EXIT INT TERM
rm -rf "$STAGING"
echo "==> 创建UVC ext4镜像: $OUTPUT"
echo "    /uvc-frame.mjpg capacity=$CAPACITY start_block=$FIRST_BLOCK"
