#!/usr/bin/env python3
"""核查原 LiteEth 49 个同名接口覆盖，并测试公共 Rust 接口的独立复用与资源语义。"""
from pathlib import Path
import json,re,subprocess
root=Path(__file__).resolve().parents[2]
source=root/'docs/LiteEth/源码核查.json'
expected={name for group in json.loads(source.read_text())['groups'] for name in group['names']}
provided={name for path in (root/'libos/src/linux_api').glob('*.rs') for match in re.findall(r'pub (?:unsafe )?(?:const )?fn (\w+)|macro_rules!\s*(\w+)',path.read_text()) for name in match if name}
missing=expected-provided
if missing:raise SystemExit(f'缺少同名封装：{sorted(missing)}')
print(f'PASS {len(expected)} 个同名 Rust 接口完整覆盖',flush=True)
out=root/'target/linux-api-tests';out.mkdir(parents=True,exist_ok=True)
rustc=Path.home()/'.cargo/bin/rustc'
subprocess.run([str(rustc) if rustc.exists() else 'rustc','--edition=2021','--test',str(root/'tests/linux_api.rs'),'-o',str(out/'test_linux_api')],cwd=root,check=True)
subprocess.run([str(out/'test_linux_api'),'--test-threads=1'],check=True)
