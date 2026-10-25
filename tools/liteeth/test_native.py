#!/usr/bin/env python3
"""测试实际 Rust 状态机的协议、所有权和恶意设备边界；使用公共同名 Rust 封装，不链接 C 兼容层。"""
from pathlib import Path
import os,subprocess,json,re
root=Path(__file__).resolve().parents[2]
expected=set(json.loads((root/'docs/LiteEth/源码核查.json').read_text())['functions'])
translated='\n'.join((root/name).read_text() for name in ['libos/src/drivers/liteeth_core.rs','libos/src/drivers/liteeth.rs'])
provided=set(re.findall(r'\bfn\s+(liteeth_\w+)\s*(?:<[^>]*>)?\s*\(',translated))
if expected != provided:
    raise SystemExit(f'原驱动函数名称不一致：缺少 {sorted(expected-provided)}，多出 {sorted(provided-expected)}')
print(f'PASS {len(expected)} 个原驱动函数全部保留同名 Rust 实现',flush=True)
out=root/'target/liteeth-native-tests';out.mkdir(parents=True,exist_ok=True)
rustc=Path.home()/'.cargo/bin/rustc'
subprocess.run([str(rustc) if rustc.exists() else 'rustc','--edition=2021','--test',str(root/'tests/liteeth_native.rs'),'-o',str(out/'test_native')],cwd=root,check=True)
subprocess.run([str(out/'test_native'),'--test-threads=1'],check=True)
