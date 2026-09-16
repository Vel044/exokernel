#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT_DIR/.." && pwd)"
EXPERIMENT_DIR="$ROOT_DIR/experiment/02-ACT真实数据推理正确性验证"
MODEL_DIR="${ACT_MODEL_DIR:-$WORKSPACE_ROOT/data/act/model}"
DATASET_DIR="${ACT_DATASET_DIR:-$WORKSPACE_ROOT/data/act/dataset}"
CASES_DIR="${ACT_CASES_DIR:-$WORKSPACE_ROOT/data/act/correctness}"
FFMPEG="${FFMPEG:-/opt/homebrew/bin/ffmpeg}"

# 正式基线固定为最基础的bottle模型；拒绝从环境变量误传classification目录。
MODEL_REPO="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["repo_id"])' "$MODEL_DIR/config.json")"
if [ "$MODEL_REPO" != "Vel044/so101_act_bottle" ]; then
    echo "ERROR: expected Vel044/so101_act_bottle, got $MODEL_REPO" >&2
    exit 1
fi

conda run --no-capture-output -n lerobot python \
    "$EXPERIMENT_DIR/scripts/prepare_cases.py" \
    --dataset-dir "$DATASET_DIR" --model-dir "$MODEL_DIR" \
    --output "$CASES_DIR" --ffmpeg "$FFMPEG"

conda run --no-capture-output -n lerobot python \
    "$EXPERIMENT_DIR/scripts/generate_pytorch_reference.py" \
    --model-dir "$MODEL_DIR" --cases "$CASES_DIR"

# 被测对象是自研Rust ACT后端；它与上面的lerobot/src PyTorch真值逐元素比较。
PATH="$HOME/.cargo/bin:$PATH" cargo run --release \
    --manifest-path "$ROOT_DIR/act-runtime/Cargo.toml" --example infer -- \
    "$MODEL_DIR" "$CASES_DIR"

conda run --no-capture-output -n lerobot python \
    "$EXPERIMENT_DIR/scripts/generate_comparison_csv.py" \
    --cases "$CASES_DIR" \
    --output-dir "$EXPERIMENT_DIR/results/full-output"

# figures必须和刚生成的3000行逐元素CSV来自同一轮运行，不能继续复用
# 仓库中上一次实验遗留的图表。
conda run --no-capture-output -n lerobot python \
    "$EXPERIMENT_DIR/scripts/analyze_results.py" \
    --csv "$EXPERIMENT_DIR/results/full-output/all-cases-pytorch-vs-rust.csv" \
    --output-dir "$EXPERIMENT_DIR/results/figures" \
    --system-label "Rust act-runtime (Exokernel-validated)"

python3 "$EXPERIMENT_DIR/scripts/pack_bundle.py" \
    --model-dir "$MODEL_DIR" --cases-dir "$CASES_DIR" \
    --output "$CASES_DIR/act-correctness.bundle"

echo "ACT correctness vectors: $CASES_DIR"
echo "宿主Rust输出仅用于预检；正式Linux结果请运行linux-qemu/all.sh"
