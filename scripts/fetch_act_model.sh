#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT_DIR/.." && pwd)"
DESTINATION="${ACT_MODEL_DIR:-$WORKSPACE_ROOT/data/act/model}"
REPOSITORY="Vel044/so101_act_bottle"
REVISION="1c9b9309c387041f051e4ded0199e79f538f4984"

echo "==> 下载ACT策略: $REPOSITORY"
echo "    目标目录: $DESTINATION"

if command -v conda >/dev/null 2>&1; then
    # 直接调用lerobot环境的Python API，避免PATH中的其他hf CLI因Typer版本
    # 不兼容而在真正开始下载前失败；snapshot_download仍会复用HF本机缓存。
    ACT_HF_REPOSITORY="$REPOSITORY" ACT_HF_REVISION="$REVISION" ACT_HF_DESTINATION="$DESTINATION" \
        conda run -n lerobot python -c \
        'import os; from huggingface_hub import snapshot_download; snapshot_download(repo_id=os.environ["ACT_HF_REPOSITORY"], revision=os.environ["ACT_HF_REVISION"], local_dir=os.environ["ACT_HF_DESTINATION"])'
elif command -v hf >/dev/null 2>&1; then
    hf download "$REPOSITORY" --revision "$REVISION" --local-dir "$DESTINATION"
else
    echo "ERROR: 找不到hf或conda命令" >&2
    exit 1
fi

test -f "$DESTINATION/model.safetensors"
test -f "$DESTINATION/policy_preprocessor_step_3_normalizer_processor.safetensors"
echo "==> 模型准备完成"
echo "    QEMU: LIBOS_APP=act-inference bash $ROOT_DIR/qemu/run.sh"
