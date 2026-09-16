#!/bin/bash
set -euo pipefail

# 从guest写回的ext4盘导出二进制结果；不使用串口文本重建float32。
ROOT_DIR="$(cd "$(dirname "$0")/../../.." && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT_DIR/.." && pwd)"
SCRIPT_DIR="$ROOT_DIR/experiment/02-ACT真实数据推理正确性验证/linux-qemu"
BUILD_DIR="${LINUX_QEMU_BUILD_DIR:-$SCRIPT_DIR/build}"
RESULT_DIR="$ROOT_DIR/experiment/02-ACT真实数据推理正确性验证/results/linux-rust"
CASES_DIR="${ACT_CASES_DIR:-$WORKSPACE_ROOT/data/act/correctness}"
IMAGE="$BUILD_DIR/act-linux.ext4"
DEBUGFS=/opt/homebrew/opt/e2fsprogs/sbin/debugfs

mkdir -p "$RESULT_DIR/raw"
"$DEBUGFS" -R "dump /linux-rust-summary.csv $RESULT_DIR/summary.csv" "$IMAGE" >/dev/null
for case_index in 000 001 002 003 004; do
    mkdir -p "$RESULT_DIR/raw/case-$case_index"
    "$DEBUGFS" -R "dump /cases/case-$case_index/linux-rust-action.f32le $RESULT_DIR/raw/case-$case_index/linux-rust-action.f32le" \
        "$IMAGE" >/dev/null
done

python3 "$ROOT_DIR/experiment/02-ACT真实数据推理正确性验证/scripts/generate_comparison_csv.py" \
    --cases "$CASES_DIR" \
    --actual-root "$RESULT_DIR/raw" \
    --actual-name linux-rust-action.f32le \
    --output-dir "$RESULT_DIR/full-output"

conda run --no-capture-output -n lerobot python \
    "$ROOT_DIR/experiment/02-ACT真实数据推理正确性验证/scripts/analyze_results.py" \
    --csv "$RESULT_DIR/full-output/all-cases-pytorch-vs-rust.csv" \
    --output-dir "$ROOT_DIR/experiment/02-ACT真实数据推理正确性验证/results/figures" \
    --system-label "QEMU Linux Rust"

echo "Linux Rust results: $RESULT_DIR"
