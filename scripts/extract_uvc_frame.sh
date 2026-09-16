#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${UVC_FS_IMAGE:-$ROOT_DIR/qemu/uvc-capture.ext4}"
OUTPUT="${UVC_FRAME_OUTPUT:-$ROOT_DIR/qemu/uvc-frame.jpg}"
DEBUGFS="${DEBUGFS:-/opt/homebrew/opt/e2fsprogs/sbin/debugfs}"

if [ ! -f "$IMAGE" ] || [ ! -x "$DEBUGFS" ]; then
    echo "ERROR: 缺少UVC镜像或debugfs" >&2
    exit 1
fi
TEMP="$OUTPUT.full.$$"
trap 'rm -f "$TEMP"' EXIT INT TERM
mkdir -p "$(dirname "$OUTPUT")"
"$DEBUGFS" -R "dump /uvc-frame.mjpg $TEMP" "$IMAGE" >/dev/null 2>&1

python3 - "$IMAGE" "$TEMP" "$OUTPUT" <<'PY'
import hashlib
import struct
import sys

image_path, source_path, output_path = sys.argv[1:]
with open(image_path, "rb") as image:
    metadata = image.read(512)
if metadata[:8] != b"EXOUVC01":
    raise SystemExit("ERROR: UVC capture metadata magic mismatch")
length = struct.unpack_from("<Q", metadata, 24)[0]
with open(source_path, "rb") as source:
    frame = source.read(length)
if len(frame) != length or length == 0:
    raise SystemExit(f"ERROR: invalid captured frame length: {length}")
if not frame.startswith(b"\xff\xd8") or not frame.endswith(b"\xff\xd9"):
    raise SystemExit("ERROR: captured UVC payload is not a complete JPEG")
with open(output_path, "wb") as output:
    output.write(frame)
print(f"==> 已从ext4提取照片: {output_path}")
print(f"    bytes={length} sha256={hashlib.sha256(frame).hexdigest()}")
PY

trap - EXIT INT TERM
rm -f "$TEMP"

