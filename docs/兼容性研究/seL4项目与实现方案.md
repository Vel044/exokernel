# seL4 相关项目与本项目兼容层方案

调研日期：2026-10-19。项目能力以链接中的版本/文档为依据；下文的本项目目录与阶段安排均为建议，尚未实现。

## 各项目解决的不是同一个问题

| 项目 | 做了什么 | 可以借鉴什么 | 不能据此得出的结论 |
| --- | --- | --- | --- |
| seL4 musllibc / libsel4muslcsys | seL4 上的 libc 移植与最小 syscall 后端 | libc 需求与底层内核机制分离 | seL4 本身已经兼容所有 Linux syscall |
| LionsOS 0.4.0 libc | musl 加 EL0 POSIX 子集 | 重定向 libc 内部 syscall，接文件/串口/网络服务 | 未修改 Linux ELF 可直接运行，或完整 POSIX 已支持 |
| Genode DDE Linux | 为 Linux 驱动提供用户态内核内部 API 环境 | lx_emul、用户态调度、驱动入口/服务适配 | 复制一个库就能在本 exokernel 上跑任意 Linux 驱动 |
| sDDF | seL4 驱动框架、接口、共享数据通道及相关工具 | 驱动与客户端服务边界、通知和共享内存设计 | 通用 Linux 驱动 API 或 POSIX 兼容层 |
| CAmkES VMM / Linux VM | 在 seL4 上运行客体操作系统 | Linux 作为整体复用、设备直通或虚拟化 | Linux 驱动已被翻译成原生 libOS 驱动 |

## 1. LionsOS：最贴近“在 libOS 实现 POSIX”

LionsOS 0.4.0 的 libc 由 musl fork、POSIX 后端和编译器运行时组成。它把 musl 原本的架构陷入操作重定向到 `__sysinfo` 指向的 EL0 dispatcher，因此“libc 内部叫 syscall”不代表每次都会陷入 seL4。

官方文档明确它是子集：有文件操作、单调时间和部分 socket；没有 UDP/符号链接；`ppoll` 忽略 timeout/sigmask；内存分配不能回收。这些限制说明兼容必须逐项声明，不能只凭同名函数判断。[libc 机制与限制](https://www.lionsos.org/docs/use/language_support/libc/)

0.4.0 将此前散落在组件内的 POSIX 支持整理为共享库，并提供 POSIX 测试示例。[发布说明](https://www.lionsos.org/docs/releases/0.4.0/)

仓库入口：[au-ts/lionsos](https://github.com/au-ts/lionsos)。重点阅读路径为 `lib/libc/posix/`、`lib/libc/libc.mk`、`include/lions/posix/posix.h`；它依赖 LionsOS/sDDF 的服务组织，不能原样当作本项目后端。

对本项目的启发：复用 libc 中字符串、格式化、stdio 等通用实现，把文件描述符、设备访问、内存与时间操作接到本 libOS；实际需要保护操作时才调用 exokernel syscall。

## 2. Genode DDE Linux：最贴近“移植 Linux 内核驱动”

DDE 是 Device Driver Environment。Genode 将 Linux 驱动及所需 Linux 子系统编译进用户态组件，用 `lx_emul` 提供 C 接口，由 `lx_kit` 等机制支撑调度、资源和运行环境，再把驱动服务连接到 Genode。[DDE 架构说明](https://genode.org/documentation/release-notes/21.08#Linux-device-driver_environment_re-imagined)

这里处理的是 Linux 内部 API，不是应用 syscall。复用原始头文件有利于保留类型，但仍需要处理依赖、初始化顺序和并发；生成未使用函数的桩也不能替代实际执行路径的语义实现。

Genode 可运行在 seL4 上；25.11 增加了 AArch64 seL4 支持，核查平台为 QEMU virt 和 i.MX8Q EVK。不能因此推断 Pi5 上全部驱动都能直接用。[平台说明](https://genode.org/documentation/release-notes/25.11)

该工作仍在维护：26.08 说明 DDE Linux 的 ARM SoC 部分已更新到 Linux 6.18.19。[发布说明](https://genode.org/documentation/release-notes/26.08)

源码入口：[Genode（Codeberg）](https://codeberg.org/genodelabs/genode)，历史镜像 [GitHub](https://github.com/genodelabs/genode)。架构文档指向 `repos/dde_linux/src/include/lx_emul`、`repos/dde_linux/src/lib/lx_emul`、`repos/dde_linux/src/lib/lx_kit` 及 `tool/dde_linux`。路径随版本可能调整，应固定目标 tag 后核查。**本次核查了官方架构及版本说明，没有编译 Genode 或验证这些库能直接移植到本项目。**

对本项目的启发：先允许 Linux C 驱动保留 C 形式，通过 FFI 调用 EL0 Rust 适配层；可先运行一个驱动的限定依赖集，再研究自动化提取/IR。无需一开始同时完成 C→Rust 翻译。

## 3. seL4 musl 和 sDDF 的边界

seL4 官方将 `libsel4muslcsys` 定义为最小 musl syscall 实现；这些用户态原型库并不自动继承 seL4 内核的形式化保证。[官方库说明](https://docs.sel4.systems/projects/user_libs/)

源码入口：[seL4/musllibc](https://github.com/seL4/musllibc)、[seL4/seL4_libs](https://github.com/seL4/seL4_libs)。本次仅确认官方定义和项目入口，不把未核实的 handler 数量列为兼容覆盖率。

[sDDF 仓库](https://github.com/au-ts/sddf) 定位为 seL4 设备驱动所需接口、库和工具。可借鉴其驱动服务划分，但仍需对接本项目 MMIO/DMA/IRQ 授权与调度机制。

## 4. Linux VM 的位置

[CAmkES VMM](https://docs.sel4.systems/projects/camkes-vm/) 及 [仓库](https://github.com/seL4/camkes-vm) 展示了在 seL4 上运行 Linux 客体的路线。Linux syscall 与内核驱动由客体 Linux 提供，外层负责虚拟化和资源安排。

它适合“尽快保留完整 Linux 软件环境”的目标，但不能作为“Linux 驱动已移入本 exokernel 用户态”的验收结果。本项目是否采用该路线还需要单独评估 EL2/VMM 和设备隔离能力。

## 本项目建议架构

```text
重新编译的 POSIX 应用              Linux 驱动 C 源码（固定版本和配置）
          │                                  │
     libc / POSIX API                 Linux 驱动环境 / lx_compat
          │                                  │
   fd / VFS / 设备文件                  kmalloc / DMA / IRQ / 同步
          │                                  │
  现有 Rust 驱动或迁移驱动  ←── 驱动对外服务接口 ──┘
          │
   kernel_api / runtime
          │ 原生 exokernel syscall
          ▼
 EL1：授权、Frame/VSpace、IRQ投递、线程调度、IPC
```

这是职责图，不表示每一次 POSIX 调用都依次经过所有模块。例如字符串函数不需要内核，映射后的 UART 寄存器访问通常也不陷入 EL1。

建议新增模块（尚不存在）：

| 模块 | 职责 |
| --- | --- |
| `libos/src/compat/posix/` | POSIX 后端、错误转换、fd 表及同步 C 接口 |
| `libos/src/compat/linux_driver/` | 只覆盖被选中驱动依赖的 Linux 内部 API、C/Rust 桥接 |
| libc 移植构建产物 | 复用 musl 通用 C 实现，替换其操作系统后端及启动代码 |
| `libos/src/fs/` | 文件后端；增加 VFS 接入时保持与块驱动分层 |
| `kernel_api` / `runtime` | 继续封装本地对象和原生 SVC；不把 Linux 结构体泄露到全部代码中 |

Linux syscall 编号可作为 **EL0 libc dispatcher 的内部选择号** 使用，不必复制到 EL1 原生 ABI。

## POSIX 子集怎样分阶段

### P0：单应用、单线程、可重新编译

目标：一个 C 串口回显程序，在 Linux 与本系统上用同一应用源码构建运行。

- 静态链接、明确启动约定；解决 C ABI、crt、libc 初始化和分配器接入。
- `open/read/write/close`、基础 `fstat`、有限 `fcntl`；fd 0/1/2 有明确后端。
- 串口采用明确声明的 raw 字节语义，不宣称完整 Linux tty；POSIX 应用不直接拿到任意 MMIO/DMA 权限。
- 一个进程内实现 fd 表，记录访问模式、设备对象、关闭状态；同名设备是否独占明确规定。
- 读操作支持短读，非阻塞无数据返回 EAGAIN，零长度请求、无效 fd、关闭后的访问均定义行为。
- 同步 C read 与 Rust async USB 的桥接另做，不把 `.await` 当作 C ABI。先用 UART 降低依赖。

入口可以先由现有 Rust App 调用静态链接的 C 程序入口；这样不必一开始承诺完整 Linux ELF 装载。单线程 errno 可先有清晰实现，多线程前必须落实 TLS。

### P1：只读文件和基本时间

增加只读 ext4 的 open/read/lseek/stat，明确不支持目录修改；匿名内存管理及回收；CLOCK_MONOTONIC；基于定时事件的阻塞睡眠和有限 poll。将忙等实现与真正睡眠区分。

定时等待可以设计为计时服务向 Notification 发信号，也可以增加最小超时原语；应先核查可授予的 timer 资源与定时器路由。现有 syscall 表没有成熟的 timed wait，不能认定只靠改 libc 就全部完成。

### P2：多线程和更复杂应用

先核查并实现 TLS/TPIDR_EL0 上下文、线程退出通知和 join、互斥/条件变量、竞态安全的等待注册、超时、线程局部 errno。然后逐步扩展 pthread 和文件共享语义。

Notification 可作为唤醒机制，但“检查条件→注册等待”的过程必须避免丢唤醒。对于重编译的库可以设计自己的锁协议；对于原版 Linux futex ABI 必须满足它的原子比较并等待等约束。

### P3：确有需求再考虑未修改 Linux ELF

仅修改/替换 libc **截获不了**二进制内联的 Linux SVC。当前 dispatcher 也没有用户态 syscall 转发机制，因此至少需要设计：

1. 装载时显式标记 Linux ABI；在 EL1 区分 Linux 和原生编号，避免数字碰撞。
2. 选择受控的 Linux ABI handler 或转发至 EL0 personality 服务；转发涉及寄存器、缓冲区校验、阻塞和返回上下文。
3. 原生 libOS 服务与 Linux 应用如何分别发起原生调用，避免仅凭当前 VSpace 标记导致服务自身 SVC 被误分流。
4. Linux 初始栈、auxv、TLS、动态链接器（若支持）、结构体布局、errno、进程/信号及文件语义。

建议只有在明确需要某个现成 ELF 时，才按该 ELF 的依赖和行为扩展；不能先承诺“Linux 应用都能跑”。

## 验收方式

兼容性按支持矩阵验收，不按函数数量验收。每个接口记录支持参数、返回值、错误码、阻塞/取消行为、并发条件和排除项。

- 应用侧：同一测试源码分别在 Linux 和本项目运行，比较字节流、短读写、offset、errno、资源释放结果。
- 并发侧：覆盖事件先于 wait、事件发生于准备睡眠期间、多个完成合并通知、关闭/取消与完成竞态。
- 性能侧：记录陷入次数、数据复制次数、等待时 CPU 占用和延迟；正确性通过后再优化。
- 二进制侧：只有实际执行了未重新链接的目标 ELF 并验证依赖，才声明对应二进制兼容范围。

这些测试建立有限范围内的证据；形式化证明需要另行定义模型、假设和证明义务，不由“使用 seL4 项目作为参考”自动获得。
