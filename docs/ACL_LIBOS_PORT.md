# ACL CPU/NEON 到 EL0 LibOS 的移植记录

## 当前结论

ACL v52.7.0 的完整 runtime 可以用 AArch64 工具链生成静态 archive，但不能
直接链接进本项目的 EL0 LibOS。对离线快照的实际审计结果为：archive 约 16.7 MiB，
仍有 172 个跨 object 的外部符号，其中包含 `pthread_*`、`std::thread`、
`std::condition_variable`、`mmap/munmap`、`fopen/fclose`、`getauxval`、
`sched_*`、`sysconf` 和正则表达式运行时。这些依赖违反“不移植 glibc、动态加载器、
libgomp 或 Linux syscall 兼容层”的约束，因此本项目不把完整 ACL runtime 放进
LibOS。

审计入口是 [`scripts/build_acl_freestanding.sh`](../scripts/build_acl_freestanding.sh)。
它校验 ACL archive SHA-256 为
`602d6ffa7b7f6d1445c36eace63b17660e7aeb144a2347b7a882917680bdbdd5`，并保留
manifest、源码树 hash、ELF section、raw/external undefined symbol 和禁用符号报告。
构建即使发现禁用依赖也保留证据，并以状态码 2 结束，防止把 Linux candidate
误报为 bare-metal 成果。

## 采用的窄边界

`act-kernels-acl` 是 `#![no_std]` Rust provider。它不暴露 ACL C++ 类型，只接受
显式 shape、FP32 指针和调用者提供的 workspace。`native/acl_shim.cpp` 只引入
ACL 的 Cortex-A76 `a64_sgemm_asimd_8x12` NEON 微内核，自己完成：

1. row-major GEMM 的 A/B panel 打包和 8x12 C panel 解包；
2. NHWC 输入、OIHW 权重的 Conv2d im2col 逻辑；
3. bias、边界 padding、stride/dilation 校验；
4. persistent（预打包句柄）和 temporary（每次可复位）workspace 查询。

微内核本身不调用 `new/delete`、pthread、文件系统、CPU 探测或 syscall。Cargo
启用 `acl-freestanding` 时，`build.rs` 只编译 ACL 对应的 NEON kernel 源文件和
第一方 shim，生成 `libact_acl_microkernel.a`；缺少 AArch64 编译器或 ACL 源码时
构建直接失败，不会静默回退 portable。

## ACT 接线

算子 golden、目标 smoke 和后续扩展规则见
[`ACL_OPERATOR_GOLDEN.md`](ACL_OPERATOR_GOLDEN.md)。

`act-runtime` 的 `KernelProvider::AclNeon` 会把 ACT ResNet 的 Conv2d 和 Transformer
线性层送进上述微内核；LayerNorm、Softmax、Attention 和其他非热点算子继续使用
已有 Rust 实现。`KernelProvider::PortableNeon` 保持旧接口和结果不变。ACL raw
入口按 8 行输出块拆成最多 4 个 job，每个 job 使用独立的 A/C panel scratch，
通过 `rutorch_parallel_jobs` 接入 LibOS 的三个 worker 和调用线程；小于两个 job
时保持当前 CPU 串行，避免无效同步。

LibOS 通过 `ACT_KERNEL_PROVIDER=portable|acl` 选择构建路径。ACL 路径固定使用
四核 `ActParallelPool`；job 调度只在 EL0 共享原子屏障内同步完成，不引入 OpenMP、
libgomp、pthread 或额外 SVC。ACL provider 选择成功但静态 bridge 缺失时，`ActModel` 返回
`AclProviderUnavailable`，禁止降级到 portable。

## 已完成的可验证项

- AArch64 `a64_sgemm_asimd_8x12` 源文件和第一方 shim 可在无 C++ 标准库对象的
  条件下编译；archive 的 `nm -u` 只剩该 ACL kernel 的内部链接符号。
- `act-kernels-acl` 和 `act-runtime` 的 `aarch64-unknown-none` 检查通过。
- LibOS ACL release ELF 已链接微内核，`readelf` 显示无 dynamic section、无
  `PT_INTERP`、无 `DT_NEEDED`；ELF 中能看到 `rutorch_acl_*` 和 ACL 8x12 kernel
  符号。可用 `scripts/audit_libos_elf.sh` 自动复核；当前构建输出
  `ACL_LIBOS_ELF_PASS=true`、未解析符号 0、禁止符号 0，ELF SHA-256 为
  `6c3510d7b2f2b43afe4edfbc9aad20ee83c3f53b9886f56869a4700cdf722168`（278224
  bytes）。
- ACL 完整 ACT 已在 QEMU HVF、`cpu=host`、4 vCPU、4 GiB 下运行五组样本；
  最新 `scripts/run_acl_act_correctness.sh` 生成的汇总为
  `target/acl-correctness-acl.FqXws5/SUMMARY.md`，`case-000..004` 共 3000 个值
  全部通过 `atol=1e-4, rtol=1e-5`。各组最大绝对误差为
  `8.39e-5/1.14e-4/1.83e-4/1.83e-4/7.63e-5`，每组均输出 600 个 IEEE-754
  位模式并以 `EL0 exit code=0` 结束。逐组摘要和原始串口日志均位于 `target/`
  （被 git 忽略，不会把大结果提交进仓库）。
- 每次 ACL `act-benchmark` 在正式 ACT 前先执行 `acl_operator_golden_smoke()`：
  非整齐 `2x3x2` GEMM（含 bias）和带 padding 的 `3x3` Conv 均逐元素通过
  `atol=1e-4, rtol=1e-5`；case-000 日志中的
  `ACL operator golden smoke passed` 是该检查的证据。更大 shape 的 Conv/GEMM
  由下面五组完整 ACT golden 覆盖。
- 本次冻结的输入为模型 SHA-256
  `bf84b9539455980c6f0c5f1533e7a11794ae63dfbe006d1f9e233291c45df671`、normalizer
  SHA-256 `886e06102c117054b2cfc7a2c8a8c4a87a6eca40462431437cc8d188f44e00ad`；
  case-000 的 handeye/fixed/state SHA-256 分别为
  `cf55e797960deb763d1068843884b384734cfcf48dd1a283f6636f0557b27659`、
  `b38a8c5be9be2f9deb4b42546db35576c7812f6e6065652ab11284ace1b22054`、
  `6930fe30cf8f38c5236b3de436aa8b8cf0391c374ffe768ddf2496c3f0fa8d98`。
- 新版 `act-benchmark` 已固定执行 5 次 warm-up、10 次 steady-state，并输出
  median/P95。接入四 lane job 调度后的 case-000 三次独立 HVF 启动结果为：
  ACL `1672/2041/2217 ms` median、`1751/2174/2382 ms` P95；portable 对照为
  `7676/7171/7199 ms` median、`8126/7275/7359 ms` P95。三次运行均为
  600/600 正确，ACL 端到端中位数约为 portable 的 0.26 倍（约 3.5–4.6 倍
  加速，启动间差异来自 HVF 宿主负载），不是只取最快一次的宣传数字；对应
  JSON/CSV/Markdown 在 `target/acl-benchmark-acl-000.AZUq0T/` 和
  `target/acl-benchmark-portable-000.GqfVKf/`。
- 可重复的三次启动入口是 `scripts/run_acl_act_benchmark.sh`：例如
  `ACT_KERNEL_PROVIDER=acl ACT_CASE_ID=000 ACT_BENCHMARK_REPLICATES=3 QEMU_ACCEL=hvf
  ./scripts/run_acl_act_benchmark.sh`。结果目录会保留每次串口日志、JSON、CSV
  和 Markdown；最新 ACL 三次输出 SHA 完全相同为
  `e0857a56615d910048adc0d95067b37369756d08a52b698ac90ed0e46c7f6dc5`，portable
  三次输出 SHA 完全相同为
  `9ac243929f5503ed08fef5cf9bdc36512468d0e2f11f308bdd99a3fcc2c7d686`。
  新增的 `ACT model-only complete` 行会扣除 HWC 图像和 state 归一化，只统计
  Conv/Transformer/action head；当前一次有效 10 次样本的 ACL model-only 为
  median `1450 ms`、P95 `1527 ms`，对应日志和 JSON 在
  `target/acl-benchmark-acl-000.pnM0Da/`。与 PyTorch 最佳 TorchScript 的
  model-core median `466.544 ms`、P95 `520.374 ms` 相比，当前 ACL 模型核心约慢
  `3.1x`；这是当前待优化的真实差距，不再把端到端/模型时间混为一谈。
  当前源码的连续稳定性复跑位于 `target/acl-benchmark-acl-000.w4T1Fp/`：100 次
  推理通过、600/600 正确、steady-state `GlobalAlloc=0`，median 1633 ms、P95
  1695 ms，输出 SHA 仍为
  `e0857a56615d910048adc0d95067b37369756d08a52b698ac90ed0e46c7f6dc5`。
  PyTorch 最佳正确配置现有记录为 TorchScript 4 线程，model-core
  `median=466.544 ms, p95=520.374 ms`；LibOS 当前计时包含输入预处理和整个
  用户态前向，不能直接当作同一计时边界。因此 `acl_median <= best_correct_pytorch`
  的最终门槛仍待统一计时后冻结。

## 仍未冻结的门槛

1. 还需把 GEMM/Conv 的独立 golden 扩展到所有尾块、padding、workspace 不足和未
   对齐指针组合；当前已有小型 operator smoke、API 边界测试和完整 ACT 差分。
2. case-000 已使用 `ACT_CONTINUOUS_CHECK=1` 完成额外 100 次连续推理，输出
   `ACT continuous check passed runs=100`，结果 SHA 未漂移；同一日志还输出
   `ACT steady allocations=0`。该计数器覆盖 GlobalAlloc 调用，证明 steady-state
   没有隐含 Rust 堆分配；若要把统计细化到 C++ shim，可再增加独立的 arena hook。
3. PyTorch 的 `466.544 ms` 是 model-core 计时，LibOS 当前日志是端到端计时，
   两者边界尚未统一。因此暂不宣称 ACL 已达到 `acl_median <= best_correct_pytorch`
   门槛；需要下一步给 LibOS 增加与 PyTorch 相同的阶段计时后再判定。

第三方 ACL 源码保持原样，不在 `vendor/` 或源码快照内添加项目注释；本文件解释
第一方 shim 的所有权、生命周期和 EL0/EL1 边界。
