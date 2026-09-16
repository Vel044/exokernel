# 实验 03：ACT 推理速度横向比较

## 1. 实验目标

在本 Exokernel、seL4 和 Linux 上运行完全相同的ACT模型、Rust推理引擎和输入，
比较用户态推理速度及其抖动：

```text
固定模型：Vel044/so101_act_bottle
固定数据：Vel044/so101_bottle
chunk size：100
```

```text
同一model.safetensors
同一normalizer.safetensors
同一act-runtime源码
同一两路RGB图像
同一组六维舵机状态
→ 输出同一100 × 6动作序列
```

实验01比较低层Kernel原语，只适合Exokernel与seL4；实验02先验证真实输入下
PyTorch参考实现与`act-runtime`的完整输出一致；本实验只对已经通过实验02的实现
比较完整用户态计算性能，因此Linux可以与二者横向比较。

另外加入一条独立参考路径：

```text
QEMU Linux
→ 本仓库原生LeRobot ACTPolicy
→ PyTorch完整前向
```

这条路径用于再次生成PyTorch真值、检查Rust后端的数值正确性，并展示成熟框架的
参考耗时。它使用了不同的推理后端，不能参与Kernel性能比值，也不能据此判断
Exokernel、seL4或Linux哪个Kernel更快。

## 2. 研究问题

1. 模型已加载后，三套系统执行一次ACT前向分别需要多少时间和CPU周期？
2. OS调度、中断和后台服务对p95、p99和最大推理延迟有多大影响？
3. Exokernel和seL4的低层资源模型能否在不牺牲隔离的情况下接近Linux裸计算性能？
4. 推理时间是否满足机器人控制周期；不满足时瓶颈位于OS还是`act-runtime`算子？

## 3. 公平性原则

主实验禁止使用不同推理后端：

| 系统 | 推理程序 | 模型解析 | 数值算子 |
| --- | --- | --- | --- |
| Exokernel | `no_std` EL0应用 | `act-runtime` | `act-runtime` |
| seL4 | `no_std`用户任务 | `act-runtime` | `act-runtime` |
| Linux | Rust用户进程 | `act-runtime` | `act-runtime` |

Linux上的PyTorch只作为“成熟框架参考结果”，单独列图，不能与上述三者一起计算
OS加速比。否则测到的是PyTorch/BLAS与自研Rust算子的差异，不是Kernel差异。

因此实验包含两组结论，二者不能混用：

```text
Kernel环境比较：Exokernel Rust / seL4 Rust / Linux Rust
正确性与框架参考：QEMU Linux PyTorch
```

QEMU Linux PyTorch必须读取实验02冻结的相同输入，并输出完整`100 x 6`动作CSV。
该输出与Rust结果逐元素比较；它的`duration_ms`可以作为PyTorch运行记录，但不进入
`Exokernel/Linux`或`seL4/Linux`等OS比值。

三套系统必须使用同一个`act-runtime` commit和相同编译优化。任何平台专用NEON
实现必须同时提供给三套系统。

## 4. 固定输入与正确性

正式输入、PyTorch参考输出和误差阈值由实验02生成并冻结。本实验不得重新选择
数据帧或重新生成参考值，以免在速度实验中悄悄改变正确性基线。

使用两组输入：

### 4.1 确定性输入

沿用当前smoke中的两张`640 × 360 RGB HWC`确定性图像和舵机状态：

```text
[0.0, -30.0, 50.0, 10.0, -1.0, 25.0]
```

它用于跨系统逐数值回归，排除图片解码、相机和文件系统差异。

### 4.2 真实样本输入

从同一机器人数据集选择固定的两路相机帧与关节状态，转换成未压缩RGB文件。
三套系统读取完全相同的字节，不在计时区间内执行JPEG解码。

每次推理输出`100 × 6`动作。以Linux PyTorch eval输出为数值参考，并要求：

```text
所有元素有限
输出shape完全一致
abs(actual - reference) <= 1e-8 + 1e-5 * abs(reference)
```

输出不满足正确性要求的样本不计入性能结果，实验应直接失败。

## 5. 模型与文件

固定记录两个文件的SHA-256：

```text
/model.safetensors
/policy_preprocessor_step_3_normalizer_processor.safetensors
```

三套系统使用同一份文件内容：

- Exokernel：只读ext4 → virtio-blk/后续Pi5块设备 → EL0读取。
- seL4：同一镜像或启动归档 → 用户态文件/块设备服务读取。
- Linux：普通只读文件系统读取。

文件传输方式不同，所以“模型加载”与“纯前向”必须分开报告。

## 6. 测试模式

### 6.1 M1：模型加载

```text
打开两个safetensors文件
→ 读取全部字节
→ 解析tensor元数据
→ 校验模型签名
→ 分配并初始化workspace
```

报告毫秒和读取吞吐量。M1反映文件系统、块设备和内存分配，不用于判断算子速度。

### 6.2 M2：单次冷前向

模型和workspace已经准备好，但权重/工作集尚未被本轮推理预热。运行一次完整ACT
前向并报告各阶段时间：

```text
输入准备
两路图像归一化
共享ResNet18
特征投影
Transformer Encoder
Transformer Decoder
Action Head
反归一化
```

### 6.3 M3：稳定态前向

先执行3次不计时warm-up，再连续执行100次相同前向。模型保持只读，workspace
重复使用，计时区间内不进行文件I/O、内存分配或日志输出。

这是三系统横向比较的主结果。

### 6.4 M4：受干扰前向

用于观察机器人组合负载下的尾延迟：

```text
CPU0：协调线程
CPU1：控制线程周期唤醒
CPU2：USB事件负载
CPU3：ACT推理线程
```

三个系统使用相同CPU亲和性和事件频率。M4与M3分开报告，不把后台负载差异
藏在一个平均数里。

### 6.5 M5：四核并行（后续）

当前`act-runtime`是单线程实现，因此M3的正式第一阶段只比较单线程。只有当
Conv2d、Linear和Attention实现相同的四核并行后，才增加M5：

```text
1核固定CPU3
4核固定CPU0..3
speedup = single_core_time / four_core_time
parallel_efficiency = speedup / 4
```

不能让Linux使用四核BLAS而Exokernel/seL4仍使用单线程，再把结果归因于OS。

## 7. 计时与周期

每次推理同时记录：

```text
PMCCNTR_EL0：CPU cycles
CNTVCT_EL0：固定频率wall time ticks
```

- Exokernel通过benchmark-only PMU权限读取cycle counter。
- seL4使用`libsel4bench`读取cycle counter。
- Linux使用`perf_event_open`记录用户态和Kernel cycle，并用`clock_gettime`
  记录wall time。

计时前后使用compiler fence和`ISB`，避免编译器或乱序执行越过边界。阶段日志只
写入预分配记录数组，所有串口/终端输出放在推理结束后。

## 8. 硬件与运行控制

三套Rust后端的正式Kernel环境比较使用同一块Raspberry Pi 5：

```text
固定CPU频率和governor
记录温度、节流状态和供电状态
固定推理线程CPU亲和性
关闭无关日志
release构建
相同Rust版本、target-cpu和NEON选项
```

Linux普通内核与PREEMPT_RT如都测试，应作为两种独立配置。QEMU TCG只用于确认
三套程序能加载模型并产生一致输出，不进入正式速度图。

QEMU Linux PyTorch作为单独的正确性与成熟框架参考实验，可以在QEMU中运行并记录
结果，但其耗时只出现在独立图表中，不与Pi5真机Rust结果计算加速比。

## 9. 指标与结果表

每个模式报告：

```text
min / median / mean / p95 / p99 / max / standard deviation
cycles/inference
milliseconds/inference
inferences/second
```

阶段剖析报告每个网络阶段占总时间比例。主横向表：

| 模式 | Exokernel | seL4 | Linux | Exokernel/Linux | seL4/Linux |
| --- | ---: | ---: | ---: | ---: | ---: |
| M1 模型加载 ms | 待测 | 待测 | 待测 | 待算 | 待算 |
| M2 冷前向 ms | 待测 | 待测 | 待测 | 待算 | 待算 |
| M3 median ms | 待测 | 待测 | 待测 | 待算 | 待算 |
| M3 p99 ms | 待测 | 待测 | 待测 | 待算 | 待算 |
| M4 p99 ms | 待测 | 待测 | 待测 | 待算 | 待算 |

比值定义统一为：

```text
system/Linux = system_time / Linux_time
```

小于1表示比Linux快，大于1表示比Linux慢，避免使用含义不明确的“提升百分比”。

QEMU Linux PyTorch使用独立参考表：

| 输入 | PyTorch max abs reference | Rust最大误差 | PyTorch duration ms | 正确性 |
| --- | ---: | ---: | ---: | --- |
| case-000..004 | 待测 | 待测 | 待测 | 待判定 |

该表不包含任何Kernel性能倍数。

## 10. 原始数据格式

```text
system,commit,runtime_commit,model_sha256,stats_sha256,input_id,mode,
sample,cpu_mask,cycles,timer_ticks,duration_ns,temperature_c,throttled,
max_abs_error,valid
```

阶段数据使用另一张表：

```text
system,mode,sample,stage,cycles,duration_ns
```

## 11. 当前实现状态

| 项目 | 状态 |
| --- | --- |
| Exokernel读取ext4模型 | 已实现并在QEMU验证 |
| Exokernel完整ACT前向 | 已实现，当前单线程 |
| Linux Rust `act-runtime` runner | 已在QEMU AArch64 Linux完成5组/3000值正确性验证 |
| Linux PyTorch参考 | 模型仓库与LeRobot路径可用 |
| QEMU Linux PyTorch完整ACT运行 | 待实现 |
| seL4 `act-runtime`用户任务 | 已移植，已链接完整模型与五组正确性资产 |
| Pi5上的统一真机比较 | 尚未执行 |
| 四核并行ACT算子 | 尚未实现 |

## 12. 实施顺序

1. 直接复用实验02冻结的模型、normalizer、五组输入和PyTorch参考输出。
2. 为Linux基线、Exokernel和seL4实现统一benchmark runner与输出格式。
3. 为Exokernel ACT应用增加无日志批量模式、PMU和CSV输出。
4. 将同一`act-runtime`移植到seL4用户任务，并提供模型字节和workspace。
5. 在QEMU Linux中运行原生LeRobot/PyTorch，导出五组完整动作CSV。
6. 在QEMU完成PyTorch与三套Rust路径的数值一致性验证。
7. 在同一Pi5上依次启动三套Rust系统，执行M1至M4。
8. 分别汇总Kernel环境比较和PyTorch参考图，不混算加速比。
9. 完成统一四核后端后再执行M5。

## 13. 验收条件

- Linux Rust、Exokernel Rust和seL4 Rust必须分别通过实验02的全部真实样本数值
  正确性检查，未通过的路径不得进入Kernel性能比较。
- 三套系统使用相同模型、normalizer、输入和`act-runtime`源码。
- 所有输出先通过数值正确性检查，再进入性能统计。
- 模型加载、冷前向、稳定态前向和受干扰前向分别报告。
- M3计时区间中没有文件I/O、分配或日志。
- QEMU结果不用于真机性能结论。
- QEMU Linux PyTorch只用于正确性和成熟框架参考，不参与Kernel性能比值。
- 同时报告median、p99和max，不能只报告最佳值或平均值。
- PyTorch只作为数值/成熟后端参考，不冒充Linux OS本身的比较结果。
