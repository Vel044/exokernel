#!/usr/bin/env python3
"""从固定 v6.12 快照生成关键依赖证据索引和流程 SVG；不推断完整调用图。"""
from pathlib import Path
from xml.sax.saxutils import escape
import csv
import json
import re

HERE = Path(__file__).resolve().parent
SOURCE = HERE / 'sources/amba-pl011.c'
TEXT = SOURCE.read_text()

# 每组只列已在驱动中核实的代表接口；不是完成预处理后的全部外部依赖。
DEPS = [
    ('设备发现', 'amba_driver_register amba_set_drvdata amba_get_drvdata amba_rev', '设备门面', '用获授权资源构造受限设备对象，保留端口私有状态与型号来源'),
    ('内存与资源', 'devm_kzalloc devm_ioremap_resource', 'EL0 适配', '零初始化分配；授权 MMIO 映射；建立失败回滚和托管释放记录'),
    ('时钟', 'devm_clk_get clk_prepare_enable clk_get_rate clk_disable_unprepare', '待补平台契约', '需要真实频率及启停策略；现有 UserBootInfo.uart 不提供这些数据'),
    ('引脚', 'pinctrl_pm_select_default_state pinctrl_pm_select_sleep_state', '限定平台前提', 'QEMU 可声明无 pinmux；真机必须检查固件配置或提供控制'),
    ('寄存器访问', 'readl_relaxed readw_relaxed writel_relaxed writew_relaxed', 'EL0 适配', '保留访问宽度、Device 属性、volatile 与所需顺序'),
    ('IRQ', 'request_irq free_irq', 'EL0 IRQ 分发器', '保存 handler/dev_id；Notification 驱动处理；同步卸载后释放'),
    ('锁', 'uart_port_lock uart_port_unlock uart_port_lock_irq uart_port_unlock_irq uart_port_lock_irqsave uart_port_unlock_irqrestore', '执行模型设计', '不能在 EL0 照搬关硬件 IRQ；先采用单 UART worker，队列跨线程同步'),
    ('串口注册', 'uart_register_driver uart_add_one_port uart_remove_one_port uart_unregister_driver', 'serial_core 门面或导入', '明确建立 uart_port/state/ops 关系和资源生命周期；不能只返回成功'),
    ('RX 数据桥接', 'uart_insert_char tty_flip_buffer_push uart_prepare_sysrq_char uart_handle_break', 'RX 队列与状态', '搬运字节和错误标记；首阶段关闭 SysRq 但须保留错误计数与策略'),
    ('TX 队列', 'kfifo_is_empty kfifo_len kfifo_peek kfifo_skip uart_tx_stopped uart_write_wakeup', '可复用算法+桥接', '提供发送 FIFO 和背压；wakeup 唤醒有空间可写的客户端'),
    ('串口配置', 'uart_get_baud_rate uart_update_timeout tty_get_frame_size', '固定配置子集', '8N1 无硬件流控；频率明确后配置分频及状态掩码'),
    ('模式与流控', 'uart_get_rs485_mode uart_handle_cts_change uart_handle_dcd_change wake_up_interruptible', '首阶段排除路径', 'RS485、modem、硬件流控不启用；仍需编译/链接确认依赖'),
    ('DMA 分配', 'dma_alloc_coherent dma_free_coherent dma_map_single dma_unmap_single', 'CONFIG_DMA_ENGINE=n', '源码含明确的无 DMA 后备分支；无需在首阶段接 DMA_ALLOC'),
    ('DMAengine', 'dma_request_chan dma_release_channel dma_async_issue_pending', 'CONFIG_DMA_ENGINE=n', '这是 DMA 控制器通道框架；后续支持时仅有 DMA 内存分配不够'),
]


def clean_source(s):
    # 保留换行，因此索引可直接指回原始文件；仅用于词法证据，不是 C 前端。
    return re.sub(r'/\*.*?\*/|//[^\n]*|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\'',
                  lambda m: '\n' * m[0].count('\n') or ' ', s, flags=re.S)


def index():
    clean = clean_source(TEXT)
    rows = []
    for category, symbols, action, meaning in DEPS:
        for symbol in symbols.split():
            matches = list(re.finditer(r'\b' + symbol + r'\s*\(', clean))
            assert matches, f'源码中未找到 {symbol}'
            lines = [clean[:m.start()].count('\n') + 1 for m in matches]
            rows.append(dict(category=category, symbol=symbol, source='sources/amba-pl011.c',
                             lines=';'.join(map(str, lines)), action=action, meaning=meaning))
    with (HERE / '关键依赖对照.csv').open('w', encoding='utf-8-sig', newline='') as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0])); w.writeheader(); w.writerows(rows)
    md = ['# PL011 关键依赖证据索引', '',
          '基于 Linux v6.12 amba-pl011.c 的代表接口。词法扫描去掉注释和字符串，未做预处理；宏、间接回调、内联展开和传递依赖需在编译阶段补齐。不能把此表当成完整未定义符号表。', '',
          '| 类别 | 接口 | 源码首次位置 | 首阶段处理 |', '| --- | --- | --- | --- |']
    for r in rows:
        line = r['lines'].split(';')[0]
        md.append(f"| {r['category']} | `{r['symbol']}` | [amba-pl011.c:{line}](sources/amba-pl011.c#L{line}) | {r['action']}：{r['meaning']} |")
    (HERE / '关键依赖索引.md').write_text('\n'.join(md) + '\n')
    print(f'已核实并索引 {len(rows)} 个代表依赖接口')


def diagram():
    out = ['<svg xmlns="http://www.w3.org/2000/svg" width="1920" height="1080" viewBox="0 0 1920 1080" role="img" aria-labelledby="title desc">',
           '<title id="title">Linux PL011 与当前 exokernel UART 的运行链路及适配边界</title>',
           '<desc id="desc">左侧是 Linux 驱动与 serial_core 调用关系，右侧是当前 EL0 UART 实现，底部为拟议兼容层。透明画布，实色卡片。</desc>']
    def txt(x,y,t,size=22,color='#233947',bold=False,mono=False):
        font='Menlo,Consolas,monospace' if mono else 'PingFang SC,Microsoft YaHei,Noto Sans CJK SC,Arial,sans-serif'
        out.append(f'<text x="{x}" y="{y}" font-family="{font}" font-size="{size}" font-weight="{700 if bold else 400}" fill="{color}">{escape(t)}</text>')
    def rect(x,y,w,h,fill,stroke='#D2DEE7'):
        out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="12" fill="{fill}" stroke="{stroke}"/>')
    txt(40,57,'PL011 驱动迁移：先明确运行链路与适配边界',40,bold=True)
    txt(40,95,'源码事实：Linux v6.12  ↔  当前 exokernel/libOS    |    首阶段建议：QEMU、单端口、PIO 收发',23,'#58707F')
    rect(40,119,1840,54,'#EDF4FA')
    txt(60,154,'关键区别：probe 注册端口；startup 才进入启用收发与申请 IRQ 的路径。',26,'#175B9C',True)
    txt(60,215,'Linux：PL011 驱动 + serial_core / TTY',28,'#175B9C',True)
    txt(1010,215,'当前系统：EL0 App + PL011 库 + 原生 syscall',28,'#007F76',True)
    cards = [
        (235,'1 发现与注册',['pl011_init → AMBA 匹配 → pl011_probe',
                         '分配对象 / 时钟句柄 → setup_port 映射 MMIO',
                         'register_port → uart_register_driver / uart_add_one_port'],
         ['UserBootInfo.uart 提供 base / size / INTID',
          'uart_echo::run → map_mmio → Uart::new',
          '资源来源相近；当前没有 AMBA / serial_core 对象模型']),
        (381,'2 打开与配置',['TTY open → uart_startup → ops->startup',
                         'pl011_startup：hwinit / request_irq / CR / IFLS / IMSC',
                         'serial_core 随后通过 set_termios 配置波特率和格式'],
         ['Notification::create → bind_irq → set_interrupt_masks',
          '现有 run 未调用 Uart::enable 来编程波特率',
          '当前读取既有硬件配置；不能视作完整冷启动初始化']),
        (527,'3 收发与中断',['RX：pl011_int → rx_chars → fifo_to_tty → TTY 缓冲区',
                         'TX：uart_write → TX FIFO → start_tx → tx_chars → DR',
                         '数据队列、错误状态和可写唤醒都属于适配范围'],
         ['IRQ → Notification::wait → read_word → write_word',
          '驱动清设备状态 → irq_ack 重新开放 IRQ',
          '当前是直接回显；尚无 Linux RX/TX 队列与回调桥接']),
        (673,'4 停止与释放',['close 路径 → ops->shutdown：关中断 / free_irq / 关 UART',
                         'remove 路径 → uart_remove_one_port / unregister_port',
                         '托管资源还需由设备生命周期完成释放'],
         ['当前 run 是永久循环；任务退出走内核统一清理',
          '兼容层需增加显式 stop / unbind / destroy / unmap',
          '失败回滚、处理中回调退出和重复启动要单独验证'])]
    for y,title,left,right in cards:
        rect(40,y,900,131,'#FFFFFF')
        rect(980,y,900,131,'#FFFFFF')
        txt(60,y+31,title,23,'#175B9C',True)
        txt(1000,y+31,title,23,'#007F76',True)
        for j,(a,b) in enumerate(zip(left,right)):
            txt(60,y+61+26*j,a,20)
            txt(1000,y+61+26*j,b,20)
        txt(949,y+78,'↔',27,'#8398A7')
    rect(40,827,1840,152,'#FFF8EE','#E8CBA5')
    txt(60,862,'拟议兼容层：底层资源适配 + 受限 serial_core 门面 + RX/TX 队列 + IRQ worker',27,'#AA560C',True)
    txt(60,901,'MMIO / IRQ / 内存有现有基础；需要补设备与时钟契约、数据队列、回调生命周期和错误回滚。',23)
    txt(60,939,'首阶段 CONFIG_DMA_ENGINE=n，使用 Linux 已有的 PIO 后备路径；不需要先实现 DMAengine。',23)
    result = HERE / 'compiled/summary.json'
    status = '状态：源码核查与设计；尚未完成 Linux 构建、兼容层实现或迁移验收。'
    if result.exists():
        measured = json.loads(result.read_text())
        status = f"已编译：PL011 {measured['pl011_undefined']} 个外部符号；导入串口框架后仍有 {measured['combined_undefined']} 个。兼容层与迁移运行尚未实现。"
    txt(40,1016,status,22,'#58707F')
    txt(40,1052,'依据：sources/amba-pl011.c · sources/serial_core.c · libos/src/apps/uart_echo.rs  |  2026-10-19',19,'#58707F')
    out.append('</svg>')
    (HERE/'PL011运行链路与适配边界.svg').write_text('\n'.join(out)+'\n')


if __name__ == '__main__':
    index()
    diagram()
