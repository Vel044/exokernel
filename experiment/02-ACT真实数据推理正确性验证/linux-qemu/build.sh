#!/bin/bash
set -euo pipefail

# 构建可由实验02和实验03共同复用的AArch64 QEMU Linux环境。
export PATH="$HOME/.cargo/bin:$PATH"
ROOT_DIR="$(cd "$(dirname "$0")/../../.." && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT_DIR/.." && pwd)"
SCRIPT_DIR="$ROOT_DIR/experiment/02-ACT真实数据推理正确性验证/linux-qemu"
BUILD_DIR="${LINUX_QEMU_BUILD_DIR:-$SCRIPT_DIR/build}"
MODEL_DIR="${ACT_MODEL_DIR:-$WORKSPACE_ROOT/data/act/model}"
CASES_DIR="${ACT_CASES_DIR:-$WORKSPACE_ROOT/data/act/correctness}"
TARGET=aarch64-unknown-linux-musl
ALPINE_VERSION=3.22.5
ALPINE_ARCHIVE="alpine-minirootfs-${ALPINE_VERSION}-aarch64.tar.gz"
ALPINE_URL="https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/aarch64/$ALPINE_ARCHIVE"
ALPINE_NETBOOT_URL="https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/aarch64/netboot-${ALPINE_VERSION}"

for path in \
    "$MODEL_DIR/model.safetensors" \
    "$MODEL_DIR/policy_preprocessor_step_3_normalizer_processor.safetensors" \
    "$CASES_DIR/case-000/pytorch-action.f32le"; do
    test -f "$path" || { echo "missing required asset: $path" >&2; exit 1; }
done

mkdir -p "$BUILD_DIR/downloads" "$BUILD_DIR/kernel"
rm -rf "$BUILD_DIR/rootfs"
mkdir -p "$BUILD_DIR/rootfs"

echo "==> 编译AArch64 Linux Rust runner"
rustup target add "$TARGET"
RUSTFLAGS="${RUSTFLAGS:-} -C linker=rust-lld -C target-cpu=cortex-a76" \
    cargo build --release --target "$TARGET" \
    --manifest-path "$ROOT_DIR/act-runtime/Cargo.toml" \
    --example linux_correctness
RUNNER="$ROOT_DIR/target/$TARGET/release/examples/linux_correctness"

echo "==> 获取固定版本的QEMU virt AArch64 Linux Kernel"
# Darwin缺少Linux Kernel host工具需要的ELF headers，因此直接采用Alpine发布的
# AArch64 virt Kernel。模型推理仍在后续QEMU guest内运行，不在宿主或容器运行。
for file in vmlinuz-virt config-6.12.94-0-virt modloop-virt; do
    if [[ ! -f "$BUILD_DIR/downloads/$file" ]]; then
        curl -fL "$ALPINE_NETBOOT_URL/$file" -o "$BUILD_DIR/downloads/$file"
    fi
done
cp "$BUILD_DIR/downloads/vmlinuz-virt" "$BUILD_DIR/kernel/Image"
cp "$BUILD_DIR/downloads/config-6.12.94-0-virt" "$BUILD_DIR/kernel/config"
shasum -a 256 "$BUILD_DIR/kernel/Image" > "$BUILD_DIR/kernel/Image.sha256"

echo "==> 创建最小initramfs"
if [[ ! -f "$BUILD_DIR/downloads/$ALPINE_ARCHIVE" ]]; then
    curl -fL "$ALPINE_URL" -o "$BUILD_DIR/downloads/$ALPINE_ARCHIVE"
fi
rm -rf "$BUILD_DIR/rootfs"
mkdir -p "$BUILD_DIR/rootfs"
tar -xzf "$BUILD_DIR/downloads/$ALPINE_ARCHIVE" -C "$BUILD_DIR/rootfs"
# modloop是Alpine发布Kernel精确匹配的SquashFS模块包。7-Zip在macOS上会对
# SquashFS尾部元数据返回非零，但目标modules目录完整时可安全继续。
MODULE_DIR="$BUILD_DIR/modules"
rm -rf "$MODULE_DIR"
mkdir -p "$MODULE_DIR"
7z x -o"$MODULE_DIR" "$BUILD_DIR/downloads/modloop-virt" >/dev/null 2>&1 || true
test -f "$MODULE_DIR/modules/6.12.94-0-virt/kernel/fs/ext4/ext4.ko" || {
    echo "failed to extract Alpine Kernel modules" >&2
    exit 1
}
mkdir -p "$BUILD_DIR/rootfs/lib"
cp -R "$MODULE_DIR/modules" "$BUILD_DIR/rootfs/lib/modules"
install -m 0755 "$RUNNER" "$BUILD_DIR/rootfs/usr/bin/linux-act-correctness"
install -m 0755 "$SCRIPT_DIR/init" "$BUILD_DIR/rootfs/init"
(
    cd "$BUILD_DIR/rootfs"
    find . -print0 | cpio --null -o --format=newc 2>/dev/null | gzip -1 \
        > "$BUILD_DIR/initramfs.cpio.gz"
)

echo "==> 创建ACT实验ext4数据盘"
DATA_IMAGE="$BUILD_DIR/act-linux.ext4"
rm -f "$DATA_IMAGE"
truncate -s 384M "$DATA_IMAGE"
/opt/homebrew/opt/e2fsprogs/sbin/mkfs.ext4 -q -F "$DATA_IMAGE"
DEBUGFS=/opt/homebrew/opt/e2fsprogs/sbin/debugfs
"$DEBUGFS" -w -R "mkdir /cases" "$DATA_IMAGE" >/dev/null
"$DEBUGFS" -w -R "write $MODEL_DIR/model.safetensors /model.safetensors" "$DATA_IMAGE" >/dev/null
"$DEBUGFS" -w -R "write $MODEL_DIR/policy_preprocessor_step_3_normalizer_processor.safetensors /policy_preprocessor_step_3_normalizer_processor.safetensors" "$DATA_IMAGE" >/dev/null
for case_index in 000 001 002 003 004; do
    "$DEBUGFS" -w -R "mkdir /cases/case-$case_index" "$DATA_IMAGE" >/dev/null
    for file in handeye.rgb fixed.rgb state.f32le pytorch-action.f32le manifest.json; do
        "$DEBUGFS" -w -R "write $CASES_DIR/case-$case_index/$file /cases/case-$case_index/$file" \
            "$DATA_IMAGE" >/dev/null
    done
done

echo "Linux Image: $BUILD_DIR/kernel/Image"
echo "initramfs:    $BUILD_DIR/initramfs.cpio.gz"
echo "ACT disk:     $DATA_IMAGE"
