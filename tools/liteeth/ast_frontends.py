#!/usr/bin/env python3
"""真实解析器入口：Clang 使用 Linux 头文件；syn 保留 Rust 语法树与位置。"""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_LINUX = ROOT.parent / "linux"
ENTRIES = ["liteeth_rx", "liteeth_interrupt", "liteeth_open", "liteeth_stop",
           "liteeth_start_xmit", "liteeth_get_stats64", "liteeth_setup_slots",
           "liteeth_probe", "liteeth_remove"]


class ParseError(RuntimeError):
    pass


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def run(args, **kwargs):
    result = subprocess.run([str(x) for x in args], capture_output=True, **kwargs)
    if result.returncode:
        raise ParseError(f"解析器退出 {result.returncode}: {' '.join(map(str, args[:5]))}\n"
                         + result.stderr.decode(errors="replace")[-12000:])
    return result


def json_stream(raw):
    decoder = json.JSONDecoder()
    items, offset = [], 0
    while offset < len(raw):
        while offset < len(raw) and raw[offset].isspace():
            offset += 1
        if offset == len(raw):
            break
        item, offset = decoder.raw_decode(raw, offset)
        items.append(item)
    return items


def clang_environment(linux=DEFAULT_LINUX, work=None):
    linux = Path(linux).resolve()
    work = Path(work or ROOT / "target/liteeth-ast/environment").resolve()
    work.mkdir(parents=True, exist_ok=True)
    generated = work / "include/generated"
    generated.mkdir(parents=True, exist_ok=True)
    clang = (os.environ.get("LITEETH_CLANG") or
             shutil.which("clang", path="/opt/homebrew/opt/llvm/bin:" + os.environ.get("PATH", "")))
    if not clang:
        raise ParseError("缺少 Clang；可通过 LITEETH_CLANG 指定")
    config = linux / "include/generated/autoconf.h"
    if not config.exists() or "#define CONFIG_ARM64 1" not in config.read_text():
        raise ParseError("Linux 源码需先生成 ARM64 的 .config 和 include/generated/autoconf.h")
    args = [clang, "--target=aarch64-linux-gnu", "-std=gnu11", "-D__KERNEL__", "-DMODULE",
            '-DKBUILD_MODNAME="liteeth"', '-DKBUILD_BASENAME="litex_liteeth"',
            "-include", linux / "include/linux/kconfig.h",
            "-include", linux / "include/linux/compiler_types.h"]
    for path in [work / "include", linux / "arch/arm64/include",
                 linux / "arch/arm64/include/generated", linux / "include",
                 linux / "arch/arm64/include/uapi", linux / "arch/arm64/include/generated/uapi",
                 linux / "include/uapi", linux / "include/generated/uapi"]:
        args += ["-I", path]
    args += ["-Wno-unknown-warning-option", "-Wno-gnu-variable-sized-type-not-at-end",
             "-Wno-address-of-packed-member"]
    # 生成头放在 target，绝不改 Linux 工作区；每次按当前头文件重新生成，避免缓存过期。
    version = run([clang, "--version"]).stdout.decode().splitlines()[0]
    stamp = {str(p): digest(p) for p in [config, linux / "kernel/bounds.c", linux / "kernel/time/timeconst.bc"]}
    stamp["clang"] = version
    stamp_path = work / "stamp.json"
    match = re.search(r"^#define CONFIG_HZ (\d+)$", config.read_text(), re.M)
    if not match:
        raise ParseError("Linux 配置缺少 CONFIG_HZ")
    timeconst = run(["bc", "-q", linux / "kernel/time/timeconst.bc"], input=(match[1] + "\n").encode())
    (generated / "timeconst.h").write_bytes(timeconst.stdout)
    # bounds.c 的内联汇编输出是 Linux 原有生成协议，不是人工伪造的常量。
    assembly = run(args + ["-S", linux / "kernel/bounds.c", "-o", "-"]).stdout.decode()
    bounds = re.findall(r'\.ascii\s+"->([A-Z_a-z0-9]+) ([0-9]+) ', assembly)
    if not bounds:
        raise ParseError("无法读取 Linux bounds.c 的生成输出")
    (generated / "bounds.h").write_text("/* 由 Linux kernel/bounds.c 生成 */\n" +
                                         "".join(f"#define {key} {value}\n" for key, value in bounds))
    stamp_path.write_text(json.dumps(stamp, indent=2))
    return dict(linux=linux, work=work, args=list(map(str, args)), clang=version,
                config_sha256=digest(config), generated=stamp)


def c_ast(source, environment, output):
    source, output = Path(source).resolve(), Path(output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    text = source.read_text()
    constant_names = re.findall(r"^\s*#\s*define\s+(LITEETH_\w+)\b", text, re.M)
    # 将宏常量放入真正的 C 常量表达式，由 Clang 求值；不在 Python 中 eval 源码。
    constants = constant_names + ["NET_RX_DROP", "NETDEV_TX_OK", "NETDEV_TX_BUSY", "IRQ_HANDLED", "ENOMEM"]
    wrapper = output / "translation-unit.c"
    wrapper.write_text('#include ' + json.dumps(str(source), ensure_ascii=False) + "\n" +
                       "enum liteeth_ast_constants {\n" +
                       ",\n".join(f"liteeth_ast_value_{name} = {name}" for name in constants) + "\n};\n" +
                       "static const char *liteeth_ast_drv_name = DRV_NAME;\n")
    args = environment["args"] + ["-fsyntax-only", "-MD", "-MF", str(output / "includes.d"),
                                   "-Xclang", "-ast-dump=json", "-Xclang", "-ast-dump-filter=liteeth", str(wrapper)]
    result = run(args)
    (output / "clang.raw.json").write_bytes(result.stdout)
    (output / "clang.stderr.txt").write_bytes(result.stderr)
    roots = json_stream(result.stdout.decode())
    # module 元数据的真实声明名不含 liteeth，需要独立取出，不能只扫源文件字符串。
    module_args = environment["args"] + ["-fsyntax-only", "-Xclang", "-ast-dump=json", "-Xclang",
                                          "-ast-dump-filter=__UNIQUE_ID", str(wrapper)]
    module_result = run(module_args)
    (output / "clang.module.raw.json").write_bytes(module_result.stdout)
    roots += json_stream(module_result.stdout.decode())
    (output / "clang.roots.json").write_text(json.dumps(roots, ensure_ascii=False))
    # 记录本次真正读到的头文件，便于回查编译环境和宏实现；不把哈希当作相等证明。
    dependency_text = (output / "includes.d").read_text().replace("\\\n", " ")
    dependencies = dependency_text.split(": ", 1)[1].split()
    includes = {p: digest(p) for p in dependencies if Path(p).is_file()}
    metadata = {"frontend": "clang", "version": environment["clang"], "target": "aarch64-linux-gnu",
                "typechecked": True, "source": str(source), "sha256": digest(source),
                "command": args, "config_sha256": environment["config_sha256"], "includes": includes}
    (output / "clang.manifest.json").write_text(json.dumps(metadata, ensure_ascii=False, indent=2))
    return roots, metadata


def build_syn():
    manifest = ROOT / "tools/liteeth/rust_ast/Cargo.toml"
    build = ROOT / "target/liteeth-ast/frontend"
    run(["cargo", "+stable", "build", "--locked", "--offline", "--manifest-path", manifest,
         "--target-dir", build])
    return build / "debug/liteeth-rust-ast"


def rust_ast(paths, output, binary=None):
    paths = [Path(p).resolve() for p in paths]
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    binary = binary or build_syn()
    result = run([binary] + paths)
    (output / "syn.raw.json").write_bytes(result.stdout)
    data = json.loads(result.stdout)
    data["inputs"] = {str(p): digest(p) for p in paths}
    return data
