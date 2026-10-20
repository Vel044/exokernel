# Linux 驱动迁移与 libOS 兼容层调研

核查日期：2026-10-19。exokernel 依据当前工作区源码（包含尚未提交的修改）；Linux 编号对比固定为 **v6.12、AArch64 原生 ABI**，不含 AArch32。选择固定版本是为了复现，不表示它是最新版本。

## 结论

**可以做兼容层，但需要把“应用兼容”和“驱动兼容”分成两个工程。** 推荐在 EL0 的 libOS 中先实现明确范围的 POSIX 源码兼容；另为选中的 Linux 驱动建立内核内部 API 兼容环境。EL1 继续负责授权、地址空间、内存和调度等保护机制。

现在的 37 个 exokernel syscall 与 Linux 的 318 个具名 ARM64 编号不是同一抽象层，不能用相减衡量欠缺工作量。Linux 清单包含 2 个显式 `sys_ni_syscall` 占位，其余调用仍受架构和配置限制。

| 要兼容什么 | 使用者 | 需要实现的内容 | 初步判断 |
| --- | --- | --- | --- |
| POSIX 源码接口 | 可重新编译的 C/C++ 应用 | libc、fd、文件/设备读写、errno，逐步加入时间和线程 | 最适合作为近期目标 |
| Linux syscall 二进制 ABI | 不重新链接的 Linux ELF | 编号、参数布局、错误码、SVC 分流、装载/进程/TLS 等语义 | 可研究，但明显超出一层函数包装 |
| Linux 驱动内部 API | Linux 内核驱动 C 源码 | 内存、DMA、IRQ、锁、等待队列、总线和设备框架等 | 按驱动裁剪的 DDE 路线可行，需要逐项适配 |
| 完整 Linux 环境 | 原有 Linux 应用和内核驱动 | 运行 Linux 客体内核的 VMM、设备分配/虚拟化 | 另一条路线，不等于驱动已移入原生 libOS |

这些是可行性判断，并不是本项目已经具备的兼容能力。本次只做源码核查、清单和设计，没有移植驱动或运行兼容性测试。

## 阅读顺序

本系统独立参考：[Exokernel 系统调用分类 SVG](../Exokernel系统调用分类.svg) · [Exokernel 系统调用清单](../Exokernel系统调用清单.md)（按 6 类逐项说明参数、返回值与状态限制）。

第一个驱动样例：[Linux PL011 迁移方案](PL011/README.md) · [实际编译结果与兼容层缺口](PL011/编译结果与兼容层缺口.md) · [运行链路 SVG](PL011/PL011运行链路与适配边界.svg)。已编译 PL011 和串口框架对象：43 项直接依赖、组合后 119 项；原先 [52 项代表索引](PL011/关键依赖索引.md) 是源码阅读记录。

1. [系统调用对比](系统调用对比.md)：37 个本地调用的完整说明、常用 Linux 调用如何落地、当前缺口。
2. [seL4 项目与实现方案](seL4项目与实现方案.md)：LionsOS、Genode DDE Linux、seL4 musl、sDDF 和 Linux VM 的区别。
3. [Linux 驱动迁移路线](Linux驱动迁移路线.md)：驱动内部 API 对照、UART/xHCI 迁移范围、IR 的位置及正确性验收。
4. [Linux 完整编号表](Linux系统调用完整清单.md)：318 个具名编号；也可打开 [Linux CSV](linux-arm64-v6.12-syscalls.csv) 和 [exokernel CSV](exokernel-syscalls.csv)。

## 建议先做的两个可验收样例

- **应用接口样例**：把一个使用 `open/read/write/close` 的串口回显 C 程序重新编译，链接到本项目的 POSIX 子集。后端先使用现有 UART 驱动。明确限定 raw 字节模式，不承诺完整 tty/termios。
- **驱动迁移样例**：固定 Linux v6.12 的一个驱动及配置，先输出完整依赖清单，再选择 PL011 的限定 RX/TX/IRQ 功能作为移植实验。若裁掉 Linux serial_core 等功能，明确称为功能子集迁移，不能称为原驱动无修改移植。

第一条验证“让用户像 Linux 一样编程”，第二条验证“复用 Linux 驱动”。两个样例可共享底层资源接口，但验收结果独立。现有 Rust UART/CrabUSB 驱动可作为行为对照，不是 Linux 驱动迁移已经成功的证据。

## 清单复现与来源

在仓库根目录运行：

```sh
python3 docs/兼容性研究/generate_inventory.py
```

脚本离线读取已保存的上游表和当前 `abi/src/lib.rs`，并核对 `kernel/src/syscall/dispatch.rs` 的分发分支。产物哈希记录在 [inventory-manifest.json](inventory-manifest.json)。更新源码后可重跑；当前文档中的语义说明仍需人工复核。

上游数据文件保留原许可证标识：

- [Linux syscall.tbl（v6.12）](https://github.com/torvalds/linux/blob/v6.12/scripts/syscall.tbl)
- [ARM64 syscall ABI 选择](https://github.com/torvalds/linux/blob/v6.12/arch/arm64/kernel/Makefile.syscalls)
- [ARM64 syscall_64.tbl 链接目标](https://github.com/torvalds/linux/blob/v6.12/arch/arm64/tools/syscall_64.tbl)
- [ARM64 UAPI 入口](https://github.com/torvalds/linux/blob/v6.12/arch/arm64/include/uapi/asm/unistd.h)
- [Linux syscall 头生成器](https://github.com/torvalds/linux/blob/v6.12/scripts/syscallhdr.sh)
- [通用 ABI 筛选与头文件生成规则](https://github.com/torvalds/linux/blob/v6.12/scripts/Makefile.asm-headers)

筛选标签为 `common,64,renameat,rlimit,memfd_secret`；不要把其他架构专有项或 32 位 time32 项算入本表。来源快照在 `sources/`，仅包含生成清单所需的小文本文件。

本次验证：使用同一快照的 Linux 官方 `syscallhdr.sh --abis common,64,renameat,rlimit,memfd_secret --emit-nr` 生成头文件，与 CSV 按名称和编号逐项比对，318 项一致；本地 37 项与分发器分支集合一致；文档内本地链接均存在。这些是清单和文档验证，没有执行兼容层运行测试。
