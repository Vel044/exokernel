#!/bin/bash
set -euo pipefail

# 下载与默认ACT模型严格配套的两路摄像头、关节状态和动作数据集。
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT_DIR/.." && pwd)"
DESTINATION="${ACT_DATASET_DIR:-$WORKSPACE_ROOT/data/act/dataset}"
REPOSITORY="Vel044/so101_bottle"
REVISION="4b5f3bc6638db278caaf6b3b1696bdb49493ad3a"

echo "==> 下载ACT数据集: $REPOSITORY@$REVISION"
echo "    目标目录: $DESTINATION"
ACT_HF_REPOSITORY="$REPOSITORY" ACT_HF_REVISION="$REVISION" \
ACT_HF_DESTINATION="$DESTINATION" conda run -n lerobot python -c \
    'import os; from huggingface_hub import snapshot_download; snapshot_download(repo_id=os.environ["ACT_HF_REPOSITORY"], repo_type="dataset", revision=os.environ["ACT_HF_REVISION"], local_dir=os.environ["ACT_HF_DESTINATION"])'

test -f "$DESTINATION/data/chunk-000/file-000.parquet"
test -f "$DESTINATION/videos/observation.images.handeye/chunk-000/file-000.mp4"
test -f "$DESTINATION/videos/observation.images.fixed/chunk-000/file-000.mp4"
echo "==> bottle数据集准备完成"
