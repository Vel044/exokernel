#!/usr/bin/env bash
# 在原生 Linux AArch64 容器内运行；源码和大产物均置于仓库外。
set -euo pipefail
recipe_dir="$(cd "$(dirname "$0")" && pwd)"
linux_source="${PL011_LINUX_SOURCE:-/work/linux-6.12}"
build_output="${PL011_BUILD_OUTPUT:-/work/build-pl011-pio}"
artifacts="${PL011_ARTIFACTS:-/work/artifacts/pio}"
mkdir -p "$build_output" "$artifacts"
export KBUILD_BUILD_USER=exokernel KBUILD_BUILD_HOST=pl011-lab
export KBUILD_BUILD_TIMESTAMP='2024-11-17 00:00:00 UTC' KBUILD_BUILD_VERSION=1

build_make=(make -C "$linux_source" O="$build_output" ARCH=arm64 LLVM=1 KCFLAGS=-g)
"${build_make[@]}" KCONFIG_ALLCONFIG="$recipe_dir/pio.fragment" allnoconfig > "$artifacts/configure.log" 2>&1
python3 - "$build_output/.config" <<'PY'
import sys
from pathlib import Path
s=Path(sys.argv[1]).read_text()
for name in ['SMP','TTY','ARM_AMBA','COMMON_CLK','SERIAL_CORE','SERIAL_AMBA_PL011']:
    assert f'CONFIG_{name}=y\n' in s, name
for name in ['DMA_ENGINE','SERIAL_AMBA_PL011_CONSOLE','CONSOLE_POLL','PM','ACPI','PINCTRL']:
    assert f'CONFIG_{name}=y\n' not in s and f'CONFIG_{name}=m\n' not in s, name
print('Kconfig assertions passed')
PY
"${build_make[@]}" -j2 V=1 \
    drivers/tty/serial/amba-pl011.o \
    drivers/tty/serial/amba-pl011.i \
    drivers/tty/serial/amba-pl011.s > "$artifacts/build.log" 2>&1
# 内建模式下 Kbuild 将 serial_base-y 的四个对象直接放入 thin archive。
"${build_make[@]}" -j2 V=1 drivers/tty/serial/ >> "$artifacts/build.log" 2>&1
cp "$build_output/.config" "$artifacts/linux.config"
cp "$recipe_dir/pio.fragment" "$artifacts/requested.fragment"
cp "$build_output/drivers/tty/serial/.amba-pl011.o.cmd" "$artifacts/amba-pl011.o.cmd"
for suffix in o i s; do
    cp "$build_output/drivers/tty/serial/amba-pl011.$suffix" "$artifacts/amba-pl011.$suffix"
done
python3 "$linux_source/scripts/clang-tools/gen_compile_commands.py" -d "$build_output" -o "$artifacts/compile_commands.json"

python3 "$recipe_dir/replay_compiler.py" "$artifacts" "$build_output"
# 显式部分链接这四个真实 Kbuild 对象，仅用于比较兼容边界，不称为完整内核链接。
ld.lld -r "$build_output/drivers/tty/serial/serial_core.o" \
    "$build_output/drivers/tty/serial/serial_base_bus.o" \
    "$build_output/drivers/tty/serial/serial_ctrl.o" \
    "$build_output/drivers/tty/serial/serial_port.o" -o "$artifacts/serial_base.o"
for obj in amba-pl011 serial_base; do
    object_path="$build_output/drivers/tty/serial/$obj.o"
    if [ "$obj" = serial_base ]; then object_path="$artifacts/serial_base.o"; fi
    llvm-nm --undefined-only "$object_path" > "$artifacts/$obj.undefined.txt"
    llvm-nm --defined-only "$object_path" > "$artifacts/$obj.defined.txt"
    llvm-readelf -h -S -s -r "$object_path" > "$artifacts/$obj.readelf.txt"
done
# 把串口框架和驱动部分链接，量化“导入 serial_core”仍然留下的外部依赖。
ld.lld -r "$build_output/drivers/tty/serial/amba-pl011.o" \
    "$artifacts/serial_base.o" -o "$artifacts/pl011-with-serial-base.o"
llvm-nm --undefined-only "$artifacts/pl011-with-serial-base.o" > "$artifacts/pl011-with-serial-base.undefined.txt"
llvm-objdump -dr "$build_output/drivers/tty/serial/amba-pl011.o" > "$artifacts/amba-pl011.disassembly.txt"
dpkg-query -W > "$artifacts/packages.tsv"
{
    uname -a
    clang --version
    ld.lld --version
    make --version
} > "$artifacts/toolchain.txt"
printf 'PL011 and serial_base object builds finished; artifacts: %s\n' "$artifacts"
