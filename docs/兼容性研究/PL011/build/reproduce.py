#!/usr/bin/env python3
"""在独立 Docker Linux 卷中重建 PL011；大产物只导出到用户缓存。"""
import argparse
import hashlib
import lzma
from pathlib import Path
import shutil
import subprocess

HERE = Path(__file__).resolve().parent
parser = argparse.ArgumentParser()
parser.add_argument('--cache', type=Path, default=Path.home() / '.cache/exokernel-linux-v6.12')
parser.add_argument('--container', default='exokernel-pl011-v612')
args = parser.parse_args()
cache = args.cache.expanduser().resolve()
container = args.container
downloads = cache / 'downloads'
downloads.mkdir(parents=True, exist_ok=True)


def run(cmd, **kwargs):
    print('+', ' '.join(map(str, cmd)), flush=True)
    return subprocess.run(list(map(str, cmd)), check=True, **kwargs)


archive = downloads / 'linux-6.12.tar.xz'
if not archive.exists():
    run(['curl', '-fL', '--retry', '2', 'https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.12.tar.xz', '-o', archive])
expected = 'b1a2562be56e42afb3f8489d4c2a7ac472ac23098f1ef1c1e40da601f54625eb'
with archive.open('rb') as stream:
    assert hashlib.file_digest(stream, 'sha256').hexdigest() == expected, 'Linux archive hash mismatch'

exists = subprocess.run(['docker', 'container', 'inspect', container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0
if not exists:
    run(['docker', 'build', '--platform', 'linux/arm64', '-t', 'exokernel-pl011-toolchain:6.12', HERE])
    run(['docker', 'create', '--name', container, '--label', 'task=exokernel-pl011-v612',
         '--mount', 'type=volume,src=exokernel-linux-v612,dst=/work',
         '--mount', f'type=bind,src={downloads},dst=/downloads,readonly',
         'exokernel-pl011-toolchain:6.12'])
else:
    label = subprocess.check_output(['docker', 'inspect', '--format', '{{index .Config.Labels "task"}}', container], text=True).strip()
    assert label == 'exokernel-pl011-v612', '拒绝复用其他任务的同名容器'
run(['docker', 'start', container], stdout=subprocess.DEVNULL)
run(['docker', 'exec', container, 'clang', '--version'])

source_exists = subprocess.run(['docker', 'exec', container, 'test', '-f', '/work/linux-6.12/Makefile']).returncode == 0
if not source_exists:
    proc = subprocess.Popen(['docker', 'exec', '-i', container, 'tar', '-xf', '-', '-C', '/work'], stdin=subprocess.PIPE)
    with lzma.open(archive, 'rb') as stream:
        shutil.copyfileobj(stream, proc.stdin, 8 * 1024 * 1024)
    proc.stdin.close()
    assert proc.wait() == 0, 'Linux source extraction failed'
run(['docker', 'exec', container, 'mkdir', '-p', '/work/recipe'])
run(['docker', 'cp', str(HERE) + '/.', container + ':/work/recipe/'])
run(['docker', 'exec', container, 'bash', '/work/recipe/build_linux.sh'])
run(['docker', 'exec', container, 'python3', '/work/recipe/analyze_build.py',
     '/work/artifacts/pio', '/work/analysis/pio', '/work/linux-6.12', '/work/build-pl011-pio'])
for kind in ['artifacts', 'analysis']:
    target = cache / kind / 'pio'
    target.mkdir(parents=True, exist_ok=True)
    run(['docker', 'cp', f'{container}:/work/{kind}/pio/.', target])
print(f'原始编译证据保存在 {cache}；容器 {container} 保持可复用。', flush=True)
