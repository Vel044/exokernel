#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"
# 优先使用 rustup shim；它会按 libos/rust-toolchain.toml 选择 nightly，
# 同时避免 Homebrew rustc/cargo 与 rust-objcopy 使用不同 LLVM 版本。
export PATH="$HOME/.cargo/bin:$PATH"

select_objcopy() {
    if [ -n "${OBJCOPY:-}" ]; then
        printf '%s\n' "$OBJCOPY"
    elif [ -x /opt/homebrew/opt/llvm/bin/llvm-objcopy ]; then
        # macOS Homebrew Rust与LLVM版本可能不同；直接使用独立llvm-objcopy
        # 处理EL0 ELF，不加载rustc_driver，因此不会发生动态库版本冲突。
        printf '%s\n' /opt/homebrew/opt/llvm/bin/llvm-objcopy
    elif command -v llvm-objcopy >/dev/null 2>&1; then
        command -v llvm-objcopy
    else
        command -v rust-objcopy
    fi
}

echo "==> 1. 编译 libos (EL0)"
cd libos
if [ -n "${LIBOS_FEATURES:-}" ]; then
    echo "ERROR: LIBOS_FEATURES已不再是公开接口，请改用LIBOS_APP。" >&2
    exit 1
fi
LIBOS_APP="${LIBOS_APP:-scservo}"
source ../scripts/libos_features.sh
RESOLVED_LIBOS_FEATURES="$(resolve_libos_features qemu "$LIBOS_APP" "${LIBOS_ENABLE_XHCI:-0}")"
echo "    libOS app:      ${LIBOS_APP}"
echo "    cargo features: ${RESOLVED_LIBOS_FEATURES}"
cargo build --release --target aarch64-unknown-none --no-default-features --features "${RESOLVED_LIBOS_FEATURES}"
cd ..

echo ""
echo "==> 2. strip → libos.elf"
OBJCOPY_BIN="$(select_objcopy)"
"$OBJCOPY_BIN" --strip-debug target/aarch64-unknown-none/release/libos libos/libos.elf
echo "    libos.elf: $(wc -c < libos/libos.elf) bytes"

echo ""
echo "==> 3. 编译 exokernel (EL2 boot shim + EL1 kernel)"
cargo build -p exokernel --target aarch64-unknown-uefi

echo ""
echo "==> 完成!"
echo "    kernel: target/aarch64-unknown-uefi/debug/BOOTAA64.efi"
echo "    libos:  libos/libos.elf (embedded in .efi)"
