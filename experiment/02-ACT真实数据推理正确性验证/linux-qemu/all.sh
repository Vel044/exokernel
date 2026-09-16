#!/bin/bash
set -euo pipefail

# 一键执行正式Linux Rust正确性路径：所有模型推理均发生在QEMU AArch64 Linux内。
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
bash "$SCRIPT_DIR/build.sh"
bash "$SCRIPT_DIR/run.sh"
bash "$SCRIPT_DIR/export_results.sh"
