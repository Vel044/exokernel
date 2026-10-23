# Exokernel：面向机器人驱动与运行时的外核原型

本项目探索面向异构机器人的操作系统：EL1 内核负责资源授权、地址空间、线程和中断投递；EL0 libOS 负责设备协议、文件系统、运行时和应用策略。当前实现为 AArch64 原型，开发入口为 QEMU virt，另有 Raspberry Pi 5 启动与设备适配路径。

**第一次参与驱动迁移工作，请先读 [驱动协作入口](docs/驱动移植/README.md)。** Linux → IR 自动化目前是待开展的协作方向；仓库中的 Rust 驱动适配代码是目标侧参考，不是自动翻译 Linux 驱动的产物。

## 目录与职责

| 目录 | 内容 | 初次协作是否需要 |
| --- | --- | --- |
| `abi/` | EL0/EL1 共享结构、系统调用编号与约定 | 必读 |
| `kernel/src/` | 启动、资源保护、内存、GIC、线程与调度 | 先读资源及系统调用路径 |
| `libos/src/kernel_api/` | Frame、Notification、线程等接口封装 | 按样例阅读 |
| `libos/src/runtime/` | SVC 封装、日志、USB Future 执行器 | 必读相关部分 |
| `libos/src/drivers/` | xHCI、DMA、USB 串口、UVC、virtio-blk 等第一方适配 | 驱动协作主入口 |
| `libos/src/apps/` | UART 回显、设备验收和机器人应用策略 | UART 是入门例子 |
| `libos/src/fs/` | ext4 读取与采集文件处理 | 后续扩展 |
| `libos/vendor/` | 第三方源码快照及其许可证 | 查具体协议实现时阅读 |
| `platform/`、`qemu/` | 设备树快照、启动配置和脚本 | 运行时阅读 |
| `act-runtime/`、`act-kernels-acl/` | ACT 推理与可选计算库适配 | 第一阶段可跳过 |
| `scripts/`、`experiment/` | 数据准备、实验与已有结果 | 按需 |

当前 UART 使用 PL011 库；xHCI 使用 CrabUSB；virtio-blk 使用 virtio-drivers；ext4 读取使用 ext4-view。USB 串口和 UVC 位于 xHCI 之上，ext4 位于块设备之上，不能把这些层当成同一种驱动迁移对象。

## DTB 与设备发现（ABI v13）

完整 DTB 只读交给 libOS，EL0 在 `libos/src/drivers/dtb.rs` 解析设备，生成
`drivers/platform.rs::DeviceInfo`。`UserBootInfo` 只传 DTB VA、有效长度和 CPU 数。
内核不解析设备树节点；启动所需 GIC/Timer/CPU、诊断 UART 及初始设备授权由
`kernel/src/resource/resources.rs` 的 QEMU/Pi5 配置提供。设备树可见性不代表
访问授权，`MAP_MMIO` 和 `IRQ_BIND` 仍检查权限。新板卡/拓扑变化要更新平台配置。
详情见 [启动交接与内存说明](docs/Stack和Heap和Userbootinfo.md#2当前的userbootinfo)。
已有 UserBootInfo SVG/PPT 为 v12 讲解材料，设备字段图与当前 ABI 不同。

## 最小构建与运行

ACT 运行时及所需 ACL 微内核源码已包含在 `vendor/rutorch/`，构建不再依赖相邻
`rutorch` 仓库。快照来源和摘要记录在 `vendor/rutorch/manifest.json`，可用
`python3 scripts/sync_rutorch_runtime.py --verify` 检查。模型、数据盘、EFI/ELF、
编译日志和测试生成物不纳入 Git；实际 ACT 测试仍需另行准备模型与观测数据。

以下启动步骤面向 **macOS + Homebrew**。Linux/Windows 开发者可安装 AArch64 QEMU、Rust 目标和 UEFI 固件，按相同 QEMU virt 平台理解代码；宿主启动脚本、启动盘处理和固件路径需自行调整。`qemu/run.sh` 使用 macOS 的 `hdiutil`，不能保证跨宿主原样运行，本次交接不做适配。Pi5 路径见 `build_pi5.sh`，本导读不宣称完成了真机验收。

先安装 Rust/rustup，再准备工具和目标：

```bash
brew install qemu llvm mtools
rustup toolchain install stable nightly
rustup target add --toolchain stable aarch64-unknown-uefi
rustup target add --toolchain nightly aarch64-unknown-none
```

`libos/rust-toolchain.toml` 当前使用浮动 nightly；复现实验应记录 `rustc +stable -Vv`、`rustc +nightly -Vv` 和 `qemu-system-aarch64 --version`。默认构建场景是 scservo，首次接触请显式选无外设场景：

```bash
LIBOS_APP=system-smoke bash build.sh
```

构建先产生 `libos/libos.elf`，再将其嵌入 `target/aarch64-unknown-uefi/debug/BOOTAA64.efi`。这两个文件是生成物，不需要从其他开发者处复制。

第一次运行前创建 FAT 启动盘（只在文件不存在时执行）：

```bash
if [ ! -e qemu/esp.img ]; then
  mkfile -n 64m qemu/esp.img
  mformat -i qemu/esp.img -F ::
  mmd -i qemu/esp.img ::/EFI ::/EFI/BOOT
fi
LIBOS_APP=system-smoke QEMU_USB_MODE=none QEMU_SUDO=0 bash qemu/run.sh
```

先检查 `qemu/config.sh` 的 EDK2 固件路径是否匹配本机；当前默认路径是 Apple Silicon Homebrew 的 `/opt/homebrew/share/qemu/`。启动盘缺失、固件路径错误与驱动错误应分别定位。

UART 接口演示：

```bash
LIBOS_APP=uart-echo QEMU_USB_MODE=none QEMU_SUDO=0 bash qemu/run.sh
```

看到 `[libos] UART echo ready` 后输入字符，观察回显。这个例子覆盖 MMIO 和 IRQ，**不覆盖 DMA**。USB 模拟串口可另选 `LIBOS_APP=usb-echo QEMU_USB_MODE=ftdi`；UVC 和机器人应用另需真实设备/数据准备。完整场景映射以 `scripts/libos_features.sh` 为准。


## 回归验证

2026-10-20 验证：19 个受支持场景编译、IDE 检查、12 个宿主单元测试、9 组 QEMU
运行测试，以及 portable/ACL 各 5 组 ACT 正确性测试通过（共比较 6000 个动作值）。
暂存源码导出到独立目录后，离线构建及 TCG/HVF 的 system/process 测试也通过。
Pi5 仅完成编译和设备树解析检查；真实 UVC/舵机、机器人运动及 ACT 长稳/性能基准未运行。
详细结果见 [验证记录](docs/validation/2026-10-20.json)。

```bash
cargo test --manifest-path tools/dtb-check/Cargo.toml
python3 scripts/sync_rutorch_runtime.py --verify
python3 scripts/test_dtb_handoff.py --hvf
```

QEMU 测试使用独立临时启动盘和虚拟外设，结果保存在 `target/dtb-regression/`，
不会使用真实机器人。`--hvf` 仅适用于 Apple Silicon；其他宿主省略该选项，并用
`--code`、`--vars` 指定本机 AArch64 EDK2 固件。覆盖固件 DTB/内嵌 DTB、映射
保护、独立 VSpace、UART、USB 串口、USB Hub 和 USB/调度并发。

ACT 正确性另用 `scripts/run_act_release.py --provider portable --case 000 --phase correctness`；
需要另行准备模型、五组冻结观测及 PyTorch golden，并为 ELF 审计配置 `READELF` 和 `NM`。
`--provider acl` 还需要 AArch64 C++ 交叉编译器。该入口只运行推理，不访问真实机械臂。

## 阅读与协作

- [驱动协作入口与第一阶段产出](docs/驱动移植/README.md)
- [LiteEth 原 Linux C 驱动的 EL0 移植与收发验收](docs/兼容性研究/LiteEth/移植实现与验收.md)
- [LiteEth AI 定向翻译 Rust 驱动实验汇报](docs/兼容性研究/LiteEth/AI定向翻译实验汇报.md)
- [目标侧接口与 UART / DMA 链路](docs/驱动移植/target-interfaces.md)
- [首次会议提纲](docs/驱动移植/meeting.md)
- [现有 Kernel 与 libOS 实现清单](docs/Kernel与LibOS模块实现清单.md)

修改第一方源码时遵守 [AGENTS.md](AGENTS.md) 的中文注释和分层要求。第三方组件保留各自许可证；当前仓库尚无统一的第一方开源许可证，后续应由维护者明确授权方式。不要把可公开访问等同于已授予任意再分发许可。

源码和文档直接维护在 `main` 分支。Fork 默认主线即可开始协作；生成的镜像、ELF 和临时运行日志不纳入版本管理。
