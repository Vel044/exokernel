#!/usr/bin/env bash
set -euo pipefail

# 构建并审计 ACL v52.7.0 的 AArch64 CPU 静态候选。构建结果和证据全部
# 写入 target/（已被 git 忽略），第三方源码目录本身不被修改。
# ACL_SOURCE_DIR 可指向离线 ACL 源码快照。未安装 SCons 时，auto 使用
# CMake 静态候选；manifest 会明确记录该结果尚未等同于 bare-metal 链接。

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
EXOKERNEL_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_ROOT="$(cd "$EXOKERNEL_ROOT/.." && pwd)"
VERSION="52.7.0"
EXPECTED_ARCHIVE_SHA256="602d6ffa7b7f6d1445c36eace63b17660e7aeb144a2347b7a882917680bdbdd5"

SOURCE_DIR="${ACL_SOURCE_DIR:-$WORKSPACE_ROOT/rutorch/qemu/build/compute-library-v$VERSION}"
ARCHIVE_PATH="${ACL_ARCHIVE:-$WORKSPACE_ROOT/rutorch/qemu/build/downloads/compute-library-v$VERSION.tar.gz}"
BUILD_ROOT="${ACL_BUILD_DIR:-$EXOKERNEL_ROOT/target/acl-freestanding-v$VERSION}"
OUTPUT_DIR="${ACL_OUTPUT_DIR:-$BUILD_ROOT/out}"
BUILD_SYSTEM="${ACL_BUILD_SYSTEM:-auto}"
BUILD_JOBS="${ACL_BUILD_JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)}"

CC="${ACL_CC:-$(command -v aarch64-linux-gnu-gcc || true)}"
CXX="${ACL_CXX:-$(command -v aarch64-linux-gnu-g++ || true)}"
AR="${ACL_AR:-$(command -v aarch64-linux-gnu-ar || command -v ar || true)}"
NM="${ACL_NM:-$(command -v aarch64-linux-gnu-nm || command -v nm || true)}"
READELF="${ACL_READELF:-$(command -v aarch64-linux-gnu-readelf || command -v readelf || true)}"
CMAKE="${ACL_CMAKE:-$(command -v cmake || true)}"
NINJA="${ACL_NINJA:-$(command -v ninja || command -v ninja-build || true)}"
SCONS="${ACL_SCONS:-$(command -v scons || true)}"

die() {
    echo "ERROR: $*" >&2
    exit 1
}

need_file() {
    [ -f "$1" ] || die "缺少文件: $1"
}

hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

hash_tree() {
    # 将相对路径和文件内容按排序顺序输入 SHA-256，避免依赖目录遍历顺序。
    python3 - "$1" <<'PY'
import hashlib
import pathlib
import sys

root = pathlib.Path(sys.argv[1]).resolve()
digest = hashlib.sha256()
excluded = {".git", "build-aarch64", "target"}
files = []
for path in root.rglob("*"):
    if path.is_file():
        relative = path.relative_to(root)
        if not any(part in excluded for part in relative.parts):
            files.append(relative)
for relative in sorted(files, key=lambda item: item.as_posix()):
    name = relative.as_posix().encode("utf-8")
    data = (root / relative).read_bytes()
    digest.update(len(name).to_bytes(8, "big"))
    digest.update(name)
    digest.update(len(data).to_bytes(8, "big"))
    digest.update(data)
print(digest.hexdigest())
PY
}

command -v awk >/dev/null 2>&1 || die "缺少 awk"
command -v python3 >/dev/null 2>&1 || die "缺少 python3"
[ -n "$CC" ] || die "找不到 aarch64-linux-gnu-gcc；可用 ACL_CC 指定"
[ -n "$CXX" ] || die "找不到 aarch64-linux-gnu-g++；可用 ACL_CXX 指定"
[ -n "$AR" ] || die "找不到 AArch64 ar；可用 ACL_AR 指定"
[ -n "$NM" ] || die "找不到 nm；可用 ACL_NM 指定"
[ -n "$READELF" ] || die "找不到 readelf；可用 ACL_READELF 指定"
need_file "$SOURCE_DIR/CMakeLists.txt"
need_file "$SOURCE_DIR/SConstruct"
need_file "$SOURCE_DIR/LICENSES/MIT.txt"

case "$BUILD_SYSTEM" in
    auto) [ -n "$SCONS" ] && BUILD_SYSTEM=scons || BUILD_SYSTEM=cmake ;;
    scons|cmake) ;;
    *) die "ACL_BUILD_SYSTEM 必须为 auto、scons 或 cmake" ;;
esac
if [ "$BUILD_SYSTEM" = scons ]; then
    [ -n "$SCONS" ] || die "ACL_BUILD_SYSTEM=scons 但找不到 scons"
else
    [ -n "$CMAKE" ] || die "ACL_BUILD_SYSTEM=cmake 但找不到 cmake"
    [ -n "$NINJA" ] || die "ACL_BUILD_SYSTEM=cmake 但找不到 ninja"
fi
case "$BUILD_JOBS" in
    ''|*[!0-9]*) die "ACL_BUILD_JOBS 必须为正整数" ;;
esac
[ "$BUILD_JOBS" -gt 0 ] || die "ACL_BUILD_JOBS 必须大于 0"

mkdir -p "$BUILD_ROOT" "$OUTPUT_DIR"
MANIFEST_PATH="$BUILD_ROOT/manifest.json"
SOURCE_TREE_SHA256="$(hash_tree "$SOURCE_DIR")"
ARCHIVE_SHA256=""
ARCHIVE_STATUS="not-provided"
if [ -f "$ARCHIVE_PATH" ]; then
    ARCHIVE_SHA256="$(hash_file "$ARCHIVE_PATH")"
    [ "$ARCHIVE_SHA256" = "$EXPECTED_ARCHIVE_SHA256" ] || die "ACL archive SHA-256 不匹配: $ARCHIVE_SHA256"
    ARCHIVE_STATUS=verified
fi

cat > "$MANIFEST_PATH" <<EOF
{
  "component": "arm-compute-library",
  "version": "$VERSION",
  "expected_archive_sha256": "$EXPECTED_ARCHIVE_SHA256",
  "archive_path": "$ARCHIVE_PATH",
  "archive_sha256": "$ARCHIVE_SHA256",
  "archive_status": "$ARCHIVE_STATUS",
  "source_dir": "$SOURCE_DIR",
  "source_tree_sha256": "$SOURCE_TREE_SHA256",
  "build_system": "$BUILD_SYSTEM",
  "build_mode": "pending",
  "target": "aarch64",
  "architecture": "armv8-a",
  "cpu_tune": "cortex-a76",
  "opencl": false,
  "openmp": false,
  "cpp_threads": false,
  "exceptions": false,
  "shared_library": false,
  "compiler": "$CXX",
  "compiler_version": "$("$CXX" -dumpversion)",
  "ar": "$AR",
  "nm": "$NM",
  "readelf": "$READELF",
  "jobs": $BUILD_JOBS,
  "compile_flags": ["-O3", "-mcpu=cortex-a76", "-fno-exceptions", "-fno-rtti", "-fno-threadsafe-statics", "-fno-unwind-tables", "-fno-asynchronous-unwind-tables", "-ffunction-sections", "-fdata-sections"],
  "forbidden_symbol_patterns": ["pthread", "GOMP_", "omp_", "dlopen", "dlsym", "mmap", "munmap", "futex", "syscall", "open", "read", "write", "fopen", "printf", "getauxval", "sched_", "sysconf", "std::thread", "condition_variable"],
  "artifacts": {},
  "audit": {}
}
EOF

if [ "$BUILD_SYSTEM" = scons ]; then
    # ACL 原生 bare_metal 模式会关闭多线程并生成静态库。这里显式指定
    # aarch64-linux-gnu 编译器，仅用于生成对象；运行时仍待 LibOS 提供。
    ACL_TOOLCHAIN_PREFIX="${ACL_TOOLCHAIN_PREFIX:-aarch64-linux-gnu-}"
    "$SCONS" -C "$SOURCE_DIR" \
        arch=armv8a os=bare_metal build=cross-compile \
        toolchain_prefix="$ACL_TOOLCHAIN_PREFIX" compiler_prefix="$ACL_TOOLCHAIN_PREFIX" \
        standalone=1 \
        neon=1 opencl=0 openmp=0 cppthreads=0 exceptions=0 \
        examples=0 gemm_tuner=0 Werror=0 multi_isa=0 \
        data_type_support=fp32 data_layout_support=nhwc,nchw \
        build_dir="$BUILD_ROOT/scons" -j "$BUILD_JOBS"
    LIBRARY="$BUILD_ROOT/scons/libarm_compute-static.a"
    [ -f "$LIBRARY" ] || die "SCons 没有生成 libarm_compute-static.a"
    BUILD_MODE="bare_metal_scons"
else
    CMAKE_BUILD="$BUILD_ROOT/cmake"
    EXTRA_FLAGS="-O3 -mcpu=cortex-a76 -fno-exceptions -fno-rtti -fno-threadsafe-statics -fno-unwind-tables -fno-asynchronous-unwind-tables -ffunction-sections -fdata-sections"
    "$CMAKE" -S "$SOURCE_DIR" -B "$CMAKE_BUILD" -G Ninja \
        -DCMAKE_SYSTEM_NAME=Linux -DCMAKE_SYSTEM_PROCESSOR=aarch64 \
        -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
        -DCMAKE_C_COMPILER="$CC" -DCMAKE_CXX_COMPILER="$CXX" -DCMAKE_AR="$AR" \
        -DCMAKE_RANLIB="${ACL_RANLIB:-$(command -v aarch64-linux-gnu-ranlib || command -v ranlib || true)}" \
        -DCMAKE_BUILD_TYPE=Release -DCMAKE_POSITION_INDEPENDENT_CODE=OFF \
        -DCMAKE_C_FLAGS="$EXTRA_FLAGS -DAT_HWCAP2=26" \
        -DCMAKE_CXX_FLAGS="$EXTRA_FLAGS -DAT_HWCAP2=26 -DARM_COMPUTE_EXCEPTIONS_DISABLED -DARM_COMPUTE_NO_EXCEPTIONS" \
        -DACL_ARCH_ISA=armv8-a -DACL_MULTI_ISA=OFF \
        -DARM_COMPUTE_BUILD_SHARED_LIB=OFF -DARM_COMPUTE_BUILD_EXAMPLES=OFF \
        -DARM_COMPUTE_BUILD_TESTING=OFF -DARM_COMPUTE_ENABLE_OPENMP=OFF \
        -DARM_COMPUTE_ENABLE_CPPTHREADS=OFF -DARM_COMPUTE_ENABLE_LOGGING=OFF \
        -DARM_COMPUTE_ENABLE_ASSERTS=OFF -DARM_COMPUTE_ENABLE_WERROR=OFF \
        -DARM_COMPUTE_CCXX_FLAGS_INIT= -DARM_COMPUTE_CCXX_FLAGS_RELEASE= \
        -DARM_COMPUTE_CCXX_FLAGS=
    "$CMAKE" --build "$CMAKE_BUILD" --target arm_compute -j "$BUILD_JOBS"
    LIBRARY="$CMAKE_BUILD/libarm_compute_armv8-a.a"
    [ -f "$LIBRARY" ] || die "CMake 没有生成 libarm_compute_armv8-a.a"
    BUILD_MODE="static_linux_candidate"
fi

cp "$LIBRARY" "$OUTPUT_DIR/libarm_compute_armv8-a.a"
LIBRARY="$OUTPUT_DIR/libarm_compute_armv8-a.a"
UNDEFINED_ALL="$OUTPUT_DIR/undefined-symbols-all.txt"
DEFINED_ALL="$OUTPUT_DIR/defined-symbols-all.txt"
UNDEFINED_SYMBOLS="$OUTPUT_DIR/undefined-symbols.txt"
DEFINED_SYMBOLS="$OUTPUT_DIR/defined-symbols.txt"
ELF_HEADERS="$OUTPUT_DIR/elf-headers.txt"
SYMBOL_AUDIT="$OUTPUT_DIR/symbol-audit.txt"
"$NM" -u "$LIBRARY" | sed -n 's/^.* [UuWw] \([^ ]*\)$/\1/p' | sort -u > "$UNDEFINED_ALL"
"$NM" -g --defined-only "$LIBRARY" | sed -n 's/^.* [A-Za-z] \([^ ]*\)$/\1/p' | sort -u > "$DEFINED_ALL"
# 静态 archive 的 raw undefined 集合还包含 archive 内部跨 object 的引用。
# 只有从定义集合中扣除后剩下的 external 集合，才是最终链接时必须提供的 ABI。
UNDEFINED_ALL="$UNDEFINED_ALL" DEFINED_ALL="$DEFINED_ALL" UNDEFINED_SYMBOLS="$UNDEFINED_SYMBOLS" DEFINED_SYMBOLS="$DEFINED_SYMBOLS" python3 - <<'PY'
import os
import pathlib

undefined_path = pathlib.Path(os.environ["UNDEFINED_ALL"])
defined_path = pathlib.Path(os.environ["DEFINED_ALL"])
undefined = {line.strip() for line in undefined_path.read_text().splitlines() if line.strip()}
defined = {line.strip() for line in defined_path.read_text().splitlines() if line.strip()}
external = sorted(undefined - defined)
undefined_path.parent.joinpath("undefined-symbols.txt").write_text("".join(f"{item}\n" for item in external))
defined_path.parent.joinpath("defined-symbols.txt").write_text("".join(f"{item}\n" for item in sorted(defined)))
PY
"$READELF" -h -S "$LIBRARY" > "$ELF_HEADERS"
FORBIDDEN_REGEX='pthread|GOMP_|(^|_)omp_|dlopen|dlsym|mmap|munmap|futex|syscall|(^|_)open$|(^|_)read$|(^|_)write$|fopen|fclose|fileno|fprintf|printf|stdout|stderr|getauxval|sched_|sysconf|localtime|strftime|regcomp|regexec|_ZNSt6thread|_ZNSt18condition_variable|_ZNSt12__basic_file'
FORBIDDEN_MATCHES="$(grep -Ein "$FORBIDDEN_REGEX" "$UNDEFINED_SYMBOLS" || true)"
{
    echo "ACL v$VERSION static archive audit"
    echo "library=$LIBRARY"
    echo "build_mode=$BUILD_MODE"
    echo "raw_undefined_symbol_count=$(wc -l < "$UNDEFINED_ALL" | tr -d ' ')"
    echo "external_undefined_symbol_count=$(wc -l < "$UNDEFINED_SYMBOLS" | tr -d ' ')"
    echo "defined_symbol_count=$(wc -l < "$DEFINED_SYMBOLS" | tr -d ' ')"
    if [ -n "$FORBIDDEN_MATCHES" ]; then
        echo "forbidden_status=FOUND"
        echo "$FORBIDDEN_MATCHES"
    else
        echo "forbidden_status=NONE_IN_ARCHIVE_UNDEFINED_SET"
    fi
} > "$SYMBOL_AUDIT"

LIBRARY_SHA256="$(hash_file "$LIBRARY")"
LIBRARY_BYTES="$(wc -c < "$LIBRARY" | tr -d ' ' | tr -d '\\n')"
MANIFEST_PATH="$MANIFEST_PATH" BUILD_MODE="$BUILD_MODE" LIBRARY_SHA256="$LIBRARY_SHA256" LIBRARY_BYTES="$LIBRARY_BYTES" UNDEFINED_ALL="$UNDEFINED_ALL" UNDEFINED_SYMBOLS="$UNDEFINED_SYMBOLS" SYMBOL_AUDIT="$SYMBOL_AUDIT" python3 - <<'PY'
import json
import os
import pathlib

path = pathlib.Path(os.environ["MANIFEST_PATH"])
document = json.loads(path.read_text())
document["build_mode"] = os.environ["BUILD_MODE"]
document["artifacts"] = {
    "static_library": {
        "sha256": os.environ["LIBRARY_SHA256"],
        "bytes": int(os.environ["LIBRARY_BYTES"]),
    },
    "undefined_symbols_all": os.environ["UNDEFINED_ALL"],
    "undefined_symbols": os.environ["UNDEFINED_SYMBOLS"],
    "symbol_audit": os.environ["SYMBOL_AUDIT"],
}
symbol_audit = pathlib.Path(os.environ["SYMBOL_AUDIT"]).read_text()
document["audit"] = {
    "forbidden_regex": "pthread|GOMP_|(^|_)omp_|dlopen|dlsym|mmap|munmap|futex|syscall|(^|_)open$|(^|_)read$|(^|_)write$|fopen|fclose|fileno|fprintf|printf|stdout|stderr|getauxval|sched_|sysconf|localtime|strftime|regcomp|regexec|_ZNSt6thread|_ZNSt18condition_variable|_ZNSt12__basic_file",
    "forbidden_matches": "forbidden_status=FOUND" in symbol_audit,
    "undefined_symbol_count": sum(1 for line in pathlib.Path(os.environ["UNDEFINED_SYMBOLS"]).read_text().splitlines() if line.strip()),
}
path.write_text(json.dumps(document, indent=2, ensure_ascii=False) + "\n")
PY

echo "ACL v$VERSION static build complete"
echo "  mode:     $BUILD_MODE"
echo "  library:  $LIBRARY"
echo "  manifest: $MANIFEST_PATH"
echo "  audit:    $SYMBOL_AUDIT"
if [ -n "$FORBIDDEN_MATCHES" ]; then
    echo "WARNING: archive undefined-symbol set contains forbidden runtime symbols; see $SYMBOL_AUDIT" >&2
    exit 2
fi
