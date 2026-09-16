#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT="${OBSERVATION_FS_IMAGE:-$ROOT_DIR/qemu/robot-observation.ext4}"
CAPACITY=$((4096 + 2 * 2 * 1024 * 1024))
MKE2FS="${MKE2FS:-/opt/homebrew/opt/e2fsprogs/sbin/mke2fs}"
DEBUGFS="${DEBUGFS:-/opt/homebrew/opt/e2fsprogs/sbin/debugfs}"

if [ ! -x "$MKE2FS" ] || [ ! -x "$DEBUGFS" ]; then
    echo "ERROR: 找不到e2fsprogs；macOS可执行 brew install e2fsprogs" >&2
    exit 1
fi

mkdir -p "$(dirname "$OUTPUT")"
STAGING="$(mktemp -d /tmp/exokernel-observation.XXXXXX)"
TEMP="$OUTPUT.tmp.$$"
trap 'rm -rf "$STAGING"; rm -f "$TEMP"' EXIT INT TERM

python3 - "$STAGING/observation.bin" "$CAPACITY" <<'PY'
import sys
path, capacity = sys.argv[1], int(sys.argv[2])
with open(path, "wb") as output:
    chunk = b"\xff" * (64 * 1024)
    for _ in range(capacity // len(chunk)):
        output.write(chunk)
    output.write(chunk[:capacity % len(chunk)])
PY

qemu-img create -q -f raw "$TEMP" 16M
"$MKE2FS" -q -F -t ext4 -b 4096 -m 0 -L ACTOBS -d "$STAGING" "$TEMP"
EXTENTS="$("$DEBUGFS" -R 'stat /observation.bin' "$TEMP" 2>/dev/null | awk '/^EXTENTS:/{getline; print; exit}')"
if [[ ! "$EXTENTS" =~ \(0-([0-9]+)\):([0-9]+)-([0-9]+) ]]; then
    echo "ERROR: observation.bin不是单一连续extent: $EXTENTS" >&2
    exit 1
fi
FIRST_BLOCK="${BASH_REMATCH[2]}"
EXPECTED_BLOCKS=$((CAPACITY / 4096))
if [ $((BASH_REMATCH[1] + 1)) -ne "$EXPECTED_BLOCKS" ]; then
    echo "ERROR: observation.bin容量不匹配" >&2
    exit 1
fi

python3 - "$TEMP" "$FIRST_BLOCK" "$CAPACITY" <<'PY'
import struct, sys
path, first_block, capacity = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
metadata = bytearray(512)
metadata[:8] = b"EXOOBS01"
struct.pack_into("<Q", metadata, 8, first_block * 4096 // 512)
struct.pack_into("<Q", metadata, 16, capacity)
with open(path, "r+b") as image:
    image.write(metadata)
PY

mv "$TEMP" "$OUTPUT"
rm -rf "$STAGING"
trap - EXIT INT TERM
echo "==> 创建机器人观测ext4: $OUTPUT"
echo "    /observation.bin capacity=$CAPACITY start_block=$FIRST_BLOCK"
