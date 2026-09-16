#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT_DIR/.." && pwd)"
SOURCE="${ACT_MODEL_DIR:-$WORKSPACE_ROOT/data/act/model}"
CASES="${ACT_CASES_DIR:-$WORKSPACE_ROOT/data/act/correctness}"
OUTPUT="${ACT_FS_IMAGE:-$ROOT_DIR/qemu/act-model.ext4}"
MODEL="$SOURCE/model.safetensors"
STATS="$SOURCE/policy_preprocessor_step_3_normalizer_processor.safetensors"
CONFIG="$SOURCE/config.json"

if [ ! -f "$MODEL" ] || [ ! -f "$STATS" ] || [ ! -f "$CONFIG" ]; then
    echo "ERROR: ACT模型文件不存在: $SOURCE" >&2
    echo "Run: bash $ROOT_DIR/scripts/fetch_act_model.sh" >&2
    exit 1
fi

if [ ! -f "$CASES/manifest.json" ]; then
    echo "ERROR: ACT正确性测试向量不存在: $CASES" >&2
    echo "Run: bash $ROOT_DIR/experiment/02-ACT真实数据推理正确性验证/run_prepare.sh" >&2
    exit 1
fi

# 所有默认ACT实验固定使用最基础的bottle策略。这里在宿主构建阶段拒绝旧
# classification权重，防止模型、数据集和PyTorch参考输出被静默混用。
MODEL_REPO="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["repo_id"])' "$CONFIG")"
if [ "$MODEL_REPO" != "Vel044/so101_act_bottle" ]; then
    echo "ERROR: 默认ACT模型必须是Vel044/so101_act_bottle，实际为$MODEL_REPO" >&2
    exit 1
fi
CASE_MODEL_REPO="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["model_repo"])' "$CASES/manifest.json")"
if [ "$CASE_MODEL_REPO" != "$MODEL_REPO" ]; then
    echo "ERROR: ACT模型与正确性输入不匹配: $MODEL_REPO != $CASE_MODEL_REPO" >&2
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

# 模型目录和image都未变化时复用现有文件，避免每次启动复制约197MiB。
if [ -f "$OUTPUT" ] && [ "$OUTPUT" -nt "$MODEL" ] && [ "$OUTPUT" -nt "$STATS" ] && [ "$OUTPUT" -nt "$CASES/manifest.json" ]; then
    echo "==> 复用ACT ext4镜像: $OUTPUT"
    exit 0
fi

mkdir -p "$(dirname "$OUTPUT")"
TEMP="$OUTPUT.tmp.$$"
STAGING="$(mktemp -d /tmp/exokernel-act-ext4.XXXXXX)"
trap 'rm -f "$TEMP"; rm -rf "$STAGING"' EXIT INT TERM

# ext4根目录只放EL0真正使用的文件。视频和Parquet保留在宿主实验缓存，避免
# 把五百多MiB原始数据塞进QEMU磁盘；五组冻结RGB及参考输出放在/cases。
cp "$MODEL" "$STAGING/model.safetensors"
cp "$STATS" "$STAGING/policy_preprocessor_step_3_normalizer_processor.safetensors"
mkdir -p "$STAGING/cases"
cp "$CASES/manifest.json" "$STAGING/cases/manifest.json"
for case_index in 000 001 002 003 004; do
    cp -R "$CASES/case-$case_index" "$STAGING/cases/case-$case_index"
done

echo "==> 创建用户态ACT ext4镜像: $OUTPUT"
# 模型约197MiB，五组冻结向量约7MiB；320MiB给ext4元数据保留明确余量。
qemu-img create -q -f raw "$TEMP" 320M
# `-d`由mke2fs直接把目录内容写入新文件系统，不需要macOS挂载ext4。
"$MKE2FS" -q -F -t ext4 -b 4096 -m 0 -L ACTMODEL -d "$STAGING" "$TEMP"
mv "$TEMP" "$OUTPUT"
rm -rf "$STAGING"
trap - EXIT INT TERM
echo "    model、normalizer和五组真实输入已写入ext4"
