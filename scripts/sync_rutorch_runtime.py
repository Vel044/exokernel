#!/usr/bin/env python3
"""同步受审计的第一方运行时和ACL微内核；快照不再依赖相邻源码或Linux动态库。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def inventory(root):
    return {str(p.relative_to(root)): sha(p) for p in sorted(root.rglob('*')) if p.is_file() and p.name not in ('manifest.json', 'Cargo.lock') and not any(part in ('target', '.git', '__pycache__') for part in p.relative_to(root).parts) and p.suffix != '.pyc'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=ROOT.parent / 'rutorch')
    parser.add_argument('--verify', action='store_true')
    args = parser.parse_args()
    destination = ROOT / 'vendor/rutorch'
    if args.verify:
        saved = json.loads((destination / 'manifest.json').read_text())
        if saved['files'] != inventory(destination):
            raise SystemExit('vendored源码摘要不一致')
        print('VENDORED_RUNTIME_PASS=true')
        return
    if destination.exists():
        saved = json.loads((destination / 'manifest.json').read_text())
        if saved['files'] != inventory(destination):
            raise SystemExit('快照有人工修改；保留现场，拒绝覆盖')
    source = args.source.resolve()
    destination.parent.mkdir(exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix='rutorch-sync-', dir=ROOT / 'target'))
    for crate in ('rutorch-runtime', 'rutorch-kernels-acl-shared', 'rutorch-kernels-acl'):
        shutil.copytree(source / 'crates' / crate, staging / 'crates' / crate, ignore=shutil.ignore_patterns('target', '__pycache__', 'Cargo.lock'))
        manifest = staging / 'crates' / crate / 'Cargo.toml'
        text = manifest.read_text().replace('rust-version.workspace = true', 'rust-version = "1.96"').replace('version.workspace = true', 'version = "0.1.0"').replace('edition.workspace = true', 'edition = "2024"')
        manifest.write_text(text)
    acl = source / 'qemu/build/compute-library-v52.7.0'
    # 上游ACL源码逐字节复制，不补注释、不改算子；第一方build.rs选择此只读快照。
    kernel = acl / 'src/core/NEON/kernels/arm_gemm/kernels/a64_sgemm_8x12/generic.cpp'
    dependencies = subprocess.check_output(['aarch64-linux-gnu-g++', '-MM', '-mcpu=cortex-a76', '-DENABLE_NEON', '-DARM_COMPUTE_ENABLE_NEON', '-DBARE_METAL', '-I'+str(acl/'src/core'), '-I'+str(acl/'src/core/NEON/kernels/arm_gemm'), str(kernel)], text=True)
    for name in shlex.split(dependencies.split(':',1)[1].replace('\\\n',' ')):
        path = Path(name).resolve()
        if path.is_relative_to(acl.resolve()):
            target = staging / 'acl' / path.relative_to(acl.resolve())
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
    shutil.copytree(acl / 'LICENSES', staging / 'acl/LICENSES')
    (staging / 'tools').mkdir()
    shutil.copyfile(source / 'python/release/act_release.py', staging / 'tools/act_release.py')
    files = inventory(staging)
    provenance = dict(upstream_head=subprocess.check_output(['git', '-C', str(source), 'rev-parse', 'HEAD'], text=True).strip(), upstream_diff_sha256=hashlib.sha256(subprocess.check_output(['git', '-C', str(source), 'diff', 'HEAD'])).hexdigest(), acl_version='52.7.0', source_tree_sha256=hashlib.sha256(json.dumps(files, sort_keys=True).encode()).hexdigest(), files=files)
    (staging / 'manifest.json').write_text(json.dumps(provenance, indent=2)+'\n')
    if destination.exists():
        backup = Path(tempfile.mkdtemp(prefix='rutorch-old-', dir=ROOT / 'target')) / 'snapshot'
        os.rename(destination, backup)
    os.rename(staging, destination)
    print('VENDORED_RUNTIME_SHA=' + provenance['source_tree_sha256'])


if __name__ == '__main__':
    main()
