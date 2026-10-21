#!/usr/bin/env python3
"""离线提取固定 Linux ARM64 调用表和当前 exokernel ABI；不修改源码。"""
import csv
import hashlib
import json
import re
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
SOURCES = HERE / "sources"


def write_csv(name, fields, rows):
    with (HERE / name).open("w", encoding="utf-8-sig", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)


def main():
    # ARM64 Makefile 添加 renameat/rlimit/memfd_secret；通用构建选择 common/64。
    abis = {"common", "64", "renameat", "rlimit", "memfd_secret"}
    linux = []
    for line in (SOURCES / "linux-v6.12-syscall.tbl").read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if parts[1] in abis:
            number, abi, name, entry = parts[:4]
            linux.append(dict(number=int(number), name=name, abi=abi, entry=entry,
                              status="占位：sys_ni_syscall" if entry == "sys_ni_syscall"
                              else "已分配编号；可用性取决于架构实现与配置"))
    abi_path = ROOT / "abi/src/lib.rs"
    dispatch_path = ROOT / "kernel/src/syscall/dispatch.rs"
    source = abi_path.read_text()
    exo = []
    for match in re.finditer(r"pub const (SYS_(?!ERR_)[A-Z_]+): u64 = (\d+);", source):
        exo.append(dict(number=int(match[2]), name=match[1],
                        source="abi/src/lib.rs", line=source[:match.start()].count("\n") + 1))
    dispatch = dispatch_path.read_text()
    handled = set(re.findall(r"\b(SYS_[A-Z_]+)\s*=>", dispatch))
    assert {row["name"] for row in exo} == handled, "ABI 与分发器不一致，需人工核查"
    assert len({row["number"] for row in exo}) == len(exo)
    assert len({row["number"] for row in linux}) == len(linux)
    assert next(x["number"] for x in linux if x["name"] == "read") == 63
    assert next(x["number"] for x in linux if x["name"] == "mmap") == 222
    linux.sort(key=lambda row: row["number"])
    exo.sort(key=lambda row: row["number"])
    write_csv("linux-arm64-v6.12-syscalls.csv", ["number", "name", "abi", "entry", "status"], linux)
    write_csv("exokernel-syscalls.csv", ["number", "name", "source", "line"], exo)
    text = ["# Linux v6.12 ARM64 原生系统调用完整编号表", "",
            "由 generate_inventory.py 生成。只统计 AArch64，不含 AArch32 compat。",
            f"共 {len(linux)} 个具名编号，其中 2 个显式指向 sys_ni_syscall。最大编号 462；463 是编号空间上界，不是实现数量。",
            "其余项也不保证在任意内核配置上可用。保留空洞不列为调用。Linux 版本固定为 v6.12，不代表调研日最新版本。", "",
            "| 编号 | 名称 | 内核入口 | ABI 筛选标签 |", "| --- | --- | --- | --- |"]
    text += [f"| {r['number']} | `{r['name']}` | `{r['entry']}` | {r['abi']} |" for r in linux]
    (HERE / "Linux系统调用完整清单.md").write_text("\n".join(text) + "\n")
    # 启动结构与资源授权语义同样纳入来源哈希，不能只更新 syscall 编号。
    context_files = [ROOT / name for name in (
        "kernel/src/boot/handoff.rs", "kernel/src/kmain.rs", "kernel/src/resource/resources.rs",
        "kernel/src/object/task.rs", "kernel/src/scheduler/priority.rs", "libos/src/main.rs",
        "libos/src/drivers/dtb.rs", "libos/src/drivers/platform.rs", "libos/src/drivers/pci_host.rs",
    )]
    files = [abi_path, dispatch_path, *context_files, *sorted(SOURCES.glob("*"))]
    manifest = dict(baseline="Linux v6.12 / AArch64 native", verified_on="2026-10-21",
                    exokernel_boot_info_version=int(re.search(r"USER_BOOT_INFO_VERSION: u16 = (\d+);", source)[1]),
                    abis=sorted(abis),
                    linux_named_numbers=len(linux), exokernel_calls=len(exo),
                    sha256={str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest()
                            for p in files if p.is_file()})
    (HERE / "inventory-manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
    print(f"Linux: {len(linux)} named numbers; exokernel: {len(exo)} calls; dispatcher cross-check passed")


if __name__ == "__main__":
    main()
