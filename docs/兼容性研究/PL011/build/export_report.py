#!/usr/bin/env python3
"""将缓存中的真实编译证据导出为小文件清单和兼容层缺口报告。"""
import argparse
from collections import Counter
import csv
import hashlib
import json
from pathlib import Path
import shutil

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
parser = argparse.ArgumentParser()
parser.add_argument('--cache', type=Path, default=Path.home() / '.cache/exokernel-linux-v6.12')
args = parser.parse_args()
cache = args.cache.expanduser().resolve()
raw = cache / 'artifacts/pio'
analysis = cache / 'analysis/pio'
out = ROOT / 'compiled'
out.mkdir(exist_ok=True)

for source in analysis.iterdir():
    if source.is_file():
        shutil.copyfile(source, out / source.name)
for name in ['linux.config', 'requested.fragment', 'toolchain.txt', 'packages.tsv',
             'compile_commands.json', 'replay-commands.json', 'amba-pl011.o.cmd',
             'amba-pl011.undefined.txt', 'serial_base.undefined.txt',
             'pl011-with-serial-base.undefined.txt', 'configure.log', 'build.log']:
    shutil.copyfile(raw / name, out / name)

def read_csv(name):
    with (out / name).open(encoding='utf-8-sig') as stream:
        return list(csv.DictReader(stream))

symbols = read_csv('symbol-boundaries.csv')
direct = [r for r in symbols if r['pl011_undefined'] == 'True']
summary = json.loads((out / 'summary.json').read_text())

# 每个实际外部符号都要分配职责与动作；新增符号必须显式复核，不能静默漏项。
GROUPS = [
    ('设备注册 / AMBA 与 SBSA', '__amba_driver_register amba_driver_unregister __platform_driver_register platform_driver_unregister platform_get_irq platform_get_resource',
     '建立受限设备注册表和资源对象。SBSA 分支仍被 init 注册；仅选择 QEMU AMBA 不会自动删除相关链接依赖。'),
    ('设备属性与 DT', 'device_property_read_u32_array of_alias_get_id of_property_read_variable_u32_array',
     '从目标设备配置提供属性、别名和资源；缺失可选属性按源 API 返回“不存在”，不伪造成功。'),
    ('普通内存 / MMIO', 'devm_kmalloc devm_ioremap_resource',
     '接 EL0 分配器和 MAP_MMIO；保留 GFP_ZERO 清零、ERR_PTR/负错误、托管账本及回滚约定。'),
    ('时钟', 'devm_clk_get clk_prepare clk_unprepare clk_enable clk_disable clk_get_rate',
     '固定平台时钟对象及生命周期；读取经核实的输入频率，才能正确设置波特率。prepare_enable 在编译后分解为多个符号。'),
    ('IRQ', 'request_threaded_irq free_irq nr_irqs',
     'request_irq 已内联为 request_threaded_irq；保存 handler/dev_id/flags 并桥接 Notification。nr_irqs 是全局上界数据，不是函数。'),
    ('同步与等待', 'queued_spin_lock_slowpath __wake_up',
     '实现目标锁/条件队列与唤醒语义；还必须替换内联关 IRQ 和原子路径，不能只补这两个函数。'),
    ('微秒延迟', '__const_udelay __udelay',
     '适配微秒忙等；__udelay 的参数为微秒，__const_udelay 的参数是按 0x10C7 缩放的 xloops，不能把两者当同一单位。用于时序/RS485 等已编译路径。'),
    ('输出与格式化', '_dev_err _dev_warn _printk snprintf',
     '提供 C 格式化与日志出口，和测试 UART 数据隔离；区分日志可裁剪与必要错误处理。'),
    ('TTY 及安全注意键', 'tty_flip_buffer_push tty_get_frame_size tty_termios_encode_baud_rate do_SAK',
     '接 RX 队列及配置算法。关闭 MAGIC_SYSRQ 后 do_SAK 仍存在；SAK 由端口运行标志控制，须显式限定或实现。'),
    ('串口框架接口', 'uart_add_one_port uart_get_baud_rate uart_get_rs485_mode uart_handle_cts_change uart_handle_dcd_change uart_insert_char uart_register_driver uart_remove_one_port uart_unregister_driver uart_update_timeout uart_write_wakeup',
     '这 11 项由实际构建的 serial_base 四对象提供；建议先做受限框架门面，真实连接对象、队列、配置与回调。'),
]
category_map = {s: (category, note) for category, names, note in GROUPS for s in names.split()}
assert set(category_map) == {r['symbol'] for r in direct}, '实际符号与人工分类不一致，必须重新审核'
rows = []
for row in direct:
    category, note = category_map[row['symbol']]
    rows.append(dict(**row, category=category, compatibility_action=note))
with (out / 'compatibility-actions.csv').open('w', encoding='utf-8-sig', newline='') as stream:
    writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
    writer.writeheader(); writer.writerows(rows)

def combined_action(symbol):
    if symbol.startswith(('tty_', '__tty_')) or symbol == 'do_SAK':
        return 'TTY / 行规程', '若导入完整框架，需要端口、driver、引用、line discipline、hangup 等语义；受限门面另行替换。'
    if symbol.startswith('console_') or symbol in {'register_console', 'of_console_check'}:
        return 'console 模型', '处理 console 注册、列表锁和设备关联；不能仅用日志打印代替。'
    if symbol in {'__const_udelay', '__udelay'}:
        return '微秒忙等', '适配 scaled xloops 与 usecs 两种参数；满足寄存器时序。'
    if 'jiffies' in symbol or symbol.startswith('msleep'):
        return '时间 / 睡眠', '提供 tick 转换、定时唤醒及睡眠/中断语义；忙等不等价。'
    if symbol.startswith(('__kmalloc', 'get_zeroed_page')) or symbol in {'devm_kmalloc', 'free_pages', 'kmalloc_caches', 'kfree'}:
        return '内存分配', '普通/页/清零分配与释放；若保留 slab 调用则须提供相应缓存对象。'
    if symbol == 'devm_ioremap_resource':
        return 'MMIO', '授权映射与托管生命周期，使用 Device 内存属性。'
    if symbol.startswith(('clk_', 'devm_clk')):
        return '时钟', '真实频率、prepare/enable 与引用生命周期。'
    if symbol.startswith(('irq_', 'fwnode_irq')) or symbol in {'nr_irqs', 'free_irq', 'request_threaded_irq', 'synchronize_irq'}:
        return 'IRQ', 'IRQ 号映射、处理函数生命周期和停机同步；唤醒后在 EL0 处理。'
    if symbol.startswith(('mutex_', 'queued_spin_')) or symbol in {'__wake_up', '__init_waitqueue_head', 'add_wait_queue', 'remove_wait_queue', 'finish_wait', 'init_wait_entry', 'prepare_to_wait_event', 'default_wake_function', 'schedule', 'down_write', 'up_write'}:
        return '锁 / 调度 / 等待', '提供原子登记、等待条件、唤醒及取消，和目标调度机制对接。'
    if symbol.startswith('__arch_copy_'):
        return '用户内存访问', '地址检查与 fault 恢复契约；不能默认等价于普通 memcpy。'
    if symbol.startswith(('__kfifo_', 'ida_')):
        return 'FIFO / ID 管理', '可移植算法与对象生命周期；并发条件仍需目标后端满足。'
    if symbol.startswith(('_dev_', '_printk', '__warn_printk')) or symbol in {'snprintf', 'sprintf', 'kasprintf', '___ratelimit'}:
        return '格式化 / 日志', '处理格式、分配与限流；核查 Linux 格式扩展，隔离日志和设备 payload。'
    if symbol in {'sized_strscpy', 'strncmp', 'kstrtobool'}:
        return '字符串工具', '复用或按源语义实现，保留截断和错误返回。'
    if symbol.startswith(('__amba_', 'amba_', '__platform_', 'platform_', 'device_', 'driver_', 'bus_', 'fwnode_', 'of_', '__dev_fwnode')) or symbol in {'dev_set_name', 'put_device'}:
        return '设备 / 总线 / 属性', '提供注册、引用、资源、属性及层级关系；范围明显大于单个 UART 对象。'
    raise AssertionError(f'组合边界新增符号尚未分类：{symbol}')

combined_rows = []
for row in symbols:
    if row['combined_undefined'] == 'True':
        category, action = combined_action(row['symbol'])
        combined_rows.append(dict(symbol=row['symbol'], category=category,
                                  compatibility_action=action, linux_exports=row['linux_exports']))
with (out / 'compatibility-actions-with-serial-base.csv').open('w', encoding='utf-8-sig', newline='') as stream:
    writer = csv.DictWriter(stream, fieldnames=list(combined_rows[0]))
    writer.writeheader(); writer.writerows(combined_rows)

manifest = dict(
    source='Linux v6.12 release archive',
    source_archive_sha256='b1a2562be56e42afb3f8489d4c2a7ac472ac23098f1ef1c1e40da601f54625eb',
    source_driver_sha256=summary['source_sha256'],
    config_sha256=summary['config_sha256'],
    base_image='ubuntu:24.04@sha256:c35e29c9450151419d9448b0fd75374fec4fff364a27f176fb458d472dfc9e54',
    container='exokernel-pl011-v612', volume='exokernel-linux-v612', cache=str(cache),
    build_recipe_sha256={p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                         for p in sorted(HERE.iterdir()) if p.is_file()},
    raw_artifacts={p.name: dict(bytes=p.stat().st_size, sha256=hashlib.sha256(p.read_bytes()).hexdigest())
                   for p in sorted(raw.iterdir()) if p.is_file()},
    extracted_evidence={p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                        for p in sorted(out.iterdir()) if p.is_file() and p.name != 'build-manifest.json'})
(out / 'build-manifest.json').write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + '\n')

md = ['# Linux v6.12 PL011 实际编译结果与兼容层缺口', '',
      '**已搭建独立 AArch64 Linux 编译环境，实际编译原始 PL011 与串口框架对象，并完成依赖提取。** 这是原 Linux 构建边界的证据；尚未实现用户态兼容层，也没有运行迁移后的驱动。', '',
      '## 环境和构建结果', '',
      '| 项目 | 本次实际值 |', '| --- | --- |',
      '| 源码 | kernel.org 正式 linux-6.12.tar.xz；SHA-256 与官方发布清单一致 |',
      '| 运行环境 | macOS 上的 Docker Linux AArch64 容器；独立 Linux 文件系统卷 |',
      '| 工具链 | Ubuntu Clang/LLD/LLVM 18.1.3，GNU Make 4.3 |',
      '| 输出 | ELF64 little-endian AArch64 ET_REL；优化 -O2，附加 -g 调试信息 |',
      '| 配置 | SMP=y，NR_CPUS=4，TTY/AMBA/PL011/COMMON_CLK=y；PIO，无 DMAengine/PL011 console/PM/ACPI/pinctrl |',
      '| 真实产物 | amba-pl011.o、.i、.s、.ll；4 个串口框架 .o；组合部分链接目标 |',
      '| 源码修改 | 原始 Linux 文件未打补丁；驱动哈希与此前 v6.12 快照一致 |', '',
      '有效配置是 [linux.config](compiled/linux.config)，输入意图是 [requested.fragment](compiled/requested.fragment)。两者不能互换。容器 uname 中的 LinuxKit 6.12.54 是构建宿主内核，不是所编译源码版本。', '',
      '内建配置下 Kbuild 把 serial_base-y 的四个对象直接收入 thin archive；本实验另外用 ld.lld -r 将它们组合，便于比较。该操作保留未解析符号，不是完整内核链接，也不是可以装载的 .ko。', '',
      '## 数字如何解释', '',
      '| 边界 / 证据 | 数量 |', '| --- | ---: |',
      f"| PL011 完整外部符号：42 个函数 + 1 个全局数据 nr_irqs | {summary['pl011_undefined']} |",
      f"| 串口框架四对象组合的外部符号 | {summary['serial_base_undefined']} |",
      f"| PL011 + 串口框架部分链接后仍未解析的符号 | {summary['combined_undefined']} |",
      f"| 导入串口框架消掉的 PL011 原依赖 | {len(summary['direct_resolved_by_core'])} |",
      f"| 组合相对 PL011 单体新增的外部符号 | {len(summary['combined_new_vs_pl011'])} |",
      f"| 实际预处理输入：703 个头文件 + 1 个驱动源文件 | {summary['headers_or_sources']} |",
      f"| 优化后 IR 的直接外部函数调用位置 | {summary['direct_external_call_sites']} |",
      f"| 优化后 IR 的间接调用位置 | {summary['indirect_call_sites']} |",
      f"| 优化后 IR 的内联汇编调用位置（含空编译器屏障） | {summary['inline_assembly_sites']} |",
      f"| 其中操作 DAIF 的位置 | {summary['asm_daif_sites']} |",
      f"| 其中引用 .altinstructions 的 MMIO 读取位置 | {summary['asm_alternative_sites']} |", '',
      '**本清单的“完整”指固定配置下该编译单元的外部符号全集、预处理文件集合及本次优化 IR 中的调用/汇编位置。** 它不表示任意 Linux 配置的全集，也不是递归导入整个 Linux 所有依赖的闭包。导入某个实现后会产生新依赖，119 项对照正是一次实际验证。数据队列、回调契约和调度语义也不能仅用符号数量衡量。', '',
      '## 一、PL011 的全部 43 个外部符号及动作', '',
      '每个名称均来自实际目标文件，完整逐项 CSV 见 [compatibility-actions.csv](compiled/compatibility-actions.csv)。调用来源及上游导出位置另见 [symbol-boundaries.csv](compiled/symbol-boundaries.csv)；导出位置只作源码定位，不代表那个提供者已在本实验编译。', '',
      '| 类别 | 全部实际符号 | 兼容层要补什么 |', '| --- | --- | --- |']
for category, names, note in GROUPS:
    md.append(f"| {category} | {'<br>'.join('`'+s+'`' for s in names.split())} | {note} |")
md += ['', '注意宏/内联名称变化：源码的 request_irq → 目标依赖 request_threaded_irq，devm_kzalloc → devm_kmalloc，clk_prepare_enable → clk_prepare + clk_enable。原先 52 个词法索引不能直接与 43 项一一相减。', '',
       '## 二、导入 serial_core 会发生什么', '',
       '实际编译并部分链接的四个文件来自 serial_base-y：serial_core.c、serial_base_bus.c、serial_ctrl.c、serial_port.c。它们提供了上表 11 个 uart_* 接口，但组合后仍有 **119 个外部符号**，比 PL011 单体多出 87 个新名字。', '',
       '新增范围包括：TTY driver/port/line discipline、schedule/等待队列、mutex/rwsem、copy_to_user/from_user、设备/总线/driver 模型、IDA、页分配与 slab、jiffies 和 sleep、console 注册。关闭 PL011_CONSOLE 不会自动去掉串口公共框架里的所有 console 相关链接依赖。', '',
       '完整集合与是否被串口框架解析的关系见 [symbol-boundaries.csv](compiled/symbol-boundaries.csv)，原始符号表为 [组合目标未解析符号](compiled/pl011-with-serial-base.undefined.txt)。[119 项逐符号职责与动作](compiled/compatibility-actions-with-serial-base.csv) 也已全部分类。这支持先实现受限 serial_core 门面的选择；如果目标是完整 Linux TTY 兼容，则应重新评估导入公共框架的成本。', '',
       '## 三、只补符号仍然运行不了：内联与结构依赖', '',
       '### 关中断与锁', '',
       '真实反汇编中包含 mrs DAIF、msr DAIFSet/DAIFClr、msr DAIF。它们来自 irqsave/irqrestore 路径，普通 EL0 不能照搬 Linux 的 IRQ 控制语义。还包含队列自旋锁的内联 LL/SC 和屏障；即便只实现 queued_spin_lock_slowpath，也没有替换全部锁实现。', '',
       '需要在重新编译的头文件/执行环境层定义目标的锁、虚拟 IRQ 与回调串行语义；不能把原 .o 直接链接进 libOS，更不能通过替换一个函数让已内联指令消失。首阶段建议由单 worker 执行驱动回调，外部客户端只操作请求队列，再逐条核对重入和等待点。', '',
       '### MMIO 与 alternatives', '',
       'MMIO 读写已经内联为 16/32 位访问；部分读取还带 Linux .altinstructions 替换信息。目标后端必须明确支持的设备访问宽度和顺序，并重新提供用户态可用的 accessor；不应无条件复制 Linux 启动时的 alternatives 修补假设。', '',
       '### 六个间接调用位置', '',
       '| 所在函数 | 间接目标 | 首阶段契约 |', '| --- | --- | --- |',
       '| pl011_hwinit | 可选平台数据 init 回调 | 不提供 platdata 或提供真实已登记回调 |',
       '| pl011_shutdown | 可选平台数据 exit 回调 | 与 init 生命周期匹配 |',
       '| pl011_shutdown | ops->flush_buffer | 无 DMA 配置下当前表项为 NULL；其他 ops 不可任意混入 |',
       '| sbsa_uart_shutdown | ops->flush_buffer | SBSA 配置对象及可选回调同样需受控 |',
       '| pl011_probe | vendor->get_fifosize | 固定受支持 vendor；保留正确 FIFO 大小 |',
       '| pl011_int | port->handle_break | NULL 或登记的 break 回调；不是 SysRq 配置的替代物 |', '',
       '位置和 IR 行号在 [indirect-calls.csv](compiled/indirect-calls.csv)。此外，uart_ops 中实际保留 **16 个非空函数回调**；flush_buffer 为 NULL，console poll 字段被预处理排除。函数本身仍在 IR 中，不等于它作为某个表项存在，索引已按有效预处理结果核对。', '',
       '### 数据结构与 C ABI', '',
       'Clang 实际输出以下大小；偏移和嵌套字段见 [结构布局](compiled/key-type-layouts.txt)。这些值仅对本次 .config/工具链成立，不能当作跨配置稳定 ABI。', '',
       '| 类型 | 字节数 | 对齐 |', '| --- | ---: | ---: |']
for row in summary['key_type_layouts']:
    md.append(f"| {row['name']} | {row['size']} | {row['alignment']} |")
md += ['', '建议在 C 侧管理这些 Linux 对象，Rust 侧通过不透明 handle 和明确 C ABI 函数连接；避免手工复制全部内嵌锁、设备和 TTY 结构。若替换头文件或结构布局，必须连同驱动一起重新编译。', '',
       '## 四、哪些功能确实被排除，哪些还在', '',
       '| 项目 | 本次证据 |', '| --- | --- |',
       '| DMA / DMAengine | 目标无 dma_* 外部依赖，无 DMA 实现函数；启用上游 PIO 后备分支 |',
       '| PL011 console / console poll | 配置关闭；相应 poll 表项和 console 代码被预处理排除 |',
       '| PM / pinctrl / ACPI | 配置不启用；相关平台 helper 可能内联为空或消失，不能推广到 Pi5 的平台要求 |',
       '| SBSA UART | 仍保留，pl011_init 无条件注册其 platform driver |',
       '| RS485 / modem | 运行模式可限定关闭，但相关实现、udelay 和 CTS/DCD 路径仍在目标里 |',
       '| SAK | do_SAK 仍是链接依赖；关闭 SysRq 没有把它删除 |',
       '| Linux 锁和 IRQ 控制 | 仍存在，且不少已内联为指令 |', '',
       '完整宏快照 effective-macros.txt 和预处理 .i 保存在仓库外缓存；不是凭源码 #ifdef 猜测配置生效。', '',
       '## 五、兼容层具体拆成哪些模块', '',
       '| 建议模块 | 需要覆盖的真实边界 | 首个验收点 |', '| --- | --- | --- |',
       '| C 设备和资源门面 | AMBA/SBSA 注册、属性、resource、devm 分配/映射账本 | 单个授权 UART 构造、probe 成功及失败回滚 |',
       '| 时钟和平台配置 | clk 对象、真实频率、enable/prepare 计数 | baud/LCRH/CR 配置有来源；不依赖偶然继承的状态 |',
       '| IRQ registry + worker | request_threaded_irq/free_irq、IRQ 号映射、handler/dev_id、Notification、ACK | 事件先于 wait、批量数据、重复停止无丢唤醒/悬空回调 |',
       '| 目标锁和 MMIO 头后端 | irqflags、spinlock/atomic、accessor、内存顺序 | 重新编译后无不适用于 EL0 的 DAIF 操作，访问宽度正确 |',
       '| 受限 serial_core / TTY 桥接 | 11 个 uart_*、3 个 tty_*、端口状态及 RX/TX 队列 | 16 个端口回调连接；短读写、错误字节与背压有定义 |',
       '| 工具与日志 | snprintf、微秒延迟、设备日志、init/exit 调用入口 | 不污染串口字节流；错误不被假成功吞掉 |', '',
       '首阶段明确不提供任意共享 IRQ、完整 TTY 行规程、SAK/RS485/硬件流控、热插拔或任意 Linux 驱动环境。对于编译中保留而实验禁止的路径，采用明确的配置/输入约束与错误诊断；不能仅留一个“成功返回”的占位函数。', '',
       '下一步最先补 **目标头文件执行环境 + 设备/MMIO/IRQ 资源门面**，把 startup/set_termios/IRQ handler 连成一个最小运行闭环；随后再补 RX/TX 桥接。当前证据不支持先原样搬入 serial_core 四对象来省去全部适配。', '',
       '## 复现与原始产物', '',
       '在 exokernel 仓库根目录运行（需要 Docker 可用；首次运行会构建工具镜像和独立卷）：', '',
       '```sh', 'python3 docs/兼容性研究/PL011/build/reproduce.py',
       'python3 docs/兼容性研究/PL011/build/export_report.py', '```', '',
       f'本次缓存目录：`{cache}`；容器：`exokernel-pl011-v612`；Linux 源码及构建目录在命名卷 `exokernel-linux-v612` 的 `/work/` 下。宿主文件系统不承载展开的 Linux 源码，以避免大小写路径问题。', '',
       '仓库内仅保留配置、脚本和提取证据；完整 Linux 源码、.o/.i/.s/.ll、完整布局与反汇编在缓存/容器卷中。文件大小及哈希见 [build-manifest.json](compiled/build-manifest.json)。', '',
       '证据入口：[构建日志](compiled/build.log)、[实际编译命令](compiled/compile_commands.json)、[工具链](compiled/toolchain.txt)、[软件包版本](compiled/packages.tsv)、[704 个预处理文件](compiled/preprocessor-files.csv)、[全部汇编位置](compiled/inline-assembly.csv)、[直接调用位置](compiled/external-callsites.csv)。', '',
       '这次运行验证的是原 Linux 目标文件能否构建及其编译边界；没有构建完整 vmlinux，也没有运行 Linux 来宾或迁移后的 PL011。']
(ROOT / '编译结果与兼容层缺口.md').write_text('\n'.join(md) + '\n')
print(f'已导出 {len(direct)} 项直接依赖、完整边界数据及编译报告：{ROOT}')
