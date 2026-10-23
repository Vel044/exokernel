#!/usr/bin/env python3
"""原 Linux LiteEth 驱动的 QEMU L2 端到端验收；生成物全部保存在 target。"""
import argparse,hashlib,json,os,re,shutil,socket,struct,subprocess,time
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser()
p.add_argument('--qemu',required=True,help='带 tools/liteeth 模型的 qemu-system-aarch64')
p.add_argument('--code',default='/opt/homebrew/share/qemu/edk2-aarch64-code.fd')
p.add_argument('--vars',default='/opt/homebrew/share/qemu/edk2-arm-vars.fd')
p.add_argument('--timeout',type=int,default=90)
p.add_argument('--skip-build',action='store_true')
p.add_argument('--implementation',choices=['c','rust'],default='c')
p.add_argument('--output',type=Path,help='独立结果目录，不得已经存在')
a=p.parse_args()
case=a.output.resolve() if a.output else ROOT/'target/liteeth-tests'/(time.strftime('%Y%m%d-%H%M%S')+'-'+a.implementation);case.mkdir(parents=True)
if not a.skip_build:
    env=dict(os.environ,LIBOS_APP='liteeth-smoke' if a.implementation=='c' else 'liteeth-rust-smoke',ACT_KERNEL_PROVIDER='portable')
    env.pop('LIBOS_FEATURES',None)
    with (case/'build.log').open('wb') as f:subprocess.run(['bash','build.sh'],cwd=ROOT,env=env,stdout=f,stderr=subprocess.STDOUT,check=True)
esp=case/'esp/EFI/BOOT';esp.mkdir(parents=True)
shutil.copyfile(ROOT/'target/aarch64-unknown-uefi/debug/BOOTAA64.efi',esp/'BOOTAA64.EFI')
shutil.copyfile(a.vars,case/'vars.fd')
shutil.copyfile(ROOT/'libos/libos.elf',case/'libos.elf')
nm=shutil.which('llvm-nm') or ('/opt/homebrew/opt/llvm/bin/llvm-nm' if Path('/opt/homebrew/opt/llvm/bin/llvm-nm').exists() else None)
if not nm:raise RuntimeError('需要 llvm-nm 验证 ELF 是否链接 C 兼容层')
symbols=subprocess.check_output([nm,str(case/'libos.elf')],text=True)
(case/'symbols.txt').write_text(symbols)
ffi=sorted(set(re.findall(r'\bexo_(?:lx|liteeth)_\w+',symbols)))
if a.implementation=='rust' and ffi:raise RuntimeError(f'Rust 版本仍链接 C 兼容层：{ffi}')
if a.implementation=='c' and 'exo_liteeth_send' not in ffi:raise RuntimeError('C 对照未链接原 C 驱动')
listener=socket.socket();listener.bind(('127.0.0.1',0));listener.listen(1);listener.settimeout(15)
port=listener.getsockname()[1];serial=case/'serial.log'
cmd=[str(Path(a.qemu).resolve()),'-accel','tcg','-M','virt,virtualization=on,acpi=off','-cpu','cortex-a76','-smp','4','-m','4G',
 '-drive',f'if=pflash,format=raw,unit=0,readonly=on,file={a.code}',
 '-drive',f'if=pflash,format=raw,unit=1,file={case}/vars.fd',
 '-drive',f'file=fat:rw:{case}/esp,format=raw,if=none,id=boot',
 '-device','virtio-blk-device,drive=boot,bootindex=0','-monitor','none','-display','none','-no-reboot','-nic','none',
 '-serial',f'file:{serial}','-global','exo-liteeth.trace=on','-netdev',f'socket,id=liteeth,connect=127.0.0.1:{port}']
(case/'command.json').write_text(json.dumps(cmd,indent=2)+'\n')
def text():return serial.read_bytes() if serial.exists() else b''
def wait_marker(marker):
    deadline=time.monotonic()+a.timeout
    while time.monotonic()<deadline:
        log=text()
        if b'[liteeth] FAIL' in log or b'fault' in log.lower() and b'[liteeth]' in log:
            raise RuntimeError(log[-3000:].decode(errors='replace'))
        if marker in log:return
        if proc.poll() is not None:raise RuntimeError('QEMU提前退出，请看 '+str(case/'qemu.log'))
        time.sleep(.05)
    raise TimeoutError(f'未出现 {marker!r}\n'+text()[-3000:].decode(errors='replace'))
def exact(n):
    data=bytearray()
    while len(data)<n:
        part=conn.recv(n-len(data))
        if not part:raise RuntimeError('网络连接关闭')
        data.extend(part)
    return bytes(data)
results=[]
with (case/'qemu.log').open('wb') as f:
    proc=subprocess.Popen(cmd,stdout=f,stderr=subprocess.STDOUT)
    try:
        conn,_=listener.accept();conn.settimeout(15)
        wait_marker(b'[liteeth] implementation='+ (b'c-reference' if a.implementation=='c' else b'rust-native'))
        wait_marker(b'[liteeth] READY')
        for i in range(17):
            if i==16:wait_marker(b'[liteeth] REOPEN')
            # 长短交替，覆盖两个 RX/TX slot、多次 IRQ 和近 MTU 帧；保留完整 L2 字节核对。
            length=[18,60,64,127,512,1024,1514,2048][i%8]
            payload=bytes.fromhex('0245584f000102112233445588b5')+i.to_bytes(4,'big')+bytes((j*31+i)&255 for j in range(length-18))
            conn.sendall(struct.pack('!I',len(payload))+payload)
            actual_length=struct.unpack('!I',exact(4))[0]
            if actual_length>65536:raise RuntimeError('backend 帧长度异常')
            actual=exact(actual_length)
            if actual!=payload:raise RuntimeError(f'第 {i} 帧不匹配：{len(actual)} vs {len(payload)}')
            results.append(dict(sequence=i,length=length,matched=True,sha256=hashlib.sha256(actual).hexdigest()))
        wait_marker(b'[liteeth] PASS')
        wait_marker(b'EL0 mappings revoked')
        log=text()
        if b'DTB source: firmware' not in log or b'EL0 exit code=0x0000000000000000' not in log:raise RuntimeError('DTB来源或退出码不符合预期')
        stat_lines=re.findall(rb'\[liteeth\] STATS (\{[^\r\n]+\})',log)
        if len(stat_lines)!=1:raise RuntimeError('缺少唯一的收发统计')
        stats=json.loads(stat_lines[0])
        expected_bytes=sum(frame['length'] for frame in results)
        expected_stats=dict(rx_packets=17,tx_packets=17,rx_bytes=expected_bytes,tx_bytes=expected_bytes,rx_errors=0,tx_errors=1,rx_dropped=0,tx_dropped=1)
        if stats!=expected_stats:raise RuntimeError(f'统计不符：{stats}')
        device_log=(case/'qemu.log').read_text()
        tx=[list(map(int,m)) for m in re.findall(r'LITEETH_TRACE TX (\d+) (\d+)',device_log)]
        rx=[list(map(int,m)) for m in re.findall(r'LITEETH_TRACE RX (\d+) (\d+)',device_log)]
        expected_slots=[[i%2,frame['length']] for i,frame in enumerate(results)]
        if tx!=expected_slots or rx!=expected_slots:raise RuntimeError('设备侧 slot/长度与预期不符')
        lifecycle=[[int(o,16),int(v)] for o,v in re.findall(r'LITEETH_TRACE WRITE (14|34) (\d+)',device_log)]
        expected_lifecycle=[[20,1],[52,1],[20,0],[52,0]]*2
        if lifecycle!=expected_lifecycle:raise RuntimeError(f'设备使能/停止顺序不符：{lifecycle}')
        # 每次 START 前必须先写 TX_SLOT、TX_LENGTH；原始 trace 保留全部 W1C 事件。
        writes=[(int(o,16),int(v)) for o,v in re.findall(r'LITEETH_TRACE WRITE ([0-9a-f]+) (\d+)',device_log)]
        for index,(offset,value) in enumerate(writes):
            if offset==0x18 and [x[0] for x in writes[index-2:index+1]]!=[0x24,0x28,0x18]:raise RuntimeError('发送寄存器提交顺序错误')
        output=dict(passed=True,implementation=a.implementation,frames=results,stats=stats,device_tx=tx,device_rx=rx,lifecycle=lifecycle,c_ffi_symbols=ffi,elf_bytes=(case/'libos.elf').stat().st_size,elf_sha256=hashlib.sha256((case/'libos.elf').read_bytes()).hexdigest(),scope='QEMU model; raw L2 echo; oversize drop; no IP/socket')
        (case/'result.json').write_text(json.dumps(output,indent=2)+'\n')
        print(f'PASS {a.implementation}: 17 frames, statistics, slot/CSR order, oversize, lifecycle; {case}')
    finally:
        proc.terminate()
        try:proc.wait(timeout=5)
        except subprocess.TimeoutExpired:proc.kill();proc.wait()
        if "conn" in globals():conn.close()
        listener.close()
