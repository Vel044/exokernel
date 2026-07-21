#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"
# 优先使用 rustup shim；它会按 libos/rust-toolchain.toml 选择 nightly，
# 同时避免 Homebrew rustc/cargo 与 rust-objcopy 使用不同 LLVM 版本。
export PATH="$HOME/.cargo/bin:$PATH"

echo "==> 1. 编译 libos (EL0)"
cd libos
LIBOS_FEATURES="${LIBOS_FEATURES:-qemu-xhci,scservo}"
echo "    libOS features: ${LIBOS_FEATURES}"
cargo build --release --target aarch64-unknown-none --no-default-features --features "${LIBOS_FEATURES}"
cd ..

echo ""
echo "==> 2. strip → libos.elf"
rust-objcopy --strip-debug target/aarch64-unknown-none/release/libos libos/libos.elf
echo "    libos.elf: $(wc -c < libos/libos.elf) bytes"

echo ""
echo "==> 3. 编译 exokernel (EL2 boot shim + EL1 kernel)"
cargo build -p exokernel --target aarch64-unknown-uefi

echo ""
echo "==> 完成!"
echo "    kernel: target/aarch64-unknown-uefi/debug/BOOTAA64.efi"
echo "    libos:  libos/libos.elf (embedded in .efi)"
