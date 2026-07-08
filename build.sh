#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"

echo "==> 1. 编译 libos (EL0)"
cd libos
cargo build --target aarch64-unknown-none
cd ..

echo ""
echo "==> 2. objcopy → libos.bin"
rust-objcopy -O binary target/aarch64-unknown-none/debug/libos libos/libos.bin
echo "    libos.bin: $(wc -c < libos/libos.bin) bytes"

echo ""
echo "==> 3. 编译 exokernel (EL2 boot shim + EL1 kernel)"
cargo build -p exokernel --target aarch64-unknown-uefi

echo ""
echo "==> 完成!"
echo "    kernel: target/aarch64-unknown-uefi/debug/BOOTAA64.efi"
echo "    libos:  libos/libos.bin (embedded in .efi)"
