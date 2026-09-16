#!/usr/bin/env bash
set -euo pipefail

# 验证最终 EL0 ELF 是 freestanding 静态镜像：没有解释器、动态依赖、未解析
# 符号或 Linux 线程/加载器符号。该脚本只读 ELF，不修改构建产物。
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ELF="${1:-$ROOT_DIR/libos/libos.elf}"
READELF="${READELF:-$(command -v aarch64-linux-gnu-readelf || command -v readelf || true)}"
NM="${NM:-$(command -v aarch64-linux-gnu-nm || command -v nm || true)}"
[ -f "$ELF" ] || { echo "ERROR: ELF 不存在: $ELF" >&2; exit 2; }
[ -n "$READELF" ] || { echo "ERROR: 缺少 readelf" >&2; exit 2; }
[ -n "$NM" ] || { echo "ERROR: 缺少 nm" >&2; exit 2; }

headers="$($READELF -l "$ELF")"
dynamic="$($READELF -d "$ELF" 2>&1 || true)"
undefined="$($NM -u "$ELF")"
forbidden="$($NM "$ELF" | rg 'pthread|GOMP_|(^|_)omp_|dlopen|dlsym|mmap|munmap|futex|syscall|getauxval|sched_|sysconf' || true)"

if printf '%s\n' "$headers" | rg -q 'INTERP'; then
  echo "ERROR: ELF 含 PT_INTERP" >&2; exit 1
fi
if printf '%s\n' "$dynamic" | rg -q 'NEEDED|INTERP'; then
  echo "ERROR: ELF 含动态依赖" >&2; exit 1
fi
if [ -n "$undefined" ]; then
  echo "ERROR: ELF 含未解析符号:" >&2
  printf '%s\n' "$undefined" >&2
  exit 1
fi
if [ -n "$forbidden" ]; then
  echo "ERROR: ELF 含禁止运行时符号:" >&2
  printf '%s\n' "$forbidden" >&2
  exit 1
fi

echo "ACL_LIBOS_ELF_PASS=true"
echo "elf=$ELF"
echo "bytes=$(wc -c < "$ELF" | tr -d ' ')"
if command -v shasum >/dev/null 2>&1; then
  echo "sha256=$(shasum -a 256 "$ELF" | awk '{print $1}')"
else
  echo "sha256=$(sha256sum "$ELF" | awk '{print $1}')"
fi
echo "pt_interp=false"
echo "dynamic_needed=false"
echo "undefined_symbols=0"
echo "forbidden_symbols=0"
