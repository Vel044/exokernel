# ACL Conv/GEMM 算子级验证

## Golden 来源

宿主端权威输入/输出来自 RuTorch 仓库的
`../rutorch/testdata/operator-vectors/`，由固定 PyTorch 2.7.1 CPU 生成器
`../rutorch/python/golden/generate_operator_vectors.py` 写出 little-endian
FP32 数据。该集合包含 `gemm_bias`、`gemm_transpose`、`conv2d_bias`、
`conv2d_stride_dilation` 和 `conv2d_groups`，覆盖 bias、尾块、stride、padding、
dilation 与 groups 属性边界；ACT LibOS provider 只接受 groups=1 的 NHWC/OIHW
窄接口。

重新生成并验证宿主 golden：

```text
cd ../rutorch
.venv/bin/python python/golden/generate_operator_vectors.py --clean
env -i PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/bin:/bin" \
  HOME="$HOME" "$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo" \
  test -p rutorch-backend-reference --test operator_vectors
```

每个元素使用 `abs_error <= 1e-4 + 1e-5*abs(reference)`，shape、元素数量和
非有限值也必须通过。Reference/LibTorch 两个后端读取同一份文件，不能由被测
后端自行生成 expected。

## LibOS 目标验证

`act-runtime::acl_operator_golden_smoke()` 在每次 ACL ACT benchmark 的模型
推理前运行两组无文件、无堆的固定向量：

* `2x3x2` row-major GEMM，`[out,in]` 权重、bias 和非整齐 K/N；
* `3x3x1` NHWC 输入、3x3 OIHW kernel、stride=1、四边 padding 的 Conv。

它们逐元素比较同样的 `atol=1e-4`、`rtol=1e-5`。目标证据是 QEMU 串口中的
`ACL operator golden smoke passed`；case-000 最新日志还同时给出
`ACT steady allocations=0`。

完整 ACT 的五组 case 是更大 shape 的交叉 golden：
`scripts/run_acl_act_correctness.sh` 固定运行 case-000..004，共 3000 个动作
值，并把 max abs/relative error、输出 SHA-256、EL0 退出码写入 target 下的
JSON/CSV/Markdown。任何 case、shape、workspace 或指针对齐错误都必须以非零
状态退出，禁止静默回退 portable。

当前尚未把全部 34 个宿主向量直接打包进 LibOS（这会扩大模型盘和启动时间）；
下一步若要宣称“每个 ACL 算子所有边界均已目标执行”，应新增只读 golden ext4
和 `acl-operator-test` 应用，在同一 C ABI 上逐 case 调用 Conv/GEMM。
