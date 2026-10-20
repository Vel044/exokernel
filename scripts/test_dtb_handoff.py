#!/usr/bin/env python3
"""构建并验证 DTB 交接；仅使用 QEMU 虚拟设备，日志和启动盘全部保存在 target。"""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def wait_until(process, predicate, timeout, description):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        if process.poll() is not None:
            raise RuntimeError(f'QEMU提前退出：{description}')
        time.sleep(0.05)
    raise TimeoutError(description)


def run_case(args, output, name, app, accel='tcg', usb=False, hub=False, acpi_off=False):
    case = output / name
    case.mkdir()
    env = dict(os.environ, LIBOS_APP=app, LIBOS_ENABLE_XHCI='1' if usb and app == 'system-smoke' else '0', ACT_KERNEL_PROVIDER='portable')
    env.pop('LIBOS_FEATURES', None)
    with (case / 'build.log').open('wb') as log:
        subprocess.run(['bash', 'build.sh'], cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    esp = case / 'esp/EFI/BOOT'
    esp.mkdir(parents=True)
    shutil.copyfile(ROOT / 'target/aarch64-unknown-uefi/debug/BOOTAA64.efi', esp / 'BOOTAA64.EFI')
    shutil.copyfile(args.vars, case / 'vars.fd')
    serial = case / 'serial.log'
    uart_socket = case / 'uart.sock'
    usb_socket = case / 'usb.sock'
    machine = 'virt,virtualization=on' if accel == 'tcg' else 'virt'
    if acpi_off:
        machine += ',acpi=off'
    command = [args.qemu, '-accel', accel, '-M', machine, '-cpu', 'cortex-a76' if accel == 'tcg' else 'host',
               '-smp', '4', '-m', '4G', '-drive', f'if=pflash,format=raw,unit=0,readonly=on,file={args.code}',
               '-drive', f'if=pflash,format=raw,unit=1,file={case}/vars.fd',
               '-drive', f'file=fat:rw:{case}/esp,format=raw,if=none,id=boot',
               '-device', 'virtio-blk-device,drive=boot,bootindex=0',
               '-monitor', 'none', '-display', 'none', '-no-reboot', '-nic', 'none']
    if app == 'uart-echo':
        command += ['-chardev', f'socket,id=uart,path={uart_socket},server=on,wait=on', '-serial', 'chardev:uart']
    else:
        command += ['-serial', f'file:{serial}']
    if usb:
        command += ['-chardev', f'socket,id=usbserial,path={usb_socket},server=on,wait=off',
                    '-device', 'qemu-xhci,id=xhci,addr=02.0,msi=off,msix=off']
        if hub:
            command += ['-device', 'usb-hub,id=hub,bus=xhci.0,port=1,ports=8',
                        '-device', 'usb-serial,bus=xhci.0,port=1.1,chardev=usbserial,always-plugged=on']
        else:
            command += ['-device', 'usb-serial,bus=xhci.0,chardev=usbserial,always-plugged=on']
    (case / 'command.json').write_text(json.dumps(command, ensure_ascii=False, indent=2) + '\n')
    started = time.monotonic()
    connection = None
    with (case / 'qemu.log').open('wb') as log:
        process = subprocess.Popen(command, cwd=case, stdout=log, stderr=subprocess.STDOUT)
        try:
            if app == 'uart-echo':
                wait_until(process, uart_socket.exists, 10, '等待UART socket')
                connection = socket.socket(socket.AF_UNIX)
                connection.connect(str(uart_socket))
                connection.settimeout(0.2)
                data = bytearray()

                def uart_ready():
                    try:
                        data.extend(connection.recv(65536))
                    except TimeoutError:
                        pass
                    return b'UART echo ready' in data

                wait_until(process, uart_ready, args.timeout, '等待UART初始化')
                serial.write_bytes(data)
            elif usb and app == 'usb-echo':
                wait_until(process, usb_socket.exists, 10, '等待USB socket')
                connection = socket.socket(socket.AF_UNIX)
                connection.connect(str(usb_socket))
                connection.settimeout(0.2)
                wait_until(process, lambda: serial.exists() and b'FTDI 0403:6001 ready' in serial.read_bytes(), args.timeout, '等待FTDI端点配置')
            if connection is not None:
                # 两笔分开的请求，验证回显循环能再次提交读取并被新的中断唤醒。
                for index in range(2):
                    payload = f'DTB-{name}-{index}-echo!'.encode()
                    connection.sendall(payload)
                    received = bytearray()

                    def echoed():
                        try:
                            received.extend(connection.recv(65536))
                        except TimeoutError:
                            pass
                        return payload in received

                    wait_until(process, echoed, 15, f'等待第{index + 1}次回显')
                    if app == 'uart-echo':
                        with serial.open('ab') as stream:
                            stream.write(received)
                    time.sleep(0.1)
            else:
                def finished():
                    text = serial.read_bytes() if serial.exists() else b''
                    if b'DTB handoff failed' in text:
                        raise RuntimeError(f'DTB交接失败：{text[-1500:]!r}')
                    return b'EL0 mappings revoked' in text
                wait_until(process, finished, args.timeout, '等待场景成功退出')
                text = serial.read_bytes()
                expected = b'process VSpace smoke passed' if app == 'process-smoke' else b'Robot thread integration passed'
                if expected not in text or b'EL0 exit code=0x0000000000000000' not in text:
                    raise RuntimeError(f'场景退出失败：{text[-1600:]!r}')
                if app == 'system-smoke' and b'DTB handoff + mapping protection passed' not in text:
                    raise RuntimeError('DTB映射保护检查未完成')
            text = serial.read_bytes()
            if b'full DTB parsed in EL0' not in text:
                raise RuntimeError('没有完成用户态DTB解析')
            # ACPI关闭时固件必须实际提供DTB，不能误把快照回退当作固件路径成功。
            if acpi_off and b'DTB source: firmware' not in text:
                raise RuntimeError('ACPI关闭但没有走固件DTB交接路径')
            source = 'firmware' if b'DTB source: firmware' in text else 'embedded QEMU'
            result = dict(case=name, app=app, accel=accel, dtb_source=source, passed=True,
                          seconds=round(time.monotonic() - started, 2))
            print(f'PASS {name}', flush=True)
            return result
        finally:
            if connection is not None:
                connection.close()
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            for path in (uart_socket, usb_socket):
                path.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default='qemu-system-aarch64')
    parser.add_argument('--code', default='/opt/homebrew/share/qemu/edk2-aarch64-code.fd')
    parser.add_argument('--vars', default='/opt/homebrew/share/qemu/edk2-arm-vars.fd')
    parser.add_argument('--timeout', type=int, default=90)
    parser.add_argument('--hvf', action='store_true', help='额外测试Apple Silicon上的EL1固件交接')
    parser.add_argument('--cases', nargs='+', choices=['system', 'firmware-dtb', 'process', 'uart', 'usb', 'usb-hub', 'system-usb'],
                        default=['system', 'firmware-dtb', 'process', 'uart', 'usb', 'usb-hub', 'system-usb'])
    args = parser.parse_args()
    for path in (args.code, args.vars):
        if not Path(path).is_file():
            parser.error(f'缺少固件：{path}')
    # Unix socket路径有限长，使用短的临时路径；报告最终保存在被忽略的target目录。
    import tempfile
    with tempfile.TemporaryDirectory(prefix='exo-dtb-', dir='/tmp') as temporary:
        output = Path(temporary)
        report = []
        destination = ROOT / 'target/dtb-regression' / time.strftime('%Y%m%d-%H%M%S')
        try:
            for name in args.cases:
                app = {'system': 'system-smoke', 'firmware-dtb': 'system-smoke', 'process': 'process-smoke',
                       'uart': 'uart-echo', 'usb': 'usb-echo', 'usb-hub': 'usb-echo', 'system-usb': 'system-smoke'}[name]
                report.append(run_case(args, output, name, app, usb=name in ('usb', 'usb-hub', 'system-usb'),
                                       hub=name == 'usb-hub', acpi_off=name == 'firmware-dtb'))
            if args.hvf:
                for name, app in [('system-hvf', 'system-smoke'), ('process-hvf', 'process-smoke')]:
                    report.append(run_case(args, output, name, app, accel='hvf'))
        finally:
            destination.mkdir(parents=True, exist_ok=True)
            # 只留可复查日志；EFI、FAT目录、NVRAM等生成物不复制进结果报告。
            for case in output.iterdir():
                if case.is_dir():
                    saved = destination / case.name
                    saved.mkdir()
                    for path in case.iterdir():
                        if path.suffix in ('.log', '.json'):
                            shutil.copyfile(path, saved / path.name)
            (destination / 'summary.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
            print(f'REPORT={destination}', flush=True)


if __name__ == '__main__':
    main()
