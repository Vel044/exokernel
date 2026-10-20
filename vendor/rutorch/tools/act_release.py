#!/usr/bin/env python3
"""ACT发布证据工具：冻结来源、严格读取EL0结果、统一统计，全部使用标准库。"""
import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import statistics
import struct
import subprocess

ROOT = Path(__file__).resolve().parents[2]
WORK = ROOT.parent


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def git(repo, *args):
    return subprocess.check_output(['git', '-C', str(repo), *args], text=True)


def freeze(output):
    repos = {}
    for name in ('rutorch', 'exokernel'):
        repo = WORK / name
        names = subprocess.check_output(['git', '-C', str(repo), 'ls-files', '--cached', '--others', '--exclude-standard', '-z']).decode().split('\0')
        files = {name: sha(repo / name) for name in sorted(set(names)) if name and (repo / name).is_file() and (name.endswith(('.rs', '.cpp', '.h', '.toml', '.lock', '.sh', '.py', '.ld')))}
        tree = hashlib.sha256(json.dumps(files, sort_keys=True).encode()).hexdigest()
        repos[repo.name] = dict(head=git(repo, 'rev-parse', 'HEAD').strip(), status=git(repo, 'status', '--short'), diff_sha256=hashlib.sha256(git(repo, 'diff', 'HEAD').encode()).hexdigest(), source_tree_sha256=tree, files=files)
    assets = {}
    model = WORK / 'data/act/model'
    for name in ('model.safetensors', 'policy_preprocessor_step_3_normalizer_processor.safetensors', 'policy_postprocessor_step_0_unnormalizer_processor.safetensors'):
        assets[str(model / name)] = sha(model / name)
    for index in range(5):
        case = WORK / f'data/act/correctness/case-{index:03}'
        for name in ('handeye.rgb', 'fixed.rgb', 'state.f32le', 'pytorch-action.f32le'):
            assets[str(case / name)] = sha(case / name)
    acl = ROOT / 'qemu/build/compute-library-v52.7.0'
    acl_files = {str(p.relative_to(acl)): sha(p) for p in sorted(acl.rglob('*')) if p.is_file() and p.suffix in ('.h', '.hpp', '.cpp', '.inl')}
    versions = {}
    for label, cmd in [('qemu', ['qemu-system-aarch64', '--version']), ('rust', ['rustup', 'run', '1.96.0', 'rustc', '--version']), ('cross_cxx', ['aarch64-linux-gnu-g++', '--version'])]:
        versions[label] = subprocess.check_output(cmd, text=True).splitlines()[0]
    Path(output).parent.mkdir(parents=True, exist_ok=True)
    Path(output).write_text(json.dumps(dict(repositories=repos, assets=assets, acl_source_tree_sha256=hashlib.sha256(json.dumps(acl_files, sort_keys=True).encode()).hexdigest(), tools=versions), indent=2) + '\n')


def compare(raw, golden):
    if len(raw) != 2400 or len(golden) != 2400:
        raise ValueError('actual/golden长度必须恰好2400字节')
    errors = []
    for index, (actual, want) in enumerate(zip(struct.unpack('<600f', raw), struct.unpack('<600f', golden))):
        if not math.isfinite(actual) or not math.isfinite(want) or abs(actual-want) > 1e-4 + 1e-5*abs(want):
            raise ValueError(f'value[{index}] actual={actual} golden={want}不通过')
        errors.append(abs(actual-want))
    return max(errors)


def parse_libos(log, golden, continuous=False, expected_samples=10, *, expected_run_id):
    text = Path(log).read_text(errors='strict')
    if re.findall(r'ACT_RELEASE_RUN_ID=([0-9a-z]+)', text) != [expected_run_id]:
        raise ValueError('guest run-id缺失、重复或来自旧启动')
    if text.count('ACT_ACTION_BITS_BEGIN count=600') != 1 or text.count('ACT_ACTION_BITS_END') != 1:
        raise ValueError('结果开始/完成标记必须各一次')
    if re.findall(r'EL0 exit code=(0x[0-9a-fA-F]+)', text) != ['0x0000000000000000']:
        raise ValueError('缺少唯一的EL0成功退出')
    values = {}
    for match in re.finditer(r'ACT_ACTION_BITS index=(\d+) bits=0x([0-9a-fA-F]+)', text):
        index, bits = int(match[1]), int(match[2], 16)
        if index in values or not 0 <= index < 600 or not 0 <= bits <= 0xffffffff:
            raise ValueError('动作索引重复/越界或非f32位模式')
        values[index] = struct.pack('<I', bits)
    if set(values) != set(range(600)):
        raise ValueError('动作索引必须精确为0..599')
    raw = b''.join(values[index] for index in range(600))
    error = compare(raw, Path(golden).read_bytes())
    if continuous and 'ACT continuous check passed runs=100' not in text:
        raise ValueError('100次逐次连续校验未完成')
    if 'ACT steady allocations=0' not in text:
        raise ValueError('稳态存在分配或未记录分配')
    samples = re.search(r'ACT_CORE_TICKS frequency=(\d+) samples=([0-9,]+)', text)
    if not samples or int(samples[1]) <= 0:
        raise ValueError('缺少模型核心原始ticks/frequency')
    timings = [int(item)*1000/int(samples[1]) for item in samples[2].rstrip(',').split(',')]
    if len(timings) != expected_samples or any(value <= 0 for value in timings):
        raise ValueError(f'必须有{expected_samples}个正模型样本')
    return raw, dict(values=600, max_abs_error=error, output_sha256=hashlib.sha256(raw).hexdigest(), model_samples_ms=timings)


def stats(samples):
    if not samples or any(not math.isfinite(n) or n <= 0 for n in samples):
        raise ValueError('性能样本必须有限且为正')
    ordered = sorted(samples)
    return dict(median_ms=statistics.median(ordered), p95_ms=ordered[math.ceil(.95*len(ordered))-1], samples_ms=samples)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('freeze'); p.add_argument('--output', required=True)
    p = sub.add_parser('verify-libos'); p.add_argument('--log', required=True); p.add_argument('--golden', required=True); p.add_argument('--output', required=True); p.add_argument('--run-id', required=True); p.add_argument('--continuous', action='store_true')
    args = parser.parse_args()
    if args.command == 'freeze':
        freeze(args.output)
    else:
        raw, result = parse_libos(args.log, args.golden, args.continuous, expected_run_id=args.run_id)
        Path(args.output).write_text(json.dumps(result, indent=2)+'\n')
        Path(args.output).with_suffix('.f32le').write_bytes(raw)


if __name__ == '__main__':
    main()
