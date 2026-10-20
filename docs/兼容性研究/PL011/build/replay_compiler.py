#!/usr/bin/env python3
"""沿用 Kbuild 实际编译参数生成 LLVM IR、宏快照和结构体布局。"""
import json
from pathlib import Path
import shlex
import subprocess
import sys

artifacts, output = map(Path, sys.argv[1:3])
entries = json.loads((artifacts / 'compile_commands.json').read_text())
entry = next(x for x in entries if x['file'].endswith('/amba-pl011.c'))
args = shlex.split(entry['command'])
filtered = []
i = 0
while i < len(args):
    arg = args[i]
    if arg == '-o':
        i += 2
        continue
    if arg == '-c' or arg.startswith('-Wp,-MMD,'):
        i += 1
        continue
    filtered.append(arg)
    i += 1

recipes = {
    'llvm-ir': filtered + ['-S', '-emit-llvm', '-o', str(artifacts / 'amba-pl011.ll')],
    'macros': filtered + ['-E', '-dM', '-o', str(artifacts / 'effective-macros.txt')],
    'record-layouts': filtered + ['-fsyntax-only', '-Xclang', '-fdump-record-layouts-complete'],
}
for name, cmd in recipes.items():
    with (artifacts / f'{name}.log').open('w') as log:
        subprocess.run(cmd, cwd=entry['directory'], stdout=log, stderr=subprocess.STDOUT, check=True)
(artifacts / 'replay-commands.json').write_text(json.dumps(recipes, indent=2) + '\n')
print('Exact-command replay produced LLVM IR, effective macros, record layouts')
