# LiteEth 移植测试工具

- `exo_liteeth.c`：QEMU 10.2.1 的 LiteEth CSR、slot SRAM、NIC backend 与 GIC 中断模型。
- `virt-hook.c.inc`：仅在选择 `-netdev ...,id=liteeth` 时为 ARM virt 创建实验设备及 DTB 节点。
- `build_qemu.py`：给独立 QEMU 源码应用以上两个文件并编译；不安装、不替换系统 QEMU。
- `test_adapter.py` / `test_adapter.c`：原 C 驱动与窄兼容层的 ASan/UBSan 语义测试。
- `../../scripts/test_liteeth.py`：启动真实 ARM64 exokernel，通过外部帧验证 EL0 驱动收发与生命周期。

运行方法及范围见 [移植实现与验收](../../docs/LiteEth/移植实现与验收.md)。

## 原生 Rust 与 C 对照

```sh
python3 tools/liteeth/build_qemu.py --source /path/to/qemu-10.2.1 --build /path/to/qemu-liteeth-build
python3 tools/liteeth/compare.py --qemu /path/to/qemu-liteeth-build/qemu-system-aarch64
```

`compare.py` 共用 `scripts/test_liteeth.py` 和 `apps/liteeth_smoke.rs`，分别选择 `LIBOS_APP=liteeth-smoke`（C）与 `LIBOS_APP=liteeth-rust-smoke`（Rust）。Rust 场景不会编译或链接 Linux C 兼容层。对照包含 17 帧逐字节匹配、统计、slot、寄存器提交顺序、超长 TX 丢弃和资源生命周期；结果保存在 `target/liteeth-comparison/`。

`test_native.py` 独立运行实际 Rust 状态机的 11 项宿主测试。QEMU 模型的 `trace` 属性仅记录设备事务，不改变收发行为。细节与范围见 [AI 定向翻译实验汇报](../../docs/LiteEth/AI定向翻译实验汇报.md)。

## Linux 同名 Rust 接口

`test_linux_api.py` 核查 49 个名称并运行公共层的 13 项复用/所有权测试；`compare.py` 已包含此项。公共实现位于 `libos/src/linux_api/`，启用 feature 为 `linux-driver-api`，不依赖 C 兼容层。[实现位置与支持范围](../../docs/LiteEth/Linux同名接口.md)。

原 C 的 9 个驱动函数在 Rust 中全部保留同名；`test_native.py` 同时核查命名契约并运行原有 11 项设备测试。具体对应见 [原驱动函数表](../../docs/LiteEth/Linux同名接口.md#原驱动函数也保留同名)。

命名保持还覆盖原 20 个寄存器常量、DRV_NAME 和对应结构体/字段；WRITER/READER 不替换成 RX/TX 缩写。`test_native.py` 会验证名称和值，`priv` 在 Rust 中使用原始标识符 `r#priv`。
