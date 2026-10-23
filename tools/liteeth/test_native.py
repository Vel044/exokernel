#!/usr/bin/env python3
"""测试实际 Rust 状态机的协议、所有权和恶意设备边界；使用公共同名 Rust 封装，不链接 C 兼容层。"""
from pathlib import Path
import os,subprocess
root=Path(__file__).resolve().parents[2]
out=root/'target/liteeth-native-tests';out.mkdir(parents=True,exist_ok=True)
rustc=Path.home()/'.cargo/bin/rustc'
subprocess.run([str(rustc) if rustc.exists() else 'rustc','--edition=2021','--test',str(root/'tests/liteeth_native.rs'),'-o',str(out/'test_native')],cwd=root,check=True)
subprocess.run([str(out/'test_native'),'--test-threads=1'],check=True)
