#!/usr/bin/env python3
"""测试实际 Rust 状态机的协议、所有权和恶意设备边界；使用公共同名 Rust 封装，不链接 C 兼容层。"""
from pathlib import Path
import os,subprocess,json,re
root=Path(__file__).resolve().parents[2]
expected=set(json.loads((root/'docs/LiteEth/源码核查.json').read_text())['functions'])
translated=(root/'libos/src/drivers/liteeth.rs').read_text()
provided=set(re.findall(r'\bfn\s+(liteeth_\w+)\s*(?:<[^>]*>)?\s*\(',translated))
if expected != provided:
    raise SystemExit(f'原驱动函数名称不一致：缺少 {sorted(expected-provided)}，多出 {sorted(provided-expected)}')
print(f'PASS {len(expected)} 个原驱动函数全部保留同名 Rust 实现',flush=True)
original=(root/'libos/vendor/liteeth-linux/litex_liteeth.c').read_text()
registers={name:int(value,16) for name,value in re.findall(r'^#define\s+(LITEETH_\w+)\s+(0x[0-9a-fA-F]+)\s*$',original,re.M)}
rust_registers={name:int(value,16) for name,value in re.findall(r'pub const (LITEETH_\w+): usize = (0x[0-9a-fA-F]+);',translated)}
if registers != rust_registers:
    raise SystemExit('原驱动寄存器名称或偏移与 Rust 不一致')
if not re.search(r'pub const DRV_NAME: &str = "liteeth";',translated):
    raise SystemExit('DRV_NAME 必须保留原名和原值')
for name in ['RX_SLOT','RX_LENGTH','RX_PENDING','RX_ENABLE','TX_START','TX_READY','TX_SLOT','TX_LENGTH','TX_PENDING','TX_ENABLE']:
    if re.search(r'pub const '+name+r'\b',translated):
        raise SystemExit(f'禁止将原寄存器改名为 {name}')
all_types=translated+'\n'+'\n'.join(path.read_text() for path in (root/'libos/src/linux_api').glob('*.rs'))
for name in ['liteeth','net_device','sk_buff','rtnl_link_stats64','pcpu_sw_netstats','platform_device','platform_driver']:
    if not re.search(r'pub struct '+name+r'\b',all_types):
        raise SystemExit(f'原类型名未保留：{name}')
print(f'PASS {len(registers)} 个寄存器及 DRV_NAME 原名原值、7 个原类型名保留',flush=True)
out=root/'target/liteeth-native-tests';out.mkdir(parents=True,exist_ok=True)
rustc=Path.home()/'.cargo/bin/rustc'
subprocess.run([str(rustc) if rustc.exists() else 'rustc','--edition=2021','--test',str(root/'tests/liteeth_native.rs'),'-o',str(out/'test_native')],cwd=root,check=True)
subprocess.run([str(out/'test_native'),'--test-threads=1'],check=True)
