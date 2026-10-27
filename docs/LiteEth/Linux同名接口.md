# Linux 同名 Rust 驱动接口

基准为本地 Linux 6.12.75；原始 C 的代码 token 未修改。Rust 驱动在 [liteeth.rs](../../libos/src/drivers/liteeth.rs)，49 个直接依赖在 [linux_api](../../libos/src/linux_api/mod.rs)。

9 个入口保持原顺序：`rx`、`interrupt`、`open`、`stop`、`start_xmit`、`get_stats64`、`setup_slots`、`probe`、`remove`（均带 `liteeth_` 前缀）。保留显式 `netdev/pdev/irq/dev_id/skb/stats/priv` 参数；`priv` 在 Rust 写作 `r#priv`。

- 39 个普通接口保留参数名、顺序和返回协议；10 个宏保留调用参数角色。C 字符串使用 `*const c_char`，C `long` 对应 ARM64 的 `i64`，C 指针对应 Rust 裸指针。
- `skb/netdev/每 CPU 统计` 改为实际堆分配；恢复 NULL、错误指针、输出参数及明确释放。
- `request_irq` 保存回调与原 `dev_id`，自动创建 EL0 服务线程；`free_irq` 同步在途处理并返回原名称指针。
- 网络核心持有 BUSY 报文，IRQ 恢复队列后调度重试；`netif_rx` 入队即返回，消费 skb 所有权。
- `probe/remove` 由 platform 框架调用；失败和 remove 返回后逆序释放 devm 资源。

应用入口另放在 [liteeth_runtime.rs](../../libos/src/drivers/liteeth_runtime.rs)：`attach/open/send/receive/stats/stop/remove`。这些是宿主框架接口，不替代原驱动入口。

完整定义见 [同名接口映射](同名接口映射.json)，校验脚本见 [check_translation.py](../../tools/liteeth/check_translation.py)。当前未通过的完整 Linux 语义见 [整改结果](逐函数翻译整改结果.md)；同名和测试通过不等于完整 Linux 兼容。
