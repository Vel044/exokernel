#!/usr/bin/env python3
"""独立兼容语义测试：原 C 驱动 + ASan/UBSan，不依赖 QEMU 构建。"""
from pathlib import Path
import hashlib,os,subprocess
root=Path(__file__).resolve().parents[2]
source=root/'libos/vendor/liteeth-linux/litex_liteeth.c'
expected='bb8b3c2cac2f8821647c66cf00a96adf7eed02b0c6c9acc925d37eb7b628285e'
if hashlib.sha256(source.read_bytes()).hexdigest()!=expected:
    raise SystemExit('原始 Linux 驱动内容发生变化，请显式核查来源及测试')
output=root/'target/liteeth-host-tests';(output/'include/linux').mkdir(parents=True,exist_ok=True)
for header in ['etherdevice','interrupt','litex','module','of_net','platform_device']:
    (output/f'include/linux/{header}.h').write_text('#include "compat.h"\n')
cmd=[os.environ.get('CC','clang'),'-std=gnu11','-O1','-g','-fsanitize=address,undefined','-I',str(root/'libos/src/drivers/linux_liteeth'),'-I',str(output/'include'),str(root/'tools/liteeth/test_adapter.c'),'-o',str(output/'test_adapter')]
subprocess.run(cmd,check=True);subprocess.run([str(output/'test_adapter')],check=True)
