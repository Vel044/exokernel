#!/bin/bash
set -euo pipefail

# 机器人闭环只需要模型和normalizer，不把正确性实验向量打进磁盘。
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT_DIR/.." && pwd)"
SOURCE="${ACT_MODEL_DIR:-$WORKSPACE_ROOT/data/act/model}"
OUTPUT="${ACT_FS_IMAGE:-$ROOT_DIR/qemu/act-model.ext4}"
MODEL="$SOURCE/model.safetensors"
STATS="$SOURCE/policy_preprocessor_step_3_normalizer_processor.safetensors"

if [ ! -f "$MODEL" ] || [ ! -f "$STATS" ]; then
    echo "ERROR: ACT模型文件不存在: $SOURCE" >&2
    echo "Run: bash $ROOT_DIR/scripts/fetch_act_model.sh" >&2
    exit 1
fi

MKE2FS="${MKE2FS:-}"
if [ -z "$MKE2FS" ]; then
    if command -v mke2fs >/dev/null 2>&1; then
        MKE2FS="$(command -v mke2fs)"
    elif [ -x /opt/homebrew/opt/e2fsprogs/sbin/mke2fs ]; then
        MKE2FS=/opt/homebrew/opt/e2fsprogs/sbin/mke2fs
    else
        echo "ERROR: 找不到mke2fs；macOS可执行 brew install e2fsprogs" >&2
        exit 1
    fi
fi

if [ -f "$OUTPUT" ] && [ "$OUTPUT" -nt "$MODEL" ] && [ "$OUTPUT" -nt "$STATS" ]; then
    echo "==> 复用ACT机器人ext4镜像: $OUTPUT"
    exit 0
fi

mkdir -p "$(dirname "$OUTPUT")"
TEMP="$OUTPUT.tmp.$$"
STAGING="$(mktemp -d /tmp/exokernel-robot-act-ext4.XXXXXX)"
trap 'rm -f "$TEMP"; rm -rf "$STAGING"' EXIT INT TERM
cp "$MODEL" "$STAGING/model.safetensors"
cp "$STATS" "$STAGING/policy_preprocessor_step_3_normalizer_processor.safetensors"

echo "==> 创建机器人ACT ext4镜像: $OUTPUT"
qemu-img create -q -f raw "$TEMP" 256M
"$MKE2FS" -q -F -t ext4 -b 4096 -m 0 -L ROBOTACT -d "$STAGING" "$TEMP"
mv "$TEMP" "$OUTPUT"
rm -rf "$STAGING"
trap - EXIT INT TERM
echo "    仅写入model.safetensors和normalizer"
