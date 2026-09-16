#!/bin/bash

# 把面向使用者的“平台 + 场景”二元选择收敛为唯一Cargo feature集合。
# 调用方只能设置LIBOS_APP；底层feature不再作为公开构建接口。
resolve_libos_features() {
    local platform="$1"
    local app="$2"
    local enable_xhci="${3:-0}"
    local xhci_feature=""
    local acl_feature=""

    case "${ACT_KERNEL_PROVIDER:-portable}" in
        portable) ;;
        acl) acl_feature=",acl-neon" ;;
        *)
            echo "ERROR: ACT_KERNEL_PROVIDER必须为portable或acl" >&2
            return 1
            ;;
    esac

    case "$platform" in
        qemu) xhci_feature="qemu-xhci" ;;
        pi5) xhci_feature="pi5-xhci" ;;
        *)
            echo "ERROR: unknown libOS platform: $platform" >&2
            return 1
            ;;
    esac

    case "$app" in
        system-smoke)
            if [ "$enable_xhci" = "1" ]; then
                printf 'app-system-smoke,%s\n' "$xhci_feature"
            else
                printf 'app-system-smoke\n'
            fi
            ;;
        process-smoke) printf 'app-process-smoke\n' ;;
        uart-echo) printf 'app-uart-echo\n' ;;
        usb-echo) printf 'app-usb-echo,%s\n' "$xhci_feature" ;;
        uvc-smoke) printf 'app-uvc-smoke,%s\n' "$xhci_feature" ;;
        scservo) printf 'app-scservo,%s\n' "$xhci_feature" ;;
        scservo-move) printf 'app-scservo-move,%s\n' "$xhci_feature" ;;
        act-inference)
            if [ "$platform" != "qemu" ]; then
                echo "ERROR: act-inference当前只支持QEMU virtio-blk后端。" >&2
                return 1
            fi
            printf 'app-act-inference%s\n' "$acl_feature"
            ;;
        act-benchmark)
            if [ "$platform" != "qemu" ]; then
                echo "ERROR: act-benchmark当前只支持QEMU virtio-blk后端。" >&2
                return 1
            fi
            printf 'app-act-benchmark%s\n' "$acl_feature"
            ;;
        robot-act-once)
            if [ "$platform" != "qemu" ]; then
                echo "ERROR: robot-act-once当前只支持QEMU真实USB直通后端。" >&2
                return 1
            fi
            printf 'app-robot-act-once%s\n' "$acl_feature"
            ;;
        robot-observation)
            if [ "$platform" != "qemu" ]; then
                echo "ERROR: robot-observation当前只支持QEMU真实USB直通后端。" >&2
                return 1
            fi
            printf 'app-robot-observation\n'
            ;;
        robot-action-replay)
            if [ "$platform" != "qemu" ]; then
                echo "ERROR: robot-action-replay当前只支持QEMU真实USB直通后端。" >&2
                return 1
            fi
            printf 'app-robot-action-replay\n'
            ;;
        *)
            echo "ERROR: 未知LIBOS_APP: $app" >&2
            return 1
            ;;
    esac
}
