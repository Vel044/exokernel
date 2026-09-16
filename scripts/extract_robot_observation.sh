#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${OBSERVATION_FS_IMAGE:-$ROOT_DIR/qemu/robot-observation.ext4}"
OUTPUT_DIR="${OBSERVATION_OUTPUT_DIR:-$ROOT_DIR/qemu/robot-observation}"
DEBUGFS="${DEBUGFS:-/opt/homebrew/opt/e2fsprogs/sbin/debugfs}"
TEMP="$(mktemp /tmp/exokernel-observation-bin.XXXXXX)"
trap 'rm -f "$TEMP"' EXIT INT TERM

mkdir -p "$OUTPUT_DIR"
"$DEBUGFS" -R "dump /observation.bin $TEMP" "$IMAGE" >/dev/null 2>&1
python3 - "$TEMP" "$OUTPUT_DIR" <<'PY'
import hashlib, json, struct, sys
from pathlib import Path

source, output_dir = Path(sys.argv[1]), Path(sys.argv[2])
data = source.read_bytes()
if data[:8] != b"ACTOBS01" or struct.unpack_from("<I", data, 8)[0] != 1:
    raise SystemExit("ERROR: observation header无效，guest可能尚未完成写盘")
handeye_len, fixed_len, state_count = struct.unpack_from("<III", data, 12)
if state_count != 6 or not (0 < handeye_len <= 2 * 1024 * 1024) or not (0 < fixed_len <= 2 * 1024 * 1024):
    raise SystemExit("ERROR: observation长度字段无效")
state = list(struct.unpack_from("<6f", data, 24))
handeye = data[4096:4096 + handeye_len]
fixed_offset = 4096 + 2 * 1024 * 1024
fixed = data[fixed_offset:fixed_offset + fixed_len]
for name, image in (("handeye", handeye), ("fixed", fixed)):
    if not image.startswith(b"\xff\xd8") or not image.endswith(b"\xff\xd9"):
        raise SystemExit(f"ERROR: {name}不是完整JPEG")
    (output_dir / f"{name}.jpg").write_bytes(image)
(output_dir / "state.f32le").write_bytes(struct.pack("<6f", *state))
manifest = {
    "format": "exokernel-act-observation-v1",
    "handeye": {"file": "handeye.jpg", "bytes": len(handeye), "sha256": hashlib.sha256(handeye).hexdigest()},
    "fixed": {"file": "fixed.jpg", "bytes": len(fixed), "sha256": hashlib.sha256(fixed).hexdigest()},
    "state": state,
}
(output_dir / "observation.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(json.dumps(manifest, indent=2))
PY

# Pillow只用于宿主预览，不参与guest采集和后续ACT输入。
python3 - "$OUTPUT_DIR" <<'PY'
import sys
from pathlib import Path
try:
    from PIL import Image, ImageDraw
except ImportError:
    raise SystemExit(0)
directory = Path(sys.argv[1])
images = [Image.open(directory / name).convert("RGB") for name in ("handeye.jpg", "fixed.jpg")]
canvas = Image.new("RGB", (images[0].width + images[1].width, max(i.height for i in images)), "white")
canvas.paste(images[0], (0, 0)); canvas.paste(images[1], (images[0].width, 0))
ImageDraw.Draw(canvas).text((8, 8), "handeye", fill="red")
ImageDraw.Draw(canvas).text((images[0].width + 8, 8), "fixed", fill="red")
canvas.save(directory / "preview.jpg", quality=95)
PY

echo "==> 观测已提取到: $OUTPUT_DIR"
