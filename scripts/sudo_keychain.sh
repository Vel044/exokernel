#!/bin/bash

# 从 macOS Keychain 取一次 sudo 密码，只用于建立短期 sudo ticket。
# 密码不写入仓库、命令行参数、shell 历史或长期环境变量。
sudo_keychain_prepare() {
    if [[ "${OSTYPE:-}" != darwin* ]]; then
        echo "ERROR: Keychain sudo helper 只支持 macOS。" >&2
        return 1
    fi

    local service="${SUDO_KEYCHAIN_SERVICE:-exokernel-sudo}"
    local account="${SUDO_KEYCHAIN_ACCOUNT:-${USER:-$(id -un)}}"
    local helper

    if ! security find-generic-password -a "$account" -s "$service" -w >/dev/null 2>&1; then
        echo "ERROR: 找不到 macOS Keychain 条目。" >&2
        echo "请先执行：" >&2
        echo "  security add-generic-password -a \"$account\" -s \"$service\" -w" >&2
        return 1
    fi

    helper="$(mktemp "${TMPDIR:-/tmp}/exokernel-askpass.XXXXXX")"
    chmod 700 "$helper"
    printf '%s\n' \
        '#!/bin/bash' \
        'security find-generic-password -a "${SUDO_KEYCHAIN_ACCOUNT:-${USER:-$(id -un)}}" -s "${SUDO_KEYCHAIN_SERVICE:-exokernel-sudo}" -w' \
        > "$helper"
    export SUDO_ASKPASS="$helper"

    if ! sudo -A -v; then
        rm -f "$helper"
        unset SUDO_ASKPASS
        echo "ERROR: 无法使用 Keychain 建立 sudo ticket。" >&2
        return 1
    fi

    rm -f "$helper"
    unset SUDO_ASKPASS
}
