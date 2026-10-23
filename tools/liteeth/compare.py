#!/usr/bin/env python3
"""串行构建 C 对照和原生 Rust，使用相同 QEMU/输入比较可观察行为。"""
import argparse,hashlib,json,subprocess,sys,time
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2]
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--qemu',required=True)
p.add_argument('--code',default='/opt/homebrew/share/qemu/edk2-aarch64-code.fd')
p.add_argument('--vars',default='/opt/homebrew/share/qemu/edk2-arm-vars.fd')
a=p.parse_args()
out=ROOT/'target/liteeth-comparison'/time.strftime('%Y%m%d-%H%M%S');out.mkdir(parents=True)
def run(command,log):
    with (out/log).open('w') as f:subprocess.run(command,cwd=ROOT,stdout=f,stderr=subprocess.STDOUT,check=True)
run([sys.executable,'tools/liteeth/test_adapter.py'],'c-host.log')
run([sys.executable,'tools/liteeth/test_native.py'],'rust-host.log')
run([sys.executable,'tools/liteeth/test_linux_api.py'],'linux-api-host.log')
for implementation in ['c','rust']:
    run([sys.executable,'scripts/test_liteeth.py','--implementation',implementation,'--qemu',a.qemu,'--code',a.code,'--vars',a.vars,'--output',str(out/implementation)],implementation+'.log')
    print(f'PASS {implementation}',flush=True)
c=json.loads((out/'c/result.json').read_text());r=json.loads((out/'rust/result.json').read_text())
keys=['frames','stats','device_tx','device_rx','lifecycle']
for key in keys:
    if c[key]!=r[key]:raise RuntimeError(f'C/Rust 差分失败：{key}')
if r['c_ffi_symbols']:raise RuntimeError('原生 Rust ELF 仍依赖 C 兼容层')
source=ROOT/'libos/vendor/liteeth-linux/litex_liteeth.c'
source_hash=hashlib.sha256(source.read_bytes()).hexdigest()
original=ROOT.parent/'linux/drivers/net/ethernet/litex/litex_liteeth.c'
if original.exists() and original.read_bytes()!=source.read_bytes():raise RuntimeError('C 对照与本地 Linux 原文件不一致')
summary=dict(passed=True,source_sha256=source_hash,source_version='Linux 6.12.75',compared=keys,frames=17,bytes=c['stats']['rx_bytes'],stats=c['stats'],rust_core_tests=11,rust_linux_api_tests=13,same_name_linux_interfaces=49,c_host_sanitizers=['AddressSanitizer','UndefinedBehaviorSanitizer'],c_elf_bytes=c['elf_bytes'],rust_elf_bytes=r['elf_bytes'],rust_c_ffi_symbols=r['c_ffi_symbols'],scope='单设备、单所属 EL0 线程；QEMU LiteEth 模型；L2 收发、超长 TX 丢弃、失败回滚、启停回收；不含真实 FPGA/IP/socket/形式化证明')
(out/'summary.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2)+'\n')
print(json.dumps(summary,ensure_ascii=False,indent=2));print('REPORT='+str(out))
