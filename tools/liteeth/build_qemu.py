#!/usr/bin/env python3
"""给独立 QEMU 10.2.1 源码应用 LiteEth 实验模型，并编译；不改系统 QEMU。"""
import argparse
from pathlib import Path
import shutil
import subprocess

HERE=Path(__file__).resolve().parent
p=argparse.ArgumentParser()
p.add_argument('--source',type=Path,required=True,help='已解压的 QEMU 10.2.1 源码')
p.add_argument('--build',type=Path,required=True,help='独立生成物目录')
p.add_argument('-j',default='8')
a=p.parse_args()
src=a.source.resolve();build=a.build.resolve()
if '10.2.1' != (src/'VERSION').read_text().strip():
    raise SystemExit('本补丁只验证 QEMU 10.2.1')
shutil.copyfile(HERE/'exo_liteeth.c',src/'hw/net/exo_liteeth.c')
f=src/'hw/net/meson.build';text=f.read_text()
line="system_ss.add(when: 'CONFIG_ARM_VIRT', if_true: files('exo_liteeth.c'))"
if line not in text:f.write_text(text+'\n'+line+'\n')
f=src/'hw/arm/virt.c';text=f.read_text()
if 'static void create_exo_liteeth(' not in text:
    marker='static void create_platform_bus(VirtMachineState *vms)'
    if text.count(marker)!=1:raise SystemExit('virt.c 不匹配')
    text=text.replace(marker,(HERE/'virt-hook.c.inc').read_text()+'\n'+marker)
    text=text.replace('    create_platform_bus(vms);','    create_platform_bus(vms);\n    create_exo_liteeth(vms);')
    f.write_text(text)
build.mkdir(parents=True,exist_ok=True)
if not (build/'build.ninja').exists():
    subprocess.run([str(src/'configure'),'--target-list=aarch64-softmmu','--disable-docs','--disable-werror','--disable-gtk','--disable-sdl','--disable-cocoa','--disable-opengl','--disable-vnc','--disable-tools','--disable-guest-agent','--disable-slirp','--disable-pvg'],cwd=build,check=True)
subprocess.run(['ninja','-j',a.j,'qemu-system-aarch64'],cwd=build,check=True)
print(build/'qemu-system-aarch64')
