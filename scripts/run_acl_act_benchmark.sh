#!/usr/bin/env bash
set -euo pipefail

# 在同一 ACT 输入上重复执行 LibOS provider，并生成可审计的三种汇总格式。
# 结果只写入 target/，不把 ELF、模型和串口大日志加入 Git。

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT_DIR/.." && pwd)"
PROVIDER="${ACT_KERNEL_PROVIDER:-acl}"
CASE_ID="${ACT_CASE_ID:-000}"
REPLICATES="${ACT_BENCHMARK_REPLICATES:-3}"
ACCEL="${QEMU_ACCEL:-hvf}"
TIMEOUT_SECONDS="${ACT_QEMU_TIMEOUT_SECONDS:-120}"

case "$PROVIDER" in
  acl|portable) ;;
  *) echo "ERROR: ACT_KERNEL_PROVIDER 必须为 acl 或 portable" >&2; exit 1 ;;
esac
case "$CASE_ID" in
  000|001|002|003|004) ;;
  *) echo "ERROR: ACT_CASE_ID 必须为 000..004" >&2; exit 1 ;;
esac
case "$REPLICATES" in
  ''|*[!0-9]*) echo "ERROR: ACT_BENCHMARK_REPLICATES 必须为正整数" >&2; exit 1 ;;
esac
[ "$REPLICATES" -gt 0 ] || { echo "ERROR: replicate 数必须大于 0" >&2; exit 1; }
case "$TIMEOUT_SECONDS" in
  ''|*[!0-9]*) echo "ERROR: ACT_QEMU_TIMEOUT_SECONDS 必须为正整数" >&2; exit 1 ;;
esac
[ "$TIMEOUT_SECONDS" -gt 0 ] || { echo "ERROR: ACT_QEMU_TIMEOUT_SECONDS 必须大于 0" >&2; exit 1; }

CASE_DIR="$WORKSPACE_ROOT/data/act/correctness/case-$CASE_ID"
IMAGE="$ROOT_DIR/qemu/act-benchmark-case-$CASE_ID.ext4"
OUT_DIR="$(mktemp -d "$ROOT_DIR/target/acl-benchmark-$PROVIDER-$CASE_ID.XXXXXX")"

ACT_OBSERVATION_DIR="$CASE_DIR" ACT_FS_IMAGE="$IMAGE" \
  "$ROOT_DIR/scripts/build_act_benchmark_ext4.sh" >/dev/null

(
  cd "$ROOT_DIR"
  LIBOS_APP=act-benchmark ACT_KERNEL_PROVIDER="$PROVIDER" ./build.sh
) >"$OUT_DIR/build.log" 2>&1

for replicate in $(seq 1 "$REPLICATES"); do
  log="$OUT_DIR/replicate-$replicate.log"
  pushd "$ROOT_DIR" >/dev/null
  DYLD_LIBRARY_PATH="${DYLD_LIBRARY_PATH:-$HOME/.rustup/toolchains/nightly-aarch64-apple-darwin/lib}" \
    EXOKERNEL_SKIP_BUILD=1 ACT_KERNEL_PROVIDER="$PROVIDER" LIBOS_APP=act-benchmark \
    ACT_FS_IMAGE="$IMAGE" QEMU_ACCEL="$ACCEL" bash qemu/run.sh >"$log" 2>&1 &
  pid=$!
  popd >/dev/null
  completed=0
  for _ in $(seq 1 "$((TIMEOUT_SECONDS * 5))"); do
    if rg -q "ACT_ACTION_BITS_END" "$log" 2>/dev/null; then
      completed=1
      # run.sh 还会再包一层 shell，pgrep -P 只能看到 bash 而看不到 QEMU；
      # 用本次 case 的只读盘路径精确定位后代，避免误伤其他 QEMU。
      child="$(pgrep -f "qemu-system-aarch64.*act-benchmark-case-${CASE_ID}\.ext4" | head -1 || true)"
      # QEMU 的串口输出完成后立即结束，避免后续 case/replicate 继续占用
      # esp.img。先给精确的子进程 TERM；若它仍在退出阶段，短暂等待后
      # 只对同一个 PID 使用 KILL，绝不按宽泛命令名误杀其他虚拟机。
      [ -n "$child" ] && kill -TERM "$child" 2>/dev/null || true
      kill -TERM "$pid" 2>/dev/null || true
      for _stop in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
      done
      if kill -0 "$pid" 2>/dev/null; then
        [ -n "$child" ] && kill -KILL "$child" 2>/dev/null || true
        kill -KILL "$pid" 2>/dev/null || true
      fi
      break
    fi
    if ! kill -0 "$pid" 2>/dev/null; then break; fi
    sleep 0.2
  done
  # 启动异常或超时时不会出现 ACTION_BITS_END；必须主动清理精确的 QEMU，
  # 否则后面的 wait 会把 runner 永久挂住，并且残留进程还会占用 HVF。
  if [ "$completed" -eq 0 ]; then
    child="$(pgrep -f "qemu-system-aarch64.*act-benchmark-case-${CASE_ID}\.ext4" | head -1 || true)"
    [ -n "$child" ] && kill -TERM "$child" 2>/dev/null || true
    kill -TERM "$pid" 2>/dev/null || true
    sleep 1
    [ -n "$child" ] && kill -KILL "$child" 2>/dev/null || true
    kill -KILL "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
done

OUT_DIR="$OUT_DIR" CASE_ID="$CASE_ID" PROVIDER="$PROVIDER" ACCEL="$ACCEL" \
  CASE_DIR="$CASE_DIR" REPLICATES="$REPLICATES" python3 - <<'PY'
import csv
import glob
import hashlib
import json
import math
import os
import re
import struct
from pathlib import Path

out = Path(os.environ["OUT_DIR"])
case = os.environ["CASE_ID"]
provider = os.environ["PROVIDER"]
reference_path = Path(os.environ["CASE_DIR"]) / "pytorch-action.f32le"
reference_bytes = reference_path.read_bytes()
reference = struct.unpack("<" + "f" * (len(reference_bytes) // 4), reference_bytes)

rows = []
for log_path in sorted(Path(item) for item in glob.glob(str(out / "replicate-*.log"))):
    text = log_path.read_text(errors="replace")
    values = {}
    for match in re.finditer(r"ACT_ACTION_BITS index=(\d+) bits=0x([0-9a-fA-F]+)", text):
        values[int(match.group(1))] = struct.unpack("<f", struct.pack("<I", int(match.group(2), 16) & 0xFFFFFFFF))[0]
    actual = [values[index] for index in sorted(values)]
    failures = 0
    max_abs = 0.0
    max_rel = 0.0
    if len(actual) != len(reference):
        failures += abs(len(actual) - len(reference))
    for got, want in zip(actual, reference):
        absolute = abs(got - want)
        relative = absolute / max(abs(want), 1e-12)
        max_abs = max(max_abs, absolute)
        max_rel = max(max_rel, relative)
        if not (math.isfinite(got) and math.isfinite(want) and absolute <= 1e-4 + 1e-5 * abs(want)):
            failures += 1
    match = re.search(r"ACT benchmark complete median_ms=(\d+) p95_ms=(\d+) samples_ms=([^\s]+)", text)
    model_match = re.search(r"ACT model-only complete median_ms=(\d+) p95_ms=(\d+) samples_ms=([^\s]+)", text)
    median_ms = int(match.group(1)) if match else None
    p95_ms = int(match.group(2)) if match else None
    samples = [int(item) for item in match.group(3).rstrip(",").split(",")] if match else []
    model_median_ms = int(model_match.group(1)) if model_match else None
    model_p95_ms = int(model_match.group(2)) if model_match else None
    model_samples = [int(item) for item in model_match.group(3).rstrip(",").split(",")] if model_match else []
    raw = b"".join(struct.pack("<f", value) for value in actual)
    rows.append({
        "replicate": log_path.stem,
        "log": str(log_path),
        "values": len(actual),
        "failed_values": failures,
        "max_abs_error": max_abs,
        "max_relative_error": max_rel,
        "median_ms": median_ms,
        "p95_ms": p95_ms,
        "samples_ms": samples,
        "model_only_median_ms": model_median_ms,
        "model_only_p95_ms": model_p95_ms,
        "model_only_samples_ms": model_samples,
        "output_sha256": hashlib.sha256(raw).hexdigest(),
        "el0_exit_zero": "EL0 exit code=0x0000000000000000" in text,
        "continuous_check_passed": "ACT continuous check passed runs=100" in text,
    })

valid = [
    row
    for row in rows
    if row["median_ms"] is not None
    and row["model_only_median_ms"] is not None
    and row["failed_values"] == 0
    and row["values"] == 600
]
summary = {
    "provider": provider,
    "case_id": case,
    "qemu": {"accelerator": os.environ["ACCEL"], "cpu": "host" if os.environ["ACCEL"] == "hvf" else "cortex-a76", "vcpus": 4, "memory": "4G"},
    "correctness": {"reference": str(reference_path), "cases": 1, "values": sum(row["values"] for row in rows), "failed_values": sum(row["failed_values"] for row in rows), "all_pass": len(rows) == len(valid) == int(os.environ["REPLICATES"])},
    "continuous_check": {"requested": os.environ.get("ACT_CONTINUOUS_CHECK") == "1", "all_pass": all(row["continuous_check_passed"] for row in rows)},
    "replicates": rows,
}
(out / "summary.json").write_text(json.dumps(summary, indent=2, ensure_ascii=False) + "\n")
with (out / "summary.csv").open("w", newline="", encoding="utf-8") as stream:
    writer = csv.DictWriter(stream, fieldnames=["replicate", "values", "failed_values", "max_abs_error", "max_relative_error", "median_ms", "p95_ms", "model_only_median_ms", "model_only_p95_ms", "output_sha256", "el0_exit_zero"])
    writer.writeheader()
    for row in rows:
        writer.writerow({key: row[key] for key in writer.fieldnames})
lines = [
    "# ACL ACT LibOS benchmark",
    "",
    f"- provider: `{provider}`",
    f"- case: `{case}`; accelerator: `{os.environ['ACCEL']}`; 4 vCPU / 4G",
    f"- correctness: `{summary['correctness']['failed_values']}` failed values / `{summary['correctness']['values']}` values",
    f"- continuous check: requested=`{summary['continuous_check']['requested']}`; passed=`{summary['continuous_check']['all_pass']}`",
    "",
    "| replicate | E2E median ms | E2E P95 ms | model median ms | model P95 ms | failed | output SHA-256 |",
    "|---|---:|---:|---:|---:|---:|---|",
]
for row in rows:
    lines.append(f"| {row['replicate']} | {row['median_ms']} | {row['p95_ms']} | {row['model_only_median_ms']} | {row['model_only_p95_ms']} | {row['failed_values']} | `{row['output_sha256']}` |")
(out / "SUMMARY.md").write_text("\n".join(lines) + "\n")
print(f"ACL_BENCHMARK_OUT={out}")
print(f"ACL_BENCHMARK_PASS={summary['correctness']['all_pass']}")
if not summary["correctness"]["all_pass"]:
    raise SystemExit(1)
PY
