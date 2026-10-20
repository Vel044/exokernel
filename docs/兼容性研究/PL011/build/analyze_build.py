#!/usr/bin/env python3
"""提取实际目标文件边界、编译头文件、IR 调用和内联汇编证据。"""
import csv
import hashlib
import json
from pathlib import Path
import re
import sys

artifacts = Path(sys.argv[1])
dest = Path(sys.argv[2])
linux = Path(sys.argv[3])
build = Path(sys.argv[4])
dest.mkdir(parents=True, exist_ok=True)


def write_csv(name, rows, fields):
    with (dest / name).open('w', encoding='utf-8-sig', newline='') as f:
        w = csv.DictWriter(f, fieldnames=fields)
        w.writeheader()
        w.writerows(rows)


def nm_names(name):
    return {line.split()[-1]: line.split()[-2] for line in
            (artifacts / name).read_text().splitlines() if len(line.split()) >= 2}


pl011 = nm_names('amba-pl011.undefined.txt')
core = nm_names('serial_base.undefined.txt')
combined = nm_names('pl011-with-serial-base.undefined.txt')
core_defs = set(nm_names('serial_base.defined.txt'))
ir = (artifacts / 'amba-pl011.ll').read_text()
metadata = dict(re.findall(r'^!(\d+) = (.*)$', ir, re.M))


def origin(mid):
    seen = set()
    line = ''
    while mid and mid not in seen:
        seen.add(mid)
        item = metadata.get(mid, '')
        if not line:
            match = re.search(r'\bline: (\d+)', item)
            if match:
                line = match[1]
        filename = re.search(r'filename: "([^"]+)"', item)
        if filename:
            directory = re.search(r'directory: "([^"]*)"', item)
            name = Path(filename[1])
            if not name.is_absolute() and directory:
                name = Path(directory[1]) / name
            name = str(name)
            prefix = str(linux) + '/'
            if name.startswith(prefix):
                name = name[len(prefix):]
            return name, line
        nxt = re.search(r'\bfile: !(\d+)', item) or re.search(r'\bscope: !(\d+)', item)
        mid = nxt[1] if nxt else None
    return '', line


calls, indirect, assembly = [], [], []
function = ''
defined_functions = set()
for ir_line, line in enumerate(ir.splitlines(), 1):
    definition = re.match(r'define\b.*?@([\w.$]+)\(', line)
    if definition:
        function = definition[1]
        defined_functions.add(function)
    if line == '}':
        function = ''
    if not function or not re.search(r'\b(?:call|invoke|callbr)\b', line):
        continue
    dbg = re.search(r'!dbg !(\d+)', line)
    source, source_line = origin(dbg[1] if dbg else None)
    common = dict(caller=function, source=source, line=source_line, ir_line=ir_line)
    if ' asm ' in line:
        assembly.append(dict(**common, instruction=line.strip()))
        continue
    symbol = re.search(r'@([\w.$]+)\(', line)
    if symbol:
        if symbol[1] in pl011:
            calls.append(dict(**common, symbol=symbol[1]))
    else:
        indirect.append(dict(**common, instruction=line.strip()))

write_csv('external-callsites.csv', calls, ['symbol', 'caller', 'source', 'line', 'ir_line'])
write_csv('indirect-calls.csv', indirect, ['caller', 'source', 'line', 'ir_line', 'instruction'])
write_csv('inline-assembly.csv', assembly, ['caller', 'source', 'line', 'ir_line', 'instruction'])

all_symbols = sorted(set(pl011) | set(core) | set(combined))
exports = {s: [] for s in all_symbols}
export_pattern = re.compile(r'EXPORT_SYMBOL\w*\s*\(\s*(' + '|'.join(map(re.escape, all_symbols)) + r')\s*[,)]')
# 此处仅定位上游导出声明，不把它误称为已经编译的提供者或传递依赖闭包。
for subtree in ['arch/arm64', 'drivers', 'kernel', 'lib', 'mm', 'fs']:
    for path in (linux / subtree).rglob('*'):
        if path.suffix not in {'.c', '.h', '.S'} or not path.is_file():
            continue
        text = path.read_text(errors='replace')
        if 'EXPORT_SYMBOL' not in text:
            continue
        for match in export_pattern.finditer(text):
            exports[match[1]].append(f'{path.relative_to(linux)}:{text[:match.start()].count(chr(10)) + 1}')

boundary = []
for symbol in all_symbols:
    declaration = re.search(r'^declare\b[^\n]*@' + re.escape(symbol) + r'\(', ir, re.M)
    global_decl = re.search(r'^@' + re.escape(symbol) + r'\s*=\s*[^\n]*\bexternal\b', ir, re.M)
    kind = 'function' if declaration else ('global' if global_decl else '见目标文件/非PL011直接符号')
    refs = [f"{r['caller']}@{r['source']}:{r['line']}" for r in calls if r['symbol'] == symbol]
    boundary.append(dict(symbol=symbol, kind=kind, pl011_undefined=symbol in pl011,
                         serial_base_undefined=symbol in core,
                         resolved_by_serial_base=symbol in pl011 and symbol in core_defs,
                         combined_undefined=symbol in combined,
                         linux_exports=';'.join(exports[symbol]), callsites=';'.join(refs)))
write_csv('symbol-boundaries.csv', boundary, list(boundary[0]))

preprocessed = build / 'drivers/tty/serial/amba-pl011.i'
headers = []
for name in sorted(set(re.findall(r'^#\s+\d+ "([^"<>]+)"', preprocessed.read_text(), re.M))):
    path = Path(name)
    if not path.is_absolute():
        path = build / path
    if not path.is_file():
        continue
    resolved = path.resolve()
    if resolved.is_relative_to(linux):
        category, rel = 'linux-source', str(resolved.relative_to(linux))
    elif resolved.is_relative_to(build):
        category, rel = 'generated', str(resolved.relative_to(build))
    else:
        category, rel = 'toolchain', str(resolved)
    headers.append(dict(category=category, path=rel, bytes=path.stat().st_size,
                        sha256=hashlib.sha256(path.read_bytes()).hexdigest()))
write_csv('preprocessor-files.csv', headers, ['category', 'path', 'bytes', 'sha256'])

driver = (linux / 'drivers/tty/serial/amba-pl011.c').read_text()
ops_body = re.search(r'static const struct uart_ops amba_pl011_pops = \{(.*?)\n\};', driver, re.S)[1]
effective_ops = re.search(r'\bamba_pl011_pops\s*=\s*\{(.*?)\};', preprocessed.read_text(), re.S)[1]
effective_fields = set(re.findall(r'\.(\w+)\s*=', effective_ops))
callbacks = []
for field, fn in re.findall(r'\.(\w+)\s*=\s*(\w+)', ops_body):
    callbacks.append(dict(field=field, source_symbol=fn,
                          field_in_preprocessed=field in effective_fields,
                          compiled_function=field in effective_fields and fn in defined_functions))
write_csv('uart-ops.csv', callbacks, ['field', 'source_symbol', 'field_in_preprocessed', 'compiled_function'])

layout_names = {'uart_amba_port', 'uart_port', 'uart_state', 'tty_port', 'uart_ops',
                'vendor_data', 'amba_device', 'resource', 'ktermios'}
layout_parts, layouts = [], []
for part in (artifacts / 'record-layouts.log').read_text().split('*** Dumping AST Record Layout'):
    name = re.search(r'^\s*0 \| struct (\w+)\s*$', part, re.M)
    size = re.findall(r'\[sizeof=(\d+), align=(\d+)\]', part)
    if name and name[1] in layout_names and size:
        layout_parts.append('*** Dumping AST Record Layout' + part)
        layouts.append(dict(name=name[1], size=size[-1][0], alignment=size[-1][1]))
write_csv('key-type-layouts.csv', layouts, ['name', 'size', 'alignment'])
(dest / 'key-type-layouts.txt').write_text(''.join(layout_parts))

summary = dict(pl011_undefined=len(pl011), serial_base_undefined=len(core),
               combined_undefined=len(combined),
               direct_resolved_by_core=sorted(set(pl011) & core_defs),
               combined_new_vs_pl011=sorted(set(combined) - set(pl011)),
               headers_or_sources=len(headers), direct_external_call_sites=len(calls),
               indirect_call_sites=len(indirect), inline_assembly_sites=len(assembly),
               compiled_driver_functions=len(defined_functions),
               compiled_ops=[c for c in callbacks if c['compiled_function']],
               asm_daif_sites=sum('daif' in a['instruction'] for a in assembly),
               asm_alternative_sites=sum('altinstructions' in a['instruction'] for a in assembly),
               key_type_layouts=layouts,
               source_sha256=hashlib.sha256((linux / 'drivers/tty/serial/amba-pl011.c').read_bytes()).hexdigest(),
               config_sha256=hashlib.sha256((build / '.config').read_bytes()).hexdigest())
(dest / 'summary.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2) + '\n')
print(json.dumps(summary, ensure_ascii=False, indent=2))
