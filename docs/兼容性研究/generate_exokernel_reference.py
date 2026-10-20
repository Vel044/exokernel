#!/usr/bin/env python3
"""按同一份接口说明生成 exokernel 清单与可编辑 SVG；不修改内核实现。"""
from pathlib import Path
from xml.sax.saxutils import escape
import re

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
DOCS = ROOT / "docs"
# 每行：编号、去掉 SYS_ 的名称、图中简述、寄存器参数、返回值、行为及约束。
GROUPS = [
    ("设备访问与 DMA", "映射设备寄存器；申请设备可见内存", "#B65B08", [
        (6,"MAP_MMIO","建立寄存器映射","PA, size_bytes, EL0_VA","0；失败为正整数状态码","验证设备授权、页对齐及 VA 冲突，建立 Device-nGnRE、EL0 可访问且不可执行的映射。size 向上取整到 4 KiB；映射后驱动直接 volatile 访问寄存器。"),
        (11,"UNMAP_MMIO","撤销寄存器映射","EL0_VA, size_bytes","0；失败为正整数状态码","按已登记的 VA 和取整后的长度撤销完整 MMIO 映射并刷新 TLB；不负责停止设备。"),
        (12,"DMA_ALLOC","分配 DMA 内存","size_bytes, alignment_bytes, EL0_VA","DMA address；失败为 u64::MAX","分配连续、清零的物理页并映射到指定 CPU VA，属性为 Normal Non-cacheable；当前无 IOMMU 路径返回 PA 作为设备 DMA 地址。alignment 必须为非零 2 的幂且不超过 2 MiB；VA 按 4 KiB 对齐。"),
        (13,"DMA_FREE","解除映射并回收 DMA","EL0_VA, size_bytes","0；失败为正整数状态码","按当前任务的 VA+取整长度核对记录，撤销映射并释放物理页。调用者必须先确保设备不再访问该缓冲区。"),
    ]),
    ("IRQ 与异步通知", "将硬件事件送达 EL0 等待线程", "#1562B5", [
        (7,"IRQ_BIND","绑定中断与通知","INTID, notification_handle, badge, target_cpu","0；失败为正整数或 SYS_ERR_*","只允许获授权的 SPI；验证通知对象及非零 badge。配置 GIC 路由并开放 IRQ。target_cpu 可为 IRQ_TARGET_CURRENT；设备自身中断使能仍由 EL0 写寄存器。"),
        (9,"IRQ_ACK","处理后重新开放 IRQ","INTID","0；失败为 1","确认当前绑定的中断处理完成，重新开放该 IRQ；不是设备寄存器清中断操作。EL1 收到 IRQ 时已先屏蔽并 EOI，EL0 必须先处理设备状态再 ACK。"),
        (10,"IRQ_UNBIND","屏蔽并解除 IRQ 绑定","INTID","0；失败为 1","解除当前任务的 IRQ 绑定，禁用相应中断；驱动应先完成设备侧停机和事件处理。"),
        (28,"NOTIFICATION_CREATE","创建通知对象","无","NotificationHandle；失败为 SYS_ERR_*","创建当前任务拥有的 pending badge 位图和等待队列；handle 不是地址。"),
        (29,"NOTIFICATION_SIGNAL","合并事件并唤醒等待者","notification_handle, badge","0；失败为 SYS_ERR_*","将非零 badge 按位 OR 合并；有等待者时取走 pending 并唤醒一个等待线程。多次相同 bit 不累计次数，也不是广播。"),
        (30,"NOTIFICATION_WAIT","有事件返回，否则阻塞","notification_handle","badge；失败为 SYS_ERR_*","原子取走并清空 pending；为空时入队并阻塞当前线程，其他线程可运行。当前没有超时参数。"),
        (31,"NOTIFICATION_POLL","非阻塞取走事件","notification_handle","badge；空为 SYS_ERR_WOULD_BLOCK","立即检查并消费 pending；不阻塞。以 EL1 实现为准：无事件返回 WOULD_BLOCK；EL0 封装把它转换为 Err，不是 Ok(0)。"),
        (32,"NOTIFICATION_DESTROY","销毁空闲通知对象","notification_handle","0；失败为 SYS_ERR_*","仍绑定 IRQ 或仍有等待线程时返回 BUSY；成功后旧 handle 失效。"),
    ]),
    ("内存与地址空间", "分开管理物理内存对象、映射与页表", "#007F76", [
        (14,"FRAME_ALLOC","分配普通内存对象","pages, align_pages","FrameHandle；失败为 SYS_ERR_*","按页分配并清零，登记当前 owner。当前分配器也使用连续物理页，但本接口不返回 DMA address，也不建立设备可见性契约。申请后需另外 map。"),
        (15,"FRAME_FREE","释放未映射的 Frame","frame_handle","0；失败为 SYS_ERR_*","检查 owner、generation；仍有 Mapping 引用时返回 BUSY，必须先撤销全部关联映射。"),
        (16,"FRAME_MAP","映射到当前地址空间","frame_handle, offset_pages, pages, VA, rights","MappingHandle；失败为 SYS_ERR_*","把 Frame 指定页范围映射到当前 VSpace，检查范围、权限和地址冲突；普通内存采用相应 RO/RW/RX 缓存映射。"),
        (17,"FRAME_UNMAP","撤销一个映射对象","mapping_handle","0；失败为 SYS_ERR_*","按 Mapping 记录撤销映射、更新引用和 TLB；不同时释放 Frame，也不是任意 VA 区间解除。"),
        (35,"VSPACE_CREATE","创建新的地址空间","无","VSpaceHandle；失败为 SYS_ERR_*","创建独立用户地址空间并登记管理关系；不复制应用、不装载 ELF、不自动启动线程。"),
        (36,"VSPACE_DESTROY","销毁空闲地址空间","vspace_handle","0；失败为 SYS_ERR_*","仅允许授权管理者销毁无线程、无 Mapping 的 VSpace；仍有引用返回 BUSY，初始地址空间不允许这样销毁。"),
        (37,"FRAME_MAP_TO","映射到指定地址空间","frame_handle, vspace_handle, offset_pages, pages, VA, rights","MappingHandle；失败为 SYS_ERR_*","在授权目标 VSpace 建立映射，参数用到 x5。返回的目标 VA 属于目标空间，不保证在调用者当前空间可解引用。"),
    ]),
    ("线程与调度", "创建执行上下文；控制运行状态与优先级", "#7053A3", [
        (18,"THREAD_CREATE","创建当前空间的线程","entry, arg, cpu, priority, mcp","ThreadHandle；失败为 SYS_ERR_*","在当前 VSpace 创建线程，内核分配线程栈和 IPC Buffer，校验入口可执行并发布为 Ready；无需另调 START。"),
        (38,"THREAD_CREATE_IN","在目标空间创建挂起线程","vspace_handle, config_VA","ThreadHandle；失败为 SYS_ERR_*","从调用者空间复制 ThreadCreateConfig；校验目标入口与栈权限，创建 Suspended 线程。目标栈由调用者预先映射。"),
        (39,"THREAD_START","将挂起线程置为就绪","thread_handle","0；失败为 SYS_ERR_*","将 Suspended 转为 Ready 并交给调度器；其他状态返回 BUSY，不保证调用后立刻获得 CPU。"),
        (40,"THREAD_SUSPEND","暂停尚未运行的线程","thread_handle","0；失败为 SYS_ERR_*","当前实现只把 Ready 转为 Suspended；已 Suspended 也成功。Running/Blocked 等状态返回 BUSY，不能强停任意运行线程。"),
        (41,"THREAD_DESTROY","销毁可回收的线程对象","thread_handle","0；失败为 SYS_ERR_*","拒绝 Running/Blocked；回收线程对象、自动 IPC Buffer 并解除 VSpace 线程引用。调用者配置的栈 Frame/Mapping 仍需单独管理。"),
        (19,"THREAD_EXIT","结束当前线程","exit_code","不返回到退出线程","取消 IPC 等待，回收该线程由内核拥有的栈/IPC Buffer，交给调度器；没有同 owner 的存活线程时走 task 退出路径。不是 pthread_join 接口。"),
        (20,"THREAD_YIELD","主动让出执行机会","无","恢复执行后返回 0","把当前执行机会交给调度器，线程仍可运行；不是 sleep，也不保证所有较低优先级线程都得到运行。"),
        (21,"THREAD_SET_PRIORITY","调整线程基础优先级","thread_handle, priority","0；失败为 SYS_ERR_*","在授权范围内调整优先级。范围 0..63，数值越大优先级越高，受线程 mcp 限制。"),
        (33,"THREAD_RUNTIME","查询累计运行 tick","thread_handle","运行 tick；失败为 SYS_ERR_*","读取内核记账的 CPU 运行时间，不是墙上时钟，也不是睡眠接口。"),
    ]),
    ("Endpoint 同步 IPC", "通过消息与 Reply 连接用户态服务", "#265077", [
        (22,"ENDPOINT_CREATE","创建同步通信端点","无","EndpointHandle；失败为 SYS_ERR_*","创建当前任务拥有的发送/接收会合端点。这里的 Endpoint 不是 USB endpoint。"),
        (23,"ENDPOINT_SEND","发送消息并等待接收","endpoint_handle","交付后 0；失败为 SYS_ERR_*","消息来自当前线程 IPC Buffer；没有接收者时发送线程阻塞，有接收者时交付。不等待服务处理结果。"),
        (24,"ENDPOINT_RECV","接收消息，无发送者则等","endpoint_handle","接收后 0；失败为 SYS_ERR_*","消息写入接收线程 IPC Buffer；没有发送者时阻塞。CALL 请求额外带 ReplyHandle。"),
        (25,"ENDPOINT_CALL","发送请求并等待回复","endpoint_handle","收到回复后 0；失败为 SYS_ERR_*","请求来自 IPC Buffer；交付后调用者继续等待服务器 Reply。回复内容也放入调用者 IPC Buffer。"),
        (26,"ENDPOINT_REPLY","用回复凭据完成请求","reply_handle","0；失败为 SYS_ERR_*","以当前线程 IPC Buffer 为回复内容，核对 Reply 归属与接收服务器线程；唤醒原调用者并消耗 ReplyHandle。"),
        (27,"ENDPOINT_REPLY_RECV","回复后接收下一请求","endpoint_handle, reply_handle","接收后 0；失败为 SYS_ERR_*","在一次 syscall 中完成回复及后续接收；没有下一请求仍会阻塞。适合服务器循环。"),
        (34,"ENDPOINT_DESTROY","销毁空闲通信端点","endpoint_handle","0；失败为 SYS_ERR_*","存在等待的发送/接收者或未完成 Call/Reply 时拒绝；成功后旧 handle 失效。"),
    ]),
    ("输出与任务退出", "提供调试输出与整体任务生命周期接口", "#56636D", [
        (2,"PUTS","经内核输出调试字符串","string_VA, length_bytes","正常返回 0","EL1 逐字节读取用户 VA 并写 UART。当前实现直接解引用，没有在此函数中做完整用户缓冲区检查；使用有效地址。libOS 接管 UART 后 puts 通常直接在 EL0 输出。"),
        (5,"EXIT","结束当前 EL0 任务","exit_code","不返回","进入 task::exit_current：停止其他 CPU 上的共享任务执行，清理 IRQ、线程、IPC、映射等资源并结束运行；不是单个用户进程的 POSIX exit。"),
    ]),
]


def generate_markdown():
    lines = ["# Exokernel 系统调用接口清单", "", "核查日期：2026-10-19。依据当前工作区实现；共 **37 个系统调用，分为 6 类**。本文只说明本系统已存在的接口，不包含 Linux 兼容层设想。", "",
             "![系统调用分类总览](Exokernel系统调用分类.svg)", "", "## 调用入口与记号", "",
             "- EL0 通过 `runtime::svc/svc5/svc6` 设置 x8=调用编号、x0…x5=参数，再执行 `svc #0`；EL1 分发器处理，通常以 x0 返回结果。",
             "- 下表参数按 x0、x1…顺序排列。VA 是 CPU 虚拟地址，PA 是物理地址；handle 是带对象身份的凭据，不能当指针或 fd。",
             "- 页单位为 4 KiB；只有标为 bytes 的参数按字节计。Frame 映射权限目前为 R、RW、RX，不接受任意组合。",
             "- 资源操作的校验依接口而异：MMIO/IRQ 对照授权记录，对象操作检查归属、generation 和状态；不能假定所有接口都采用同一种检查或错误编码。",
             "- 系统调用全集来自 abi/src/lib.rs。编号 8 是历史空洞；SYS_ERR_* 是错误常量，不算接口。", "",
             "## 分类索引", "", "| 分类 | 数量 | 管什么 |", "| --- | ---: | --- |"]
    for title, subtitle, _, rows in GROUPS:
        lines.append(f"| {title} | {len(rows)} | {subtitle} |")
    for title, subtitle, _, rows in GROUPS:
        lines += ["", f"## {title}（{len(rows)} 项）", "", subtitle + "。", "",
                  "| 编号 / 接口 | 输入参数（x0 起） | 返回值 | 具体作用与约束 |",
                  "| --- | --- | --- | --- |"]
        for no, name, short, args, result, details in rows:
            lines.append(f"| {no} · `SYS_{name}`<br>{short} | `{args}` | {result} | {details} |")
    lines += ["", "## 容易混淆的边界", "",
              "**Frame 与 DMA**：两者底层都使用内存页。Frame 面向 CPU 地址空间，返回对象 handle，再单独建立映射；DMA_ALLOC 直接组合物理分配与指定 VA 映射，返回设备地址。当前两者都可能连续，区别不能简单写成‘连续和不连续’。",
              "", "**IRQ 与 Notification**：IRQ 是设备事件来源，Notification 是唤醒线程的内核对象；软件 SIGNAL 也能发送通知。典型顺序为硬件 IRQ → EL1 屏蔽并 EOI → 投递 badge → EL0 WAIT 返回 → 处理设备状态/Event Ring → IRQ_ACK 重新开放。",
              "", "**Endpoint 与 Notification**：Endpoint 携带消息并提供同步会合/回复；Notification 合并事件位图，不传输任意 payload。IPC Buffer 是 48 字节的 IpcMessage，包含 label、4 个 u64 words 和 reply；大块数据需另行安排共享内存。",
              "", "**Task、VSpace 与 Thread**：Task 是当前授权/资源管理上下文；VSpace 是地址空间；Thread 是执行上下文。创建 VSpace 不等于启动进程。ELF 装载由 EL0 ProcessBuilder 组合 Frame、VSpace、Thread 完成。",
              "", "**这些是用户态普通操作，不是新增 syscall**：UART read_word/write_word/set_interrupt_masks；xHCI 寄存器编程和 Event Ring 解析；Rust poll/await；runtime::delay_ns 的计数器忙等；ext4 路径和文件解析。", "",
              "## 返回值与当前实现差异", "",
              "当前 ABI 没有统一成一种错误返回约定，使用时必须按接口处理：", "",
              "| 接口范围 | 错误约定 |", "| --- | --- |",
              "| Frame / VSpace / Thread / Endpoint / Notification 对象操作 | SYS_ERR_*；封装通常通过 is_sys_error 检查 |",
              "| MAP_MMIO / UNMAP_MMIO / DMA_FREE | 0 成功；正整数表示不同错误 |",
              "| IRQ_BIND | 0 成功；既可能返回正整数，也可能返回 SYS_ERR_INVALID |",
              "| IRQ_ACK / IRQ_UNBIND | 0 成功；1 失败 |",
              "| DMA_ALLOC | 成功返回 DMA address；u64::MAX 失败 |",
              "| 未识别的 syscall 编号 | 当前返回 u64::MAX |", "",
              "| 通用错误名 | u64 编码 | 含义 |", "| --- | --- | --- |",
              "| INVALID | MAX | 参数或请求无效 |", "| NO_MEMORY | MAX−1 | 内存不足 |",
              "| NO_SLOT | MAX−2 | 对象槽位不足 |", "| DENIED | MAX−3 | 授权/权限不满足 |",
              "| BUSY | MAX−4 | 当前状态或引用关系不允许操作 |", "| CONFLICT | MAX−5 | 映射等状态冲突 |",
              "| NOT_FOUND | MAX−6 | 对象不存在、失效或无法由当前调用者解析 |",
              "| WOULD_BLOCK | MAX−7 | 非阻塞检查当前没有可消费事件 |", "",
              "本次发现的文档/封装差异：Notification::poll 的注释说空时返回 0，但 EL1 实际返回 WOULD_BLOCK，封装返回 Err；runtime::irq_ack 丢弃底层返回值，清单中的返回值指原生 syscall。Frame::free 的旧注释容易让人误以为自动回收映射，EL1 实际在仍有映射时返回 BUSY。此处记录实现事实，未修改源码。", "",
              "## 常见使用顺序", "",
              "```text\nUART：MAP_MMIO → NOTIFICATION_CREATE → IRQ_BIND\n      → EL0 打开设备中断 → WAIT → EL0 寄存器读写/清状态 → IRQ_ACK\n\nDMA：DMA_ALLOC → EL0 写描述符/缓冲区 → 设备使用\n     → 确认设备不再访问 → DMA_FREE\n\n普通内存：FRAME_ALLOC → FRAME_MAP → CPU 访问 → FRAME_UNMAP → FRAME_FREE\n\n新地址空间：VSPACE_CREATE → 映射 ELF/栈 → THREAD_CREATE_IN → THREAD_START\n            → 线程结束/销毁 → 解除映射/释放 Frame → VSPACE_DESTROY\n\n同步服务：ENDPOINT_CREATE → 客户端 CALL ↔ 服务端 RECV/REPLY_RECV\n```", "",
              "## 源码定位", "",
              "| 层次 | 文件 |", "| --- | --- |",
              "| 编号、句柄、配置结构 | [abi/src/lib.rs](../abi/src/lib.rs) |",
              "| EL0 SVC、MMIO/DMA/IRQ 封装 | [runtime/mod.rs](../libos/src/runtime/mod.rs) |",
              "| EL0 对象封装 | [kernel_api](../libos/src/kernel_api/mod.rs) |",
              "| EL1 分发、MMIO/DMA/IRQ 实现 | [syscall/dispatch.rs](../kernel/src/syscall/dispatch.rs) |",
              "| Frame / VSpace | [frame.rs](../kernel/src/memory/frame.rs)、[vspace.rs](../kernel/src/memory/vspace.rs) |",
              "| Thread / IPC / Task | [thread.rs](../kernel/src/object/thread.rs)、[ipc.rs](../kernel/src/object/ipc.rs)、[task.rs](../kernel/src/object/task.rs) |", "",
              "图和本清单由 [generate_exokernel_reference.py](兼容性研究/generate_exokernel_reference.py) 的同一份数据生成。脚本核对 37 个名称和编号与 ABI 定义一致；参数、返回和行为依据内核实现人工核查。"]
    (DOCS / "Exokernel系统调用清单.md").write_text("\n".join(lines) + "\n")


def generate_svg():
    s = ['<svg xmlns="http://www.w3.org/2000/svg" width="1920" height="1080" viewBox="0 0 1920 1080" role="img" aria-labelledby="title desc">',
         '<title id="title">Exokernel 系统调用：六类、37 项</title>',
         '<desc id="desc">当前源码系统调用分类及用途。画布透明，卡片实色；所有接口名称省略 SYS_ 前缀。</desc>']
    def rect(x,y,w,h,fill,stroke="none",rx=14):
        s.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" fill="{fill}" stroke="{stroke}" stroke-width="1.5"/>')
    def text(x,y,value,size=22,color="#243441",weight="400",mono=False):
        family = 'Menlo,Consolas,monospace' if mono else 'PingFang SC,Microsoft YaHei,Noto Sans CJK SC,Arial,sans-serif'
        s.append(f'<text x="{x}" y="{y}" font-family="{family}" font-size="{size}" font-weight="{weight}" fill="{color}">{escape(value)}</text>')
    text(40,61,"Exokernel 系统调用接口",44,weight="700")
    text(40,99,"6 类 · 37 项  |  EL1 提供资源保护与调度机制，EL0 组合成驱动和系统服务",24,color="#526572")
    rect(40,122,1840,58,"#EFF5FA","#D7E3ED")
    text(62,159,"EL0：runtime / kernel_api",25,weight="600")
    text(480,159,"→   SVC #0：x8 编号 · x0…x5 参数   →",24,color="#1562B5")
    text(1080,159,"EL1：校验、处理并返回",25,weight="600")
    text(1540,159,"接口名省略 SYS_",22,color="#526572")
    for i,(title,subtitle,color,rows) in enumerate(GROUPS):
        x=40+(i%3)*620; y=202+(i//3)*385
        rect(x,y,600,365,"#FFFFFF","#CDD8E0")
        rect(x,y,600,55,color,rx=0)
        text(x+20,y+37,title,28,color="#FFFFFF",weight="700")
        text(x+515,y+36,f"{len(rows)} 项",23,color="#FFFFFF",weight="600")
        text(x+20,y+82,subtitle,19,color="#60717D")
        for j,(no,name,short,*_) in enumerate(rows):
            yy=y+116+j*28
            text(x+18,yy,str(no),17,color="#778995",mono=True)
            text(x+53,yy,name,20,color=color,weight="600",mono=True)
            text(x+322,yy,short,19)
        if i==0:
            rect(x+20,y+244,560,98,"#FFF5E9",rx=8)
            text(x+36,y+273,"MMIO：映射后直接访问设备寄存器",21,color=color,weight="600")
            text(x+36,y+303,"DMA：CPU 用 VA；设备用 DMA address",21,color=color)
            text(x+36,y+329,"分配、映射、回收均受内核记录约束",18,color="#60717D")
        if i==2:
            text(x+22,y+340,"Frame 是内存对象；Mapping 是映射记录",20,color=color,weight="600")
        if i==4:
            text(x+22,y+340,"消息经线程 IPC Buffer 传递；此处非 USB 端点",19,color=color)
        if i==5:
            rect(x+20,y+190,560,152,"#F0F4F7",rx=8)
            text(x+36,y+223,"系统调用边界",22,color=color,weight="700")
            text(x+36,y+254,"UART 读写、USB 协议、Event Ring 解析",21)
            text(x+36,y+284,"以及文件系统逻辑由 EL0 实现。",21)
            text(x+36,y+318,"这些操作并非每一步都要进入内核。",20,color="#60717D")
    text(40,994,"阅读重点：设备访问 4 项  +  IRQ 与通知 8 项，是用户态驱动接入内核的主要接口。",24,weight="600")
    text(40,1033,"来源：abi/src/lib.rs · kernel/src/syscall/dispatch.rs  |  2026-10-19 源码核查  |  编号 8 为历史空洞",19,color="#60717D")
    text(40,1061,"完整参数、返回值、状态限制和错误约定：Exokernel系统调用清单.md",18,color="#60717D")
    s.append('</svg>')
    (DOCS / "Exokernel系统调用分类.svg").write_text("\n".join(s)+"\n")


if __name__ == "__main__":
    actual={name:int(no) for name,no in re.findall(r'pub const SYS_((?!ERR_)[A-Z_]+): u64 = (\d+);',(ROOT/'abi/src/lib.rs').read_text())}
    rows=[r for g in GROUPS for r in g[3]]
    expected={r[1]:r[0] for r in rows}
    assert len(rows)==len(expected)==37 and actual==expected, "接口清单与 ABI 不一致"
    generate_markdown()
    generate_svg()
    print("37 个接口核对通过；已生成 Markdown 清单和透明画布 SVG")
