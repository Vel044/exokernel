# Linux 同名 Rust 驱动接口

当前按用户要求保留 LiteEth 的 49 个外部接口名称，公共实现位于 `libos/src/linux_api/`。驱动不再把这些行为直接散写在设备代码中。

这是 Rust 源码迁移接口：名字保留，参数使用有生命周期的引用、Result、拥有值和显式后端。原 C 文件不能直接调用它；原 C 对照继续使用 `drivers/linux_liteeth/`，不修改 vendor 原文件。

## 原驱动函数也保留同名

不仅外部依赖保留同名，原 `litex_liteeth.c` 的 9 个函数在 Rust 中也保持原名。9 个函数集中在 `liteeth.rs`，按原 C 的顺序排列。下面链接指向实际逻辑，文件后面的 Driver 发送/统计入口只做应用转发。

| 原 C 函数 | Rust 同名实现 |
| --- | --- |
| `liteeth_rx` | [liteeth::liteeth_rx](../../libos/src/drivers/liteeth.rs#L88) |
| `liteeth_interrupt` | [liteeth::liteeth_interrupt](../../libos/src/drivers/liteeth.rs#L119) |
| `liteeth_open` | [Driver::liteeth_open](../../libos/src/drivers/liteeth.rs#L140) |
| `liteeth_stop` | [Driver::liteeth_stop](../../libos/src/drivers/liteeth.rs#L160) |
| `liteeth_start_xmit` | [liteeth::liteeth_start_xmit](../../libos/src/drivers/liteeth.rs#L175) |
| `liteeth_get_stats64` | [liteeth::liteeth_get_stats64](../../libos/src/drivers/liteeth.rs#L205) |
| `liteeth_setup_slots` | [liteeth_setup_slots](../../libos/src/drivers/liteeth.rs#L215) |
| `liteeth_probe` | [Driver::liteeth_probe](../../libos/src/drivers/liteeth.rs#L231) |
| `liteeth_remove` | [Driver::liteeth_remove](../../libos/src/drivers/liteeth.rs#L319) |

`probe` 中按原 C 的顺序展开创建 netdev、初始化统计、取 IRQ、映射、设置槽位及注册。临时 `pdev.drvdata` 保存网卡对象，随后转移到 Driver；统计仍为单所属线程计数。MAC 地址读取与随机回退仍未接入，当前实验透传完整 L2 帧。

`wait_irq`、`receive` 是目标侧新增的通知等待/上层取帧接口；`prepare_open`、`enable`、`stop_device_events` 是分层接入时的辅助步骤。它们不替代上述原函数名。当前原 C 对照的 Rust 桥也使用同名应用入口，但 C 文件和 C FFI 符号不变。

Rust `liteeth_start_xmit` 按值接收 `sk_buff`：队列忙时以 `Busy(skb)` 归还报文，发送成功或超长丢弃时驱动调用 `dev_kfree_skb_any` 消费报文，对应 Linux `NETDEV_TX_BUSY` / `NETDEV_TX_OK` 的所有权规则。应用测试入口负责构造待发送 skb。

## 目录与使用

- `io.rs`：LiteX CSR、设备缓冲复制，以及受检查的 volatile/屏障实现。
- `netdevice.rs`、`skbuff.rs`：网卡私有数据、队列、链路、统计与有界线性报文。
- `platform.rs`：命名资源、IRQ 索引、属性及 drvdata，没有 LiteEth 固定资源表。
- `interrupt.rs`：request_irq/free_irq 保存回调函数并串行派发，不保存自引用裸指针。
- `module.rs`：保留元信息构造，显式进行 OF compatible 匹配及 probe/remove。
- `error.rs`、`printk.rs`：错误、日志、限流。
- `exo.rs`：上述接口接现有 MMIO、IRQ、Notification syscall 的唯一后端。

新的 Rust 驱动启用 `linux-driver-api` feature，导入对应模块，提供设备自己的资源描述和状态；不需要依赖 LiteEth 场景。公共封装测试使用 `vendor,a`、`vendor,b` 和 `vendor,other` 设备描述，验证不同实例不会共享队列/统计/回调状态；这不是已经移植第二个真实驱动的证明。

## LiteEth 当前调用链

```text
LiteEth 设备逻辑
  litex_read8 / litex_write8 / memcpy_toio / memcpy_fromio
    → Mmio 的同名方法选择寄存器区或 SRAM → 同名封装 → IoRegion volatile + barrier
  skb_put / netif_rx / netif_*queue / dev_sw_netstats_*
    → 公共线性 skb、队列、链路、统计（全部 EL0）
  devm_platform_ioremap_resource_byname
    → Mapping::map → MAP_MMIO，Mapping::drop → UNMAP_MMIO
  request_irq(registration, intid, liteeth::liteeth_interrupt, 0, "liteeth")
    → 保存 EL0 函数指针 → ExoIrq → IRQ_BIND
  wait_and_dispatch(&mut device)
    → NOTIFICATION_WAIT → liteeth_interrupt(device) → IRQ_ACK
  free_irq → IRQ_UNBIND → 清除回调；通知最后 DESTROY
```

IRQ 上下文由 `&mut device` 在派发时传入，而不是保存 `void* dev_id`。注册层保存函数指针；内核不接收用户态函数地址。线程亲和对象禁止 Send/Sync；本实现没有自动启动后台 IRQ 线程，现有回显应用仍驱动 wait_irq 循环。

## 与完整 Linux 的边界

1. 单所属线程；软件统计的同名接口使用单线程计数，不是 Linux per-CPU/softirq/RCU。完整并发语义没有实现。
2. `sk_buff<N>` 按值拥有有界内联存储，`netdev_alloc_skb_ip_align` 不做堆分配。`skb_put` 不得超过请求容量；交付保留完整 L2 帧，容量及队列深度由调用者指定。
3. `eth_type_trans` 仅实现 Ethernet II/LLC/原始 802.3 协议判断和头部视图移动；不保存完整 Linux 的 dev/pkt_type/DSA 元数据，也不启动 IP 栈。
4. `register_netdev` 保存本地对象注册状态；`unregister_netdev` 要求调用者已 stop/free_irq，不会自动执行 Linux 网络核心的全部注销流程。模块元信息有实际数据，OF 绑定有匹配/probe/remove，不是空成功桩。
5. `platform_get_irq` 接受后端约定的 IRQ 编号。Exokernel 后端要求已授权的 GIC INTID；不能把其他 Linux IRQ domain 中的逻辑号直接复制过来。
6. `request_irq` 当前只接受 flags=0，其他标志返回 -95；回调不在硬中断上下文运行。free 失败保留绑定以便重试，成功后禁止继续派发。
7. 缺失资源、属性、非法 MAC 或 skb 容量显式返回错误。随机 MAC 需要调用者传入随机字节，不以固定地址/计数器冒充随机源。错误指针改成 Result，模块宏改成同名 Rust 构造函数，因此参数/语法不承诺 C 二进制兼容。

## 49 个名称的位置

包含暂未被当前 LiteEth 路径使用、但为后续移植保留的接口，例如日志、私有数据、元信息及 MAC 策略。保留名字不等于已经完整实现 Linux 的全部语义，以下每项按所属模块的支持边界使用。

| 原 Linux 名称 | 同名 Rust 实现 |
| --- | --- |
| `IS_ERR` | [error.rs](../../libos/src/linux_api/error.rs#L7) |
| `MODULE_AUTHOR` | [module.rs](../../libos/src/linux_api/module.rs#L9) |
| `MODULE_DESCRIPTION` | [module.rs](../../libos/src/linux_api/module.rs#L15) |
| `MODULE_DEVICE_TABLE` | [module.rs](../../libos/src/linux_api/module.rs#L32) |
| `MODULE_LICENSE` | [module.rs](../../libos/src/linux_api/module.rs#L21) |
| `PTR_ERR` | [error.rs](../../libos/src/linux_api/error.rs#L10) |
| `SET_NETDEV_DEV` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L61) |
| `dev_dbg` | [printk.rs](../../libos/src/linux_api/printk.rs#L7) |
| `dev_err` | [printk.rs](../../libos/src/linux_api/printk.rs#L10) |
| `dev_fetch_sw_netstats` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L112) |
| `dev_kfree_skb_any` | [skbuff.rs](../../libos/src/linux_api/skbuff.rs#L108) |
| `dev_sw_netstats_rx_add` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L101) |
| `dev_sw_netstats_tx_add` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L105) |
| `devm_alloc_etherdev` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L41) |
| `devm_netdev_alloc_pcpu_stats` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L55) |
| `devm_platform_ioremap_resource_byname` | [platform.rs](../../libos/src/linux_api/platform.rs#L30) |
| `eth_hw_addr_random` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L119) |
| `eth_type_trans` | [skbuff.rs](../../libos/src/linux_api/skbuff.rs#L60) |
| `free_irq` | [interrupt.rs](../../libos/src/linux_api/interrupt.rs#L59) |
| `litex_read32` | [io.rs](../../libos/src/linux_api/io.rs#L15) |
| `litex_read8` | [io.rs](../../libos/src/linux_api/io.rs#L12) |
| `litex_write16` | [io.rs](../../libos/src/linux_api/io.rs#L21) |
| `litex_write8` | [io.rs](../../libos/src/linux_api/io.rs#L18) |
| `memcpy_fromio` | [io.rs](../../libos/src/linux_api/io.rs#L27) |
| `memcpy_toio` | [io.rs](../../libos/src/linux_api/io.rs#L30) |
| `module_platform_driver` | [module.rs](../../libos/src/linux_api/module.rs#L64) |
| `net_ratelimit` | [printk.rs](../../libos/src/linux_api/printk.rs#L37) |
| `netdev_alloc_skb_ip_align` | [skbuff.rs](../../libos/src/linux_api/skbuff.rs#L32) |
| `netdev_err` | [printk.rs](../../libos/src/linux_api/printk.rs#L13) |
| `netdev_info` | [printk.rs](../../libos/src/linux_api/printk.rs#L16) |
| `netdev_priv` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L58) |
| `netdev_stats_to_stats64` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L109) |
| `netif_carrier_off` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L98) |
| `netif_carrier_on` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L95) |
| `netif_queue_stopped` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L83) |
| `netif_rx` | [skbuff.rs](../../libos/src/linux_api/skbuff.rs#L117) |
| `netif_start_queue` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L86) |
| `netif_stop_queue` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L89) |
| `netif_wake_queue` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L92) |
| `of_get_ethdev_address` | [platform.rs](../../libos/src/linux_api/platform.rs#L53) |
| `of_property_read_u32` | [platform.rs](../../libos/src/linux_api/platform.rs#L47) |
| `platform_get_drvdata` | [platform.rs](../../libos/src/linux_api/platform.rs#L44) |
| `platform_get_irq` | [platform.rs](../../libos/src/linux_api/platform.rs#L37) |
| `platform_set_drvdata` | [platform.rs](../../libos/src/linux_api/platform.rs#L41) |
| `register_netdev` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L65) |
| `request_irq` | [interrupt.rs](../../libos/src/linux_api/interrupt.rs#L41) |
| `skb_put` | [skbuff.rs](../../libos/src/linux_api/skbuff.rs#L50) |
| `unlikely` | [error.rs](../../libos/src/linux_api/error.rs#L17) |
| `unregister_netdev` | [netdevice.rs](../../libos/src/linux_api/netdevice.rs#L73) |

## 验证

```sh
python3 tools/liteeth/test_linux_api.py
python3 tools/liteeth/compare.py --qemu /path/to/qemu-system-aarch64
```

第一项核查 49 个名称并运行 13 项独立公共接口测试，覆盖双设备隔离、skb 容量与 IP 对齐、协议识别、队列满/重试、IRQ 回调/ACK 顺序、绑定/解绑失败、映射拥有者回收、OF 匹配及退出。第二项继续运行原 C ASan/UBSan、原 11 项 Rust 状态机与 ARM64 QEMU 两版本对照。公共模块没有新增 kernel syscall，也没有引入 DMA。

最新本机验收：`target/liteeth-comparison/20261025-091153/`。9 个原驱动函数及 49 项外部接口名称核查、13 项公共接口测试、11 项设备状态机测试、C ASan/UBSan 及 ARM64 QEMU C/Rust 对照全部通过。两版 17 帧共 10,752 字节逐字节一致，统计/slot/寄存器提交及生命周期一致；Rust 无 C FFI 符号。此记录不代表真实硬件、完整网络栈或所有异常的形式化等价。

需要逐项确认和填写意见时使用 [接口语义确认表](接口语义确认表.md)，其中“你的备注”列保持空白。

## 原标识符保持规则

寄存器的 WRITER/READER 是硬件原名：WRITER 对应接收、READER 对应发送，翻译时不改成 RX/TX 别名。原文件的 20 个 LITEETH 寄存器常量及 DRV_NAME 均保留原名原值，包括当前路径未使用的定义。

原私有类型保留为 `struct liteeth`，对应字段使用 `base`、`netdev`、`slot_size`、`num_rx_slots`、`num_tx_slots`、`rx_slot`、`tx_slot`、`rx_base`、`tx_base`。Linux 对应类型使用 `net_device`、`sk_buff`、`rtnl_link_stats64`、`pcpu_sw_netstats`、`platform_device`、`platform_driver`，不再为了 Rust 命名风格改成另一套名称。

C 的 `priv` 是 Rust 保留字，因此写作 `r#priv`，逻辑名称仍为 priv。`Layout`、`Driver`、`Mapping`、`IrqRegistration` 等是目标侧新增的配置/资源拥有者，不是给原 C 类型改名。RX/TX 基址在当前 Io 后端中用设备 SRAM 窗口内偏移表示，不冒充 PA 或 DMA 地址。结构与 C ABI 不承诺相同。

`test_native.py` 同时检查原函数、寄存器名称/偏移、DRV_NAME 和对应类型名，防止后续再次擅自缩写。
