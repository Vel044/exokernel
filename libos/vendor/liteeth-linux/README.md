# Linux LiteEth 源码

`litex_liteeth.c` 原样取自 Linux 6.12.75 的
`drivers/net/ethernet/litex/litex_liteeth.c`，保留原版权和 GPL-2.0-or-later 声明。
第一方兼容实现位于 `libos/src/drivers/linux_liteeth/`，不得在这里改写驱动逻辑。

固定文件 SHA-256：`bb8b3c2cac2f8821647c66cf00a96adf7eed02b0c6c9acc925d37eb7b628285e`。
`tools/liteeth/test_adapter.py` 会校验该值，防止通过改写源驱动绕过适配问题。
