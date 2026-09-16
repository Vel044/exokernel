#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
INPUT="${ACT_ACTION_FILE:-$ROOT_DIR/qemu/robot-actions.f32le}"
OUTPUT="${ACT_ACTION_FS_IMAGE:-$ROOT_DIR/qemu/robot-actions.ext4}"
MKE2FS="${MKE2FS:-/opt/homebrew/opt/e2fsprogs/sbin/mke2fs}"

if [ ! -f "$INPUT" ] || [ "$(wc -c < "$INPUT" | tr -d ' ')" != "2400" ]; then
  echo "ERROR: ACT动作文件必须恰好包含100x6个float32（2400字节）: $INPUT" >&2
  exit 1
fi
STAGING="$(mktemp -d /tmp/exokernel-actions.XXXXXX)"
TEMP="$OUTPUT.tmp.$$"
trap 'rm -rf "$STAGING"; rm -f "$TEMP"' EXIT INT TERM
cp "$INPUT" "$STAGING/actions.f32le"
qemu-img create -q -f raw "$TEMP" 16M
"$MKE2FS" -q -F -t ext4 -b 4096 -m 0 -L ACTACTIONS -d "$STAGING" "$TEMP"
mv "$TEMP" "$OUTPUT"
rm -rf "$STAGING"
trap - EXIT INT TERM
echo "==> ACT动作ext4已创建: $OUTPUT"
