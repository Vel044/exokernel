#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT_DIR/.." && pwd)"
MODEL_DIR="${ACT_MODEL_DIR:-$WORKSPACE_ROOT/data/act/model}"
OBSERVATION_DIR="${ACT_OBSERVATION_DIR:-$WORKSPACE_ROOT/data/act/current-observation}"
OUTPUT="${ACT_FS_IMAGE:-$ROOT_DIR/qemu/act-benchmark.ext4}"
MODEL="$MODEL_DIR/model.safetensors"
STATS="$MODEL_DIR/policy_preprocessor_step_3_normalizer_processor.safetensors"

for file in "$MODEL" "$STATS" \
    "$OBSERVATION_DIR/handeye.rgb" "$OBSERVATION_DIR/fixed.rgb" \
    "$OBSERVATION_DIR/state.f32le"; do
    if [ ! -f "$file" ]; then
        echo "ERROR: ACT固定基准输入缺失: $file" >&2
        exit 1
    fi
done

MKE2FS="${MKE2FS:-}"
if [ -z "$MKE2FS" ]; then
    if command -v mke2fs >/dev/null 2>&1; then
        MKE2FS="$(command -v mke2fs)"
    else
        MKE2FS=/opt/homebrew/opt/e2fsprogs/sbin/mke2fs
    fi
fi
if [ ! -x "$MKE2FS" ]; then
    echo "ERROR: 找不到mke2fs；macOS可执行 brew install e2fsprogs" >&2
    exit 1
fi

if [ -f "$OUTPUT" ] && [ "$OUTPUT" -nt "$MODEL" ] && [ "$OUTPUT" -nt "$STATS" ] \
    && [ "$OUTPUT" -nt "$OBSERVATION_DIR/handeye.rgb" ] \
    && [ "$OUTPUT" -nt "$OBSERVATION_DIR/fixed.rgb" ] \
    && [ "$OUTPUT" -nt "$OBSERVATION_DIR/state.f32le" ]; then
    echo "==> 复用ACT固定观测ext4镜像: $OUTPUT"
    exit 0
fi

mkdir -p "$(dirname "$OUTPUT")"
TEMP="$OUTPUT.tmp.$$"
STAGING="$(mktemp -d /tmp/exokernel-act-benchmark.XXXXXX)"
trap 'rm -f "$TEMP"; rm -rf "$STAGING"' EXIT INT TERM

cp "$MODEL" "$STAGING/model.safetensors"
cp "$STATS" "$STAGING/policy_preprocessor_step_3_normalizer_processor.safetensors"
mkdir -p "$STAGING/observation"
cp "$OBSERVATION_DIR/handeye.rgb" "$STAGING/observation/handeye.rgb"
cp "$OBSERVATION_DIR/fixed.rgb" "$STAGING/observation/fixed.rgb"
cp "$OBSERVATION_DIR/state.f32le" "$STAGING/observation/state.f32le"

echo "==> 创建ACT固定观测ext4镜像: $OUTPUT"
qemu-img create -q -f raw "$TEMP" 256M
"$MKE2FS" -q -F -t ext4 -b 4096 -m 0 -L ACTBENCH -d "$STAGING" "$TEMP"
mv "$TEMP" "$OUTPUT"
rm -rf "$STAGING"
trap - EXIT INT TERM
echo "    模型、normalizer、当前两张RGB和六轴状态已写入ext4"
