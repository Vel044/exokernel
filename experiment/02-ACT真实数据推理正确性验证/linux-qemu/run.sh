#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BUILD_DIR="${LINUX_QEMU_BUILD_DIR:-$SCRIPT_DIR/build}"
LOG_FILE="${LINUX_QEMU_LOG:-$BUILD_DIR/linux-qemu.log}"

for path in \
    "$BUILD_DIR/kernel/Image" \
    "$BUILD_DIR/initramfs.cpio.gz" \
    "$BUILD_DIR/act-linux.ext4"; do
    test -f "$path" || { echo "missing $path; run linux-qemu/build.sh first" >&2; exit 1; }
done

echo "==> 启动QEMU AArch64 Linux正确性实验"
echo "    log: $LOG_FILE"
set +e
qemu-system-aarch64 \
    -machine virt,gic-version=2 \
    -cpu cortex-a76 \
    -accel tcg,thread=multi \
    -smp 4 \
    -m 4G \
    -nographic \
    -no-reboot \
    -kernel "$BUILD_DIR/kernel/Image" \
    -initrd "$BUILD_DIR/initramfs.cpio.gz" \
    -append "console=ttyAMA0 rdinit=/init panic=-1 loglevel=4" \
    -drive if=none,format=raw,file="$BUILD_DIR/act-linux.ext4",id=actdata \
    -device virtio-blk-device,drive=actdata \
    2>&1 | tee "$LOG_FILE"
qemu_status=${PIPESTATUS[0]}
set -e

grep -q '\[linux-rust\] PASS: 5 cases / 3000 values' "$LOG_FILE" || {
    echo "QEMU Linux correctness test did not report PASS" >&2
    exit 1
}
exit "$qemu_status"
