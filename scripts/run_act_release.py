#!/usr/bin/env python3
"""隔离的EL0 ACT运行器；按本次进程组清理，完成结果及EL0 exit均必须存在。"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import shutil
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def source_digest():
    files = subprocess.check_output(['git','ls-files','--cached','--others','--exclude-standard','-z'], cwd=ROOT).decode().split('\0')
    entries = {name:sha(ROOT/name) for name in sorted(set(files)) if name and (ROOT/name).is_file() and name.endswith(('.rs','.toml','.ld','.py','.sh','.cpp','.h'))}
    return hashlib.sha256(json.dumps(entries,sort_keys=True).encode()).hexdigest()


def run(args):
    out = Path(tempfile.mkdtemp(prefix=f'act-release-{args.provider}-{args.case}-', dir=ROOT/'target'))
    case = ROOT.parent / 'data/act/correctness' / ('case-'+args.case)
    run_id = uuid.uuid4().hex
    env = dict(os.environ, LIBOS_APP='act-benchmark', ACT_KERNEL_PROVIDER=args.provider,
               ACT_RELEASE_RUN_ID=run_id,
               ACT_RUN_PHASE='correctness' if args.phase == 'correctness' else 'benchmark',
               ACT_CONTINUOUS_CHECK='1' if args.phase == 'stability' else '0', CARGO_NET_OFFLINE='true')
    if args.phase == 'profile': env['ACT_PROFILE_STAGES']='1'
    if args.target_cpu != 'generic':
        env['CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS'] = '-C link-arg=-Tlibos/link.ld -C target-cpu='+args.target_cpu
    manifest_sha = sha(ROOT/'vendor/rutorch/manifest.json')
    source_sha = source_digest()
    # 构建只选择纯推理应用，不会启动USB设备或真实执行器。
    with (out/'build.log').open('wb') as log:
        subprocess.run(['bash', 'build.sh'], cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    if sha(ROOT/'vendor/rutorch/manifest.json') != manifest_sha or source_digest() != source_sha:
        raise RuntimeError('构建期间vendored源码版本改变')
    image = out/'data.ext4'
    subprocess.run(['bash', 'scripts/build_act_benchmark_ext4.sh'], cwd=ROOT,
                   env=dict(env, ACT_OBSERVATION_DIR=str(case), ACT_FS_IMAGE=str(image)), check=True, stdout=subprocess.DEVNULL)
    esp = out/'esp.img'
    shutil.copyfile(ROOT/'qemu/esp.img', esp)
    partition_table = subprocess.check_output(['gpt', '-r', 'show', str(esp)], text=True)
    partition = re.search(r'^\s*(\d+)\s+\d+\s+1\s+', partition_table, re.MULTILINE)
    if not partition: raise RuntimeError('ESP缺少第一分区')
    subprocess.run(['mcopy', '-o', '-i', str(esp)+'@@'+str(int(partition[1])*512), str(ROOT/'target/aarch64-unknown-uefi/debug/BOOTAA64.efi'), '::/EFI/BOOT/BOOTAA64.EFI'], check=True)
    vars_path = out/'vars.fd'
    shutil.copyfile('/opt/homebrew/share/qemu/edk2-arm-vars.fd', vars_path)
    binary = ROOT/'libos/libos.elf'
    # 保存本次ELF，后续构建覆盖工作区产物也不会丢失二进制来源与审计证据。
    shutil.copyfile(binary, out/'libos.elf')
    with (out/'elf-audit.log').open('wb') as audit:
        subprocess.run(['bash','scripts/audit_libos_elf.sh',str(out/'libos.elf')],cwd=ROOT,stdout=audit,stderr=subprocess.STDOUT,check=True)
    assets = {str(path): sha(path) for path in [binary, ROOT/'target/aarch64-unknown-uefi/debug/BOOTAA64.efi', case/'handeye.rgb', case/'fixed.rgb', case/'state.f32le', case/'pytorch-action.f32le', ROOT.parent/'data/act/model/model.safetensors', ROOT.parent/'data/act/model/policy_preprocessor_step_3_normalizer_processor.safetensors']}
    request = dict(run_id=run_id, case=args.case, provider=args.provider, phase=args.phase, rust_target_cpu=args.target_cpu, assets=assets,
                   head=subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(), source_tree_sha256=source_sha, vendored_manifest_sha256=manifest_sha)
    (out/'request.json').write_text(json.dumps(request, indent=2)+'\n')
    command = ['qemu-system-aarch64', '-accel', 'hvf', '-M', 'virt', '-cpu', 'host', '-smp', '4', '-m', '4G',
               '-drive', 'if=pflash,format=raw,unit=0,readonly=on,file=/opt/homebrew/share/qemu/edk2-aarch64-code.fd',
               '-drive', f'if=pflash,format=raw,unit=1,file={vars_path}',
               '-drive', f'file={esp},format=raw,if=none,id=boot', '-device', 'virtio-blk-device,drive=boot,bootindex=0',
               '-drive', f'file={image},format=raw,if=none,id=actmodel,readonly=on', '-device', 'virtio-blk-device,drive=actmodel,bootindex=1',
               '-nographic', '-no-reboot', '-nic', 'none']
    log_path = out/'qemu.log'
    started = time.monotonic()
    completed = False
    with log_path.open('wb') as log:
        process = subprocess.Popen(command, cwd=out, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            while time.monotonic()-started < args.timeout:
                text = log_path.read_text(errors='replace')
                if 'ACT_ACTION_BITS_END' in text and 'EL0 exit code=0x0000000000000000' in text:
                    completed = True
                    break
                if 'EL0 exit code=' in text or process.poll() is not None:
                    break
                time.sleep(.1)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try: process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL); process.wait()
    (out/'process.json').write_text(json.dumps(dict(command=command, exit_code=process.returncode, completed=completed, seconds=time.monotonic()-started), indent=2)+'\n')
    print('ACT_RELEASE_OUT='+str(out), flush=True)
    if not completed:
        raise RuntimeError('guest未完整成功退出，保留本次日志')
    for path, expected in assets.items():
        if sha(path) != expected: raise RuntimeError('来源资产在运行期间改变')
    if sha(ROOT/'vendor/rutorch/manifest.json') != manifest_sha or source_digest() != source_sha:
        raise RuntimeError('运行期间vendored源码版本改变')
    spec = importlib.util.spec_from_file_location('act_evidence', ROOT/'vendor/rutorch/tools/act_release.py')
    evidence = importlib.util.module_from_spec(spec); spec.loader.exec_module(evidence)
    raw, result = evidence.parse_libos(log_path, case/'pytorch-action.f32le', args.phase=='stability', 1 if args.phase=='correctness' else 10, expected_run_id=run_id)
    result.update(request=request, statistics=evidence.stats(result['model_samples_ms']))
    (out/'action.f32le').write_bytes(raw)
    (out/'verification.json').write_text(json.dumps(result, indent=2)+'\n')
    print('ACT_RELEASE_PASS=true', flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--provider', choices=('acl','portable'), required=True)
    parser.add_argument('--case', choices=('000','001','002','003','004'), default='000')
    parser.add_argument('--phase', choices=('correctness','benchmark','stability','profile'), default='correctness')
    parser.add_argument('--timeout', type=float, default=1200)
    parser.add_argument('--target-cpu', choices=('generic','cortex-a76'), default='generic')
    run(parser.parse_args())
