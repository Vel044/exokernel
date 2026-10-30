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

`test_linux_api.py` 核查 49 个名称并运行公共层的复用、所有权及初始状态测试；`compare.py` 从实际日志读取通过数量，不使用固定计数。公共实现位于 `libos/src/linux_api/`，启用 feature 为 `linux-driver-api`，不依赖 C 兼容层。[实现位置与支持范围](../../docs/LiteEth/Linux同名接口.md)。

原 C 的 9 个驱动函数在 Rust 中全部保留同名；`test_native.py` 同时核查命名契约并运行原有 11 项设备测试。具体对应见 [原驱动函数表](../../docs/LiteEth/Linux同名接口.md#原驱动函数也保留同名)。

命名保持还覆盖原 20 个寄存器常量、DRV_NAME 和对应结构体/字段；WRITER/READER 不替换成 RX/TX 缩写。`test_native.py` 会验证名称和值，`priv` 在 Rust 中使用原始标识符 `r#priv`。


## 逐函数约束检查

`python3 tools/liteeth/check_translation.py` 从本地 Linux 源码核对 9 个显式入口签名、20 个 if 条件/嵌套和依赖调用顺序；`linux_signatures.json` 固定经 Linux 定义核对的 39 个普通接口参数与返回映射，其余 10 个保留同名宏。C `priv` 映射为 Rust `r#priv`，整数条件映射为 `!= 0`，C goto 清理映射为带标签块。

`source_contract.py` 核对完整代码 token 哈希，允许已有中文注释与排版差异；不会忽略标识符、操作符和字符串。源码或冻结签名改变须重新人工核查。以上检查不是依赖语义等价证明，未完成项见 `docs/LiteEth/逐函数翻译整改结果.md`。
