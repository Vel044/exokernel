# LiteEth 在 Linux 中如何被找到、注册和调用

![Linux 模块匹配与驱动注册](04-Linux模块匹配与驱动注册.svg)

`MODULE_DEVICE_TABLE` 帮助系统找到要加载的模块；`module_platform_driver` 生成驱动注册和注销入口。驱动注册后，Linux 根据 `.of_match_table` 匹配设备，匹配成功才调用 `liteeth_probe()`。

**先认清图中的几个对象。**

| 名称 | 保存什么／做什么 |
| --- | --- |
| DTB 设备节点 | 描述硬件，包含 `compatible = "litex,liteeth"`、寄存器地址、中断等。 |
| `platform_device` | Linux 为这个硬件设备创建的对象。`probe(pdev)` 的参数就是它。 |
| `liteeth_of_match` | 驱动支持的设备匹配表，其中包含 `"litex,liteeth"`。 |
| `liteeth_driver` | `platform_driver` 结构体，保存匹配表和 `probe/remove` 回调。 |
| `litex_liteeth.ko` | 驱动编成可加载模块时的文件，包含驱动代码、入口和模块别名。 |

**45：`MODULE_DEVICE_TABLE` 让模块构建工具知道支持哪些设备。**

原驱动先定义匹配表，然后用宏导出它：

```c
static const struct of_device_id liteeth_of_match[] = {
    { .compatible = "litex,liteeth" },
    { }
};
MODULE_DEVICE_TABLE(of, liteeth_of_match);
```

这里的 `of` 表示设备树（Open Firmware）匹配表。编译为模块时，宏为表创建一个特殊符号；`modpost` 的 `file2alias` 逻辑识别它，生成模块别名并写入模块信息。安装模块时，`depmod` 根据模块信息建立 `modules.alias` 等索引，具体可见 [kmod 的 output_aliases / depmod_output 实现](https://github.com/kmod-project/kmod/blob/v33/tools/depmod.c#L2054)。

设备出现时，内核发出的设备事件带有 `MODALIAS`。常见的用户态 `udev/kmod` 配置会用这个设备标识查索引，找到 `litex_liteeth.ko`，再请求内核加载。systemd 的 [80-drivers.rules](https://github.com/systemd/systemd/blob/v256/rules.d/80-drivers.rules#L5) 在设备添加且存在 `MODALIAS` 时运行 `kmod load`；[udev-builtin-kmod.c](https://github.com/systemd/systemd/blob/v256/src/udev/udev-builtin-kmod.c#L40) 读取该属性并调用模块加载逻辑。执行查找和加载的是用户态工具；这个宏提供的是它们需要的匹配信息。

**46：`module_platform_driver` 生成加载和卸载时要执行的入口。**

原驱动把匹配表与回调放进 `liteeth_driver`：

```c
static struct platform_driver liteeth_driver = {
    .probe = liteeth_probe,
    .remove_new = liteeth_remove,
    .driver = {
        .name = DRV_NAME,
        .of_match_table = liteeth_of_match,
    },
};
module_platform_driver(liteeth_driver);
```

最后一行通过 `module_driver` 展开，效果相当于下面这段代码：

```c
static int __init liteeth_driver_init(void)
{
    return platform_driver_register(&liteeth_driver);
}
module_init(liteeth_driver_init);

static void __exit liteeth_driver_exit(void)
{
    platform_driver_unregister(&liteeth_driver);
}
module_exit(liteeth_driver_exit);
```

`module_init` 将初始化函数接到模块加载入口，`module_exit` 将清理函数接到模块卸载入口。注册的是整个 platform 驱动；这时还需要找到与它匹配的设备。

**设备先出现时，图中的实际顺序如下。**

1. Linux 解析设备树，为可用的 LiteEth 节点创建 `platform_device`。
2. 内核发布带有 `MODALIAS` 的设备事件。用户态工具利用 45 提供的别名索引找到并加载 `.ko`。
3. 内核执行 46 生成的 `liteeth_driver_init()`，它调用 `platform_driver_register(&liteeth_driver)`。
4. 驱动核心尝试绑定设备。platform 总线的 `platform_match()` 调用 `of_driver_match_device()`，用 `liteeth_driver.driver.of_match_table` 比对设备树中的 `compatible`。
5. 匹配成功后，驱动核心进入 platform 总线的 `platform_probe()`，再通过 `.probe` 函数指针调用 `liteeth_probe(pdev)`。LiteEth 在这里准备资源并注册网卡；打开网卡时才走 `liteeth_open()`。
6. 模块允许且成功卸载时，内核执行 `liteeth_driver_exit()`。它注销 platform 驱动，解绑已绑定设备并调用 `liteeth_remove(pdev)`；LiteEth 注销网卡，驱动核心随后清理设备托管资源。

图画的是常见自动加载路径。模块也可以手动加载；如果驱动先注册、设备后出现，设备出现后同样会进行匹配和 `probe`。一次匹配也可能因资源尚未就绪而推迟，或因 `probe` 返回错误而失败。

**同一张匹配表被两处使用。**

| 使用位置 | 用途 |
| --- | --- |
| `MODULE_DEVICE_TABLE(of, liteeth_of_match)` | 编译出模块别名，帮助用户态找到应加载的 `.ko`。 |
| `.of_match_table = liteeth_of_match` | 驱动注册后，供 Linux 内核实际匹配设备。 |

因此，删掉 `MODULE_DEVICE_TABLE` 会影响依赖这些别名的模块自动加载；手动加载后，只要 `.of_match_table` 和注册流程仍在，设备仍能匹配。只保留 `MODULE_DEVICE_TABLE` 而没有注册驱动，也不会执行 `probe`。

**如果驱动直接编进内核。**

`CONFIG_LITEX_LITEETH=y` 时没有单独加载 `.ko` 的步骤，`MODULE_DEVICE_TABLE` 展开为空。`module_platform_driver` 生成的初始化函数通过内核启动初始化流程执行，仍会注册 platform 驱动并使用 `.of_match_table` 匹配设备。内置驱动的退出入口不作为可卸载模块入口执行。

**与你现在的 Rust LiteEth 路径对照。**

当前 Rust 验收应用从 `DeviceInfo` 取得 `LiteEthInfo`，直接调用 `Driver::liteeth_probe(resource)`。这里没有 Linux 的模块自动加载、platform 驱动注册和总线自动绑定流程。`linux_api/module.rs` 中虽然保留了同名封装，LiteEth 当前路径没有调用它们。

**源码核对位置（路径相对于本地 Linux 源码根目录）。**

| 位置 | 对应内容 |
| --- | --- |
| `drivers/net/ethernet/litex/litex_liteeth.c:315` | 匹配表、两个宏、`probe/remove` 和 `.of_match_table`。 |
| `drivers/net/ethernet/litex/Makefile:5` | `CONFIG_LITEX_LITEETH` 对应 `litex_liteeth.o`。 |
| `include/linux/module.h:249` | `MODULE_DEVICE_TABLE` 在模块／内置构建中的展开。 |
| `scripts/mod/file2alias.c:356` | 根据 OF 匹配表生成模块别名。 |
| `drivers/of/platform.c:161` | 从设备树节点创建并注册 platform_device。 |
| `drivers/of/device.c:257` | 设备事件中生成 `MODALIAS`。 |
| `include/linux/platform_device.h:301` | `module_platform_driver` 转给 `module_driver`。 |
| `include/linux/device/driver.h:257` | 生成注册、注销函数并接入 `module_init/module_exit`。 |
| `drivers/base/platform.c:861` | 注册 platform 驱动并进入通用 `driver_register`。 |
| `drivers/base/platform.c:1335` | platform 总线的设备／驱动匹配。 |
| `include/linux/of_device.h:20` | 根据驱动的 `.of_match_table` 匹配设备。 |
| `drivers/base/platform.c:1379` | platform 总线最终调用驱动 `.probe`。 |

用户态 `depmod`、`udev/kmod` 属于 Linux 用户空间工具，实现在 Linux 内核源码树之外。图中只表示它们如何使用模块别名与设备事件。
