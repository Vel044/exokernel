# LiteEth AST 解析与规范化比较

现在用实际解析器比较，不再把正则扫描称为 AST。**当前源码仍有差异，不能宣布 AST 相等。**

## 怎么运行

在 exokernel 根目录运行：

```sh
python3 tools/liteeth/check_ast.py
python3 tools/liteeth/test_ast.py
```

第一条检查当前源码：退出码 `0` 表示已覆盖范围相同，`1` 表示有差异，`2` 表示解析失败或遇到未支持语法。当前源码预期返回 `1`。

第二条验证检查器：包含独立正例、排版正例和故意改错的反例。测试脚本只在 `target` 中生成样例，不修改实际驱动。

输入默认是：

- C：`../linux/drivers/net/ethernet/litex/litex_liteeth.c`，读取实际 Linux 头文件。
- Rust：[liteeth.rs](../../libos/src/drivers/liteeth.rs)，以及 `libos/src/linux_api/` 中的实际接口定义。

可用 `--linux`、`--c`、`--rust`、`--output` 指定路径。需要 Clang、Python、`bc`、Rust stable，以及已经生成 ARM64 `include/generated/autoconf.h` 的 Linux 源码。

Rust 解析器依赖锁在 [Cargo.lock](../../tools/liteeth/rust_ast/Cargo.lock)，默认离线构建。新机器缺缓存时先运行：

```sh
cargo +stable fetch --locked --manifest-path tools/liteeth/rust_ast/Cargo.toml
```

本机使用 Clang 22.1.8、`syn` 2.0.119。`bounds.h`、`timeconst.h` 由 Linux 原有生成器产生，放在 `target/liteeth-ast/environment`，不改 Linux 工作区。

## 按翻译约束检查什么

| 约束 | 已实现的检查 |
| --- | --- |
| 入口定义保持 | 9 个函数的名字、参数名/顺序/映射类型、返回类型；局部变量类型 |
| 依赖接口保持 | 调用名字、每一个实参表达式及顺序、返回值使用；39 个普通接口的真实 Rust 签名与已审核映射清单比较，3 个关键类型别名及 10 个宏定义存在性 |
| 有意义分支保持 | 条件运算符、短路表达式、嵌套、分支内操作顺序、返回、错误跳转和清理路径 |
| 分配/布局/生命周期保持 | 当前自动检查 `liteeth` 字段类型、字段顺序与 `repr(C)`；依赖分配器的连续性、清零、失败及回收语义尚未自动证明 |

另外比较回调表的字段与函数绑定、设备匹配表、platform 驱动引用、模块宏实参和元数据值。模块宏这里只比较驱动声明，**不把 Rust 宏说成实现了 Linux 的模块加载和注销机制**。

## 怎样规范化

[Clang 入口](../../tools/liteeth/ast_frontends.py) 导出带类型和源码位置的 C AST。[Rust 入口](../../tools/liteeth/rust_ast/src/main.rs) 用 `syn::parse_file` 导出实际 Rust 语法树。Rust 侧没有冒充 rustc 的类型检查，只对当前支持的表达式子集读取声明并推导类型。

[规范化代码](../../tools/liteeth/ast_normalize.py) 独立读取两棵树，采用以下明确映射：

- `priv` ↔ `r#priv`；`p->field` ↔ `(*p).field`。
- C 整数条件 ↔ Rust `!= 0`；空指针判断 ↔ `is_null()`。
- C 声明后赋值 ↔ Rust `let`；无地址逃逸的同类型标量重新赋值 ↔ 同名 `let`。
- C `goto rx_drop` ↔ Rust 具名块 `break 'rx_drop`，保留跳转和后面的清理操作。
- C `return x` ↔ Rust 尾表达式；地址运算 ↔ `addr_of!`；字节指针加法 ↔ `byte_add`。
- 指定接口的 C `void*` 字节缓冲角色 ↔ Rust `*mut/const u8`；其他指针转换保留。
- 日志 `%d/%u` ↔ `{}`，保留日志正文、参数和顺序；有符号格式的完全等价仍需值域前提。

**不会把 `as` 一律删除。** 整数宽度、截断、乘法发生在转换之前还是之后，都保存在规范化树里。仅常量值不变的转换可以归并。

Linux 宏先保留原调用；宏实参从 Clang 的展开 AST 按源码范围取回，宏的完整展开树也保存。宏内部实现作为未证明的依赖，不按同名直接判为语义相等。

新增语法、未登记导入、函数遮蔽、条件编译属性、`?` 等不能可靠映射的节点会报“未支持”，不会删掉后继续判通过。

## 当前真正发现的差异

| 项目 | Linux C | 当前 Rust | 涉及函数 |
| --- | --- | --- | --- |
| 槽偏移乘法 | 先做 `u32` 乘法，再转换成指针偏移 | 先转 `usize`，再做 64 位乘法 | `liteeth_rx`、`liteeth_start_xmit`、`liteeth_probe` |
| IRQ 字段 | `net_device.irq` 是 `int` | 是 `u32` | `liteeth_open`、`liteeth_stop`、`liteeth_probe` |
| 私有结构体布局 | C 布局 | 没有 `#[repr(C)]` | `struct liteeth` |

结果为 **4 个函数相同、5 个函数不同，另有 1 项布局发现**。39 个普通接口签名符合现有审核清单，回调与匹配表声明的规范化比较相同。

这些差异不等于现有正常收发必然失败。例如槽数很小时乘法不会溢出，已验证非负的 IRQ 也可能有相同行为。但当前检查器没有用这些值域前提消去差异，所以保守报告不同。`repr(Rust)` 也不能保证 C 的字段偏移。

本轮 [结果快照](AST比较摘要.json) 固定源码哈希、解析器版本和测试数量，记录 42 个反例全部检出，以及 11 项驱动、17 项接口宿主回归通过。

## 改错实验

临时正例明确补上 `repr(C)`、使用 C 的 IRQ 字段类型和乘法宽度，**重新解析 Rust 源码后得到 9 个函数相同**；它不是从 C 的规范化树复制出来的，也没有覆盖生产源码。

反例包含：错误寄存器及常量、反转条件、改变短路操作符、交换复制源/目标、错误长度、改变 MMIO 顺序、遗漏 `free_irq`、错误返回、错误 IRQ 上下文/flags、遗漏上交数据包、增加截断 cast、错误默认值、错误回调/匹配表、字段顺序、入口参数名、依赖参数名及类型别名宽度。还检查 C 侧改错、未知节点、隐藏 `cfg`/`?`、同名遮蔽和语法错误。

每个反例都重新解析源码，要求出现对应树差异或明确拒绝。不是因为源文件哈希变了就算检出。反例及断言见 [test_ast.py](../../tools/liteeth/test_ast.py)。

## 结果放在哪里

`target/liteeth-ast/current/` 包含：

- `report.md`：函数结果、差异树路径和可点击源码位置。
- `report.json`：完整发现、映射记录与未证明事项。
- `c.normalized.json`、`rust.normalized.json`：两侧规范化树。
- `c/clang.raw.json`、`c/clang.module.raw.json`、`rust/syn.raw.json`：原始 AST。
- `c/clang.manifest.json`：编译器、目标、参数、Linux 配置、源码和实际头文件哈希。

`target/liteeth-ast/tests/summary.json` 记录改错测试；每个样例目录保留实际源码、AST 和报告。大输出不进入 Git。

**本阶段检查的是已声明映射下的驱动结构。** 依赖函数体、完整 Linux 网络栈、IRQ 并发上下文、编译器配置下的溢出行为、内存分配与释放等，还不能由这份 AST 报告证明。即使临时正例通过，报告中的 `semantic_equivalence_proven` 和 `strict_constraints_fully_satisfied` 仍为 `false`。
