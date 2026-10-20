#!/usr/bin/env bash
set -euo pipefail
# 兼容原环境变量；严格验收和进程清理由本地隔离runner提供。
exec python3 "$(cd "$(dirname "$0")" && pwd)/act_release_entry.py" benchmark
