#!/usr/bin/env bash
set -euo pipefail

# 在固定 QEMU/HVF 和同一 Safetensors 上跑完整五组 ACT 正确性。
# 性能 runner 只负责一组/若干启动；本入口负责汇总 3000 个动作值，任何
# case 失败都以非零状态退出，避免把部分证据误报为全通过。

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROVIDER="${ACT_KERNEL_PROVIDER:-acl}"
ACCEL="${QEMU_ACCEL:-hvf}"
RESULT_ROOT="$(mktemp -d "$ROOT_DIR/target/acl-correctness-$PROVIDER.XXXXXX")"
SUMMARY_LIST="$RESULT_ROOT/summary-paths.txt"
: >"$SUMMARY_LIST"

for case_id in 000 001 002 003 004; do
  echo "== case-$case_id ($PROVIDER/$ACCEL) =="
  output="$(
    ACT_KERNEL_PROVIDER="$PROVIDER" ACT_CASE_ID="$case_id" \
      ACT_BENCHMARK_REPLICATES=1 QEMU_ACCEL="$ACCEL" \
      "$SCRIPT_DIR/run_acl_act_benchmark.sh"
  )"
  printf '%s\n' "$output"
  summary_dir="$(printf '%s\n' "$output" | sed -n 's/^ACL_BENCHMARK_OUT=//p' | tail -1)"
  [ -n "$summary_dir" ] || { echo "ERROR: case-$case_id 未生成 summary" >&2; exit 2; }
  printf '%s\n' "$summary_dir/summary.json" >>"$SUMMARY_LIST"
done

SUMMARY_LIST="$SUMMARY_LIST" RESULT_ROOT="$RESULT_ROOT" PROVIDER="$PROVIDER" ACCEL="$ACCEL" \
python3 - <<'PY'
import csv
import json
import os
from pathlib import Path

paths = [Path(line.strip()) for line in Path(os.environ["SUMMARY_LIST"]).read_text().splitlines() if line.strip()]
rows = []
for path in paths:
    data = json.loads(path.read_text())
    row = data["replicates"][0]
    rows.append({
        "case_id": data["case_id"],
        "values": row["values"],
        "failed_values": row["failed_values"],
        "max_abs_error": row["max_abs_error"],
        "max_relative_error": row["max_relative_error"],
        "median_ms": row["median_ms"],
        "p95_ms": row["p95_ms"],
        "output_sha256": row["output_sha256"],
        "el0_exit_zero": row["el0_exit_zero"],
        "source_summary": str(path),
    })
all_pass = len(rows) == 5 and all(
    row["values"] == 600 and row["failed_values"] == 0 and row["el0_exit_zero"] for row in rows
)
summary = {
    "provider": os.environ["PROVIDER"],
    "qemu": {"accelerator": os.environ["ACCEL"], "cpu": "host" if os.environ["ACCEL"] == "hvf" else "cortex-a76", "vcpus": 4, "memory": "4G"},
    "tolerance": {"atol": 1e-4, "rtol": 1e-5},
    "cases": rows,
    "correctness": {"case_count": len(rows), "values": sum(row["values"] for row in rows), "failed_values": sum(row["failed_values"] for row in rows), "all_pass": all_pass},
}
out = Path(os.environ["RESULT_ROOT"])
(out / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
with (out / "summary.csv").open("w", newline="", encoding="utf-8") as stream:
    writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
    writer.writeheader()
    writer.writerows(rows)
lines = [
    "# ACL ACT 五组正确性",
    "",
    f"- provider: `{summary['provider']}`; QEMU: `{os.environ['ACCEL']}`; 4 vCPU / 4G",
    "- tolerance: `atol=1e-4`, `rtol=1e-5`",
    f"- result: `{summary['correctness']['failed_values']}` failed / `{summary['correctness']['values']}` values",
    "",
    "| case | values | failed | max abs | max rel | output SHA-256 |",
    "|---|---:|---:|---:|---:|---|",
]
for row in rows:
    lines.append(f"| {row['case_id']} | {row['values']} | {row['failed_values']} | {row['max_abs_error']:.9g} | {row['max_relative_error']:.9g} | `{row['output_sha256']}` |")
(out / "SUMMARY.md").write_text("\n".join(lines) + "\n")
print(f"ACL_CORRECTNESS_OUT={out}")
print(f"ACL_CORRECTNESS_PASS={all_pass}")
if not all_pass:
    raise SystemExit(1)
PY
