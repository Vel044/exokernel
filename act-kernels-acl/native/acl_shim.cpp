// 第一方 ACL 微内核桥：只调用 ACL v52.7.0 的 AArch64 SGEMM 汇编入口。
//
// 完整 ACL runtime 会拉入 pthread、mmap、libstdc++ 和 Linux CPU 探测；这些
// 依赖不能进入 EL0 LibOS。因此这里保留 ACL 的 Cortex-A76 NEON 8x12 微内核，
// 在调用者的 persistent/temporary arena 中完成权重预打包和工作区复用。
// C ABI 不泄漏 C++ 类型、异常、线程或全局堆；所有指针只在同步调用期间有效。

#include <stddef.h>
#include <stdint.h>

struct Conv2dConfig {
  size_t input_height;
  size_t input_width;
  size_t input_channels;
  size_t output_channels;
  size_t kernel_height;
  size_t kernel_width;
  size_t output_height;
  size_t output_width;
  size_t strides[2];
  size_t pads[4];
  size_t dilations[2];
};

struct GemmConfig {
  size_t rows;
  size_t inner;
  size_t columns;
};

struct GemmHandle {
  GemmConfig config;
  const float *packed_right;
};

struct ConvHandle {
  Conv2dConfig config;
  const float *packed_right;
  const float *bias;
};

// 由 Rust/LibOS 提供的同步 job 调度桥。回调只在 ACL 调用栈存活期间使用
// context；没有安装平台调度器时，act-runtime 会在当前 CPU 串行执行所有 job。
using JobKernel = void (*)(void *, size_t);
// 弱符号只作为独立微内核测试/静态 archive 的串行兜底；完整 LibOS 会由
// act-runtime 提供同名强符号，把 job 映射到四 lane 原子屏障。这样 ACL crate
// 不会因为脱离 LibOS 链接测试而留下未解析外部符号。
extern "C" __attribute__((weak)) void rutorch_parallel_jobs(void *context,
                                                              size_t jobs,
                                                              JobKernel kernel) {
  for (size_t job = 0; job < jobs; ++job) kernel(context, job);
}

// ACL 源码中的 C++ 名称以 asm label 引入，避免把 arm_gemm 的 C++ ABI 暴露给
// Rust。该函数只接收已按 8x12 panel 排列的内存，内部不分配、不加锁。
extern "C" void acl_sgemm_8x12(const float *, const float *, float *, int, int,
                                int)
    asm("_ZN8arm_gemm20a64_sgemm_asimd_8x12EPKfS1_Pfiii");

static size_t round_up(size_t value, size_t multiple) {
  return (value + multiple - 1) / multiple * multiple;
}

static int valid_gemm(const GemmConfig *config) {
  return config != 0 && config->rows != 0 && config->inner != 0 &&
         config->columns != 0 && config->rows <= 0x7fffffff &&
         config->inner <= 0x7fffffff && config->columns <= 0x7fffffff;
}

static int valid_conv(const Conv2dConfig *config) {
  return config != 0 && config->input_height != 0 && config->input_width != 0 &&
         config->input_channels != 0 && config->output_channels != 0 &&
         config->kernel_height != 0 && config->kernel_width != 0 &&
         config->output_height != 0 && config->output_width != 0 &&
         config->strides[0] != 0 && config->strides[1] != 0 &&
         config->dilations[0] != 0 && config->dilations[1] != 0 &&
         config->output_height <= 0x7fffffff &&
         config->output_width <= 0x7fffffff;
}

static size_t gemm_persistent(const GemmConfig *config) {
  const size_t k = round_up(config->inner, 2);
  const size_t n = round_up(config->columns, 12);
  return round_up(sizeof(GemmHandle), 64) + k * n * sizeof(float);
}

static size_t gemm_temporary(const GemmConfig *config) {
  const size_t k = round_up(config->inner, 2);
  const size_t n = round_up(config->columns, 12);
  // 一个 8 行 A panel 和一个 8xN C panel，按 A block 复用。
  return round_up(8 * k * sizeof(float), 64) + 8 * n * sizeof(float);
}

static size_t gemm_raw_temporary(const GemmConfig *config) {
  const size_t k = round_up(config->inner, 2);
  const size_t n = round_up(config->columns, 12);
  const size_t packed_right = k * n * sizeof(float);
  const size_t row_blocks = (config->rows + 7) / 8;
  const size_t jobs = row_blocks < 4 ? row_blocks : 4;
  return round_up(packed_right, 64) + jobs * gemm_temporary(config);
}

extern "C" int rutorch_acl_gemm_query_persistent(const GemmConfig *config,
                                                   size_t *bytes) {
  if (!valid_gemm(config) || bytes == 0) return -1;
  *bytes = gemm_persistent(config);
  return 0;
}

extern "C" int rutorch_acl_gemm_query_temporary(const GemmConfig *config,
                                                  size_t *bytes) {
  if (!valid_gemm(config) || bytes == 0) return -1;
  *bytes = gemm_temporary(config);
  return 0;
}

extern "C" int rutorch_acl_gemm_query_raw_temporary(const GemmConfig *config,
                                                      size_t *bytes) {
  if (!valid_gemm(config) || bytes == 0) return -1;
  *bytes = gemm_raw_temporary(config);
  return 0;
}

extern "C" int rutorch_acl_gemm_prepare(const GemmConfig *config,
                                         const float *right, void *persistent,
                                         size_t persistent_bytes,
                                         void **handle) {
  if (!valid_gemm(config) || right == 0 || persistent == 0 || handle == 0 ||
      persistent_bytes < gemm_persistent(config) ||
      ((uintptr_t)persistent & 63u) != 0) {
    return -1;
  }
  GemmHandle *result = static_cast<GemmHandle *>(persistent);
  result->config = *config;
  const size_t k = round_up(config->inner, 2);
  const size_t n = round_up(config->columns, 12);
  float *packed = reinterpret_cast<float *>(reinterpret_cast<uint8_t *>(persistent) +
                                            round_up(sizeof(GemmHandle), 64));
  for (size_t block = 0; block < n; block += 12) {
    for (size_t row = 0; row < k; ++row) {
      for (size_t col = 0; col < 12; ++col) {
        const size_t c = block + col;
        // Rust/ACT 线性层权重按 [out, in] 保存；ACL B panel 需要 [in, out]。
        packed[(block / 12) * k * 12 + row * 12 + col] =
            row < config->inner && c < config->columns ? right[c * config->inner + row] : 0.0f;
      }
    }
  }
  result->packed_right = packed;
  *handle = result;
  return 0;
}

static void run_gemm_rows(const GemmConfig *config, const float *packed_right,
                          const float *bias, const float *input, float *output,
                          uint8_t *temporary, size_t first_block,
                          size_t last_block) {
  const size_t k = round_up(config->inner, 2);
  const size_t n = round_up(config->columns, 12);
  const size_t a_bytes = round_up(8 * k * sizeof(float), 64);
  float *a_panel = reinterpret_cast<float *>(temporary);
  float *c_panel = reinterpret_cast<float *>(temporary + a_bytes);
  const size_t bblocks = (n + 11) / 12;
  for (size_t block = first_block; block < last_block; ++block) {
    const size_t row0 = block * 8;
    for (size_t row = 0; row < 8; ++row) {
      for (size_t depth = 0; depth < k; ++depth) {
        a_panel[depth * 8 + row] =
            row0 + row < config->rows && depth < config->inner
                ? input[(row0 + row) * config->inner + depth]
                : 0.0f;
      }
    }
    acl_sgemm_8x12(a_panel, packed_right, c_panel, 1, (int)bblocks, (int)k);
    for (size_t row = 0; row < 8 && row0 + row < config->rows; ++row) {
      for (size_t col = 0; col < config->columns; ++col) {
        const size_t block = col / 12;
        float value = c_panel[block * 96 + row * 12 + col % 12];
        if (bias != 0) value += bias[col];
        output[(row0 + row) * config->columns + col] = value;
      }
    }
  }
}

static void run_gemm_panels(const GemmConfig *config, const float *packed_right,
                            const float *bias, const float *input, float *output,
                            uint8_t *temporary) {
  run_gemm_rows(config, packed_right, bias, input, output, temporary, 0,
                (config->rows + 7) / 8);
}

struct GemmJobContext {
  const GemmConfig *config;
  const float *packed_right;
  const float *bias;
  const float *input;
  float *output;
  uint8_t *temporary;
  size_t scratch_bytes;
  size_t jobs;
};

static void run_gemm_job(void *opaque, size_t job) {
  GemmJobContext *context = static_cast<GemmJobContext *>(opaque);
  const size_t blocks = (context->config->rows + 7) / 8;
  const size_t first = blocks * job / context->jobs;
  const size_t last = blocks * (job + 1) / context->jobs;
  run_gemm_rows(context->config, context->packed_right, context->bias,
                context->input, context->output,
                context->temporary + job * context->scratch_bytes, first, last);
}

static void run_gemm_panels_parallel(const GemmConfig *config,
                                     const float *packed_right, const float *bias,
                                     const float *input, float *output,
                                     uint8_t *temporary) {
  const size_t blocks = (config->rows + 7) / 8;
  const size_t jobs = blocks < 4 ? blocks : 4;
  if (jobs < 2) {
    run_gemm_panels(config, packed_right, bias, input, output, temporary);
    return;
  }
  GemmJobContext context = {config, packed_right, bias, input, output,
                            temporary, gemm_temporary(config), jobs};
  // 每个 job 固定对应一个 scratch lane；Rust 回调保证 job 完成后才返回。
  rutorch_parallel_jobs(&context, jobs, run_gemm_job);
}

extern "C" int rutorch_acl_gemm_run(void *opaque, const float *input,
                                      float *output, void *temporary,
                                      size_t temporary_bytes) {
  if (opaque == 0 || input == 0 || output == 0 || temporary == 0) return -1;
  GemmHandle *handle = static_cast<GemmHandle *>(opaque);
  if (temporary_bytes < gemm_temporary(&handle->config)) return -1;
  run_gemm_panels(&handle->config, handle->packed_right, 0, input, output,
                  static_cast<uint8_t *>(temporary));
  return 0;
}

extern "C" int rutorch_acl_gemm_run_raw(const GemmConfig *config,
                                          const float *input,
                                          const float *right, const float *bias,
                                          float *output, void *temporary,
                                          size_t temporary_bytes) {
  if (!valid_gemm(config) || input == 0 || right == 0 || output == 0 ||
      temporary == 0 || temporary_bytes < gemm_raw_temporary(config))
    return -1;
  uint8_t *base = static_cast<uint8_t *>(temporary);
  const size_t packed_bytes = round_up(round_up(config->inner, 2) *
                                           round_up(config->columns, 12) * sizeof(float),
                                       64);
  float *packed = reinterpret_cast<float *>(base);
  const size_t k = round_up(config->inner, 2);
  const size_t n = round_up(config->columns, 12);
  for (size_t block = 0; block < n; block += 12) {
    for (size_t row = 0; row < k; ++row) {
      for (size_t col = 0; col < 12; ++col) {
        const size_t c = block + col;
        packed[(block / 12) * k * 12 + row * 12 + col] =
            row < config->inner && c < config->columns ? right[c * config->inner + row] : 0.0f;
      }
    }
  }
  run_gemm_panels_parallel(config, packed, bias, input, output,
                           base + packed_bytes);
  return 0;
}

extern "C" void rutorch_acl_gemm_destroy(void *) {}

static size_t conv_columns(const Conv2dConfig *config) {
  return config->input_channels * config->kernel_height * config->kernel_width;
}

static size_t conv_persistent(const Conv2dConfig *config) {
  GemmConfig gemm = {config->output_height * config->output_width,
                     conv_columns(config), config->output_channels};
  return round_up(sizeof(ConvHandle), 64) +
         round_up(gemm.inner, 2) * round_up(gemm.columns, 12) * sizeof(float);
}

static size_t conv_temporary(const Conv2dConfig *config) {
  GemmConfig gemm = {config->output_height * config->output_width,
                     conv_columns(config), config->output_channels};
  return gemm_temporary(&gemm);
}

static size_t conv_raw_temporary(const Conv2dConfig *config) {
  GemmConfig gemm = {config->output_height * config->output_width,
                     conv_columns(config), config->output_channels};
  const size_t b = round_up(round_up(gemm.inner, 2) *
                                round_up(gemm.columns, 12) * sizeof(float),
                            64);
  const size_t row_blocks = (gemm.rows + 7) / 8;
  const size_t jobs = row_blocks < 4 ? row_blocks : 4;
  return b + jobs * gemm_temporary(&gemm);
}

extern "C" int rutorch_acl_conv2d_query_persistent(const Conv2dConfig *config,
                                                    size_t *bytes) {
  if (!valid_conv(config) || bytes == 0) return -1;
  *bytes = conv_persistent(config);
  return 0;
}

extern "C" int rutorch_acl_conv2d_query_temporary(const Conv2dConfig *config,
                                                   size_t *bytes) {
  if (!valid_conv(config) || bytes == 0) return -1;
  *bytes = conv_temporary(config);
  return 0;
}

extern "C" int rutorch_acl_conv2d_query_raw_temporary(const Conv2dConfig *config,
                                                        size_t *bytes) {
  if (!valid_conv(config) || bytes == 0) return -1;
  *bytes = conv_raw_temporary(config);
  return 0;
}

extern "C" int rutorch_acl_conv2d_prepare(const Conv2dConfig *config,
                                           const float *weight,
                                           const float *bias, void *persistent,
                                           size_t persistent_bytes,
                                           void **handle) {
  if (!valid_conv(config) || weight == 0 || persistent == 0 || handle == 0 ||
      persistent_bytes < conv_persistent(config) ||
      ((uintptr_t)persistent & 63u) != 0) return -1;
  ConvHandle *result = static_cast<ConvHandle *>(persistent);
  result->config = *config;
  result->bias = bias;
  const size_t k = round_up(conv_columns(config), 2);
  const size_t n = round_up(config->output_channels, 12);
  float *packed = reinterpret_cast<float *>(reinterpret_cast<uint8_t *>(persistent) +
                                            round_up(sizeof(ConvHandle), 64));
  for (size_t block = 0; block < n; block += 12) {
    for (size_t depth = 0; depth < k; ++depth) {
      const size_t channel = depth % config->input_channels;
      const size_t kernel_index = depth / config->input_channels;
      const size_t ky = kernel_index / config->kernel_width;
      const size_t kx = kernel_index % config->kernel_width;
      for (size_t col = 0; col < 12; ++col) {
        const size_t output_channel = block + col;
        packed[(block / 12) * k * 12 + depth * 12 + col] =
            depth < conv_columns(config) && output_channel < config->output_channels
                ? weight[(output_channel * config->input_channels * config->kernel_height * config->kernel_width) +
                         channel * config->kernel_height * config->kernel_width +
                         ky * config->kernel_width + kx]
                : 0.0f;
      }
    }
  }
  result->packed_right = packed;
  *handle = result;
  return 0;
}

extern "C" int rutorch_acl_conv2d_run(void *opaque, const float *input,
                                        float *output, void *temporary,
                                        size_t temporary_bytes) {
  if (opaque == 0 || input == 0 || output == 0 || temporary == 0) return -1;
  ConvHandle *handle = static_cast<ConvHandle *>(opaque);
  const Conv2dConfig *config = &handle->config;
  if (temporary_bytes < conv_temporary(config)) return -1;
  GemmConfig gemm = {config->output_height * config->output_width,
                     conv_columns(config), config->output_channels};
  const size_t k = round_up(gemm.inner, 2);
  const size_t n = round_up(gemm.columns, 12);
  const size_t a_bytes = round_up(8 * k * sizeof(float), 64);
  float *a_panel = reinterpret_cast<float *>(temporary);
  float *c_panel = reinterpret_cast<float *>(static_cast<uint8_t *>(temporary) + a_bytes);
  const size_t bblocks = (n + 11) / 12;
  for (size_t row0 = 0; row0 < gemm.rows; row0 += 8) {
    for (size_t row = 0; row < 8; ++row) {
      const size_t output_row = row0 + row;
      const size_t oy = output_row / config->output_width;
      const size_t ox = output_row % config->output_width;
      for (size_t depth = 0; depth < k; ++depth) {
        float value = 0.0f;
        if (depth < gemm.inner) {
          const size_t channel = depth % config->input_channels;
          const size_t kernel_index = depth / config->input_channels;
          const size_t ky = kernel_index / config->kernel_width;
          const size_t kx = kernel_index % config->kernel_width;
          const int64_t iy = static_cast<int64_t>(oy * config->strides[0] + ky * config->dilations[0]) -
                             static_cast<int64_t>(config->pads[0]);
          const int64_t ix = static_cast<int64_t>(ox * config->strides[1] + kx * config->dilations[1]) -
                             static_cast<int64_t>(config->pads[1]);
          if (iy >= 0 && ix >= 0 && iy < (int64_t)config->input_height &&
              ix < (int64_t)config->input_width)
            value = input[(static_cast<size_t>(iy) * config->input_width + static_cast<size_t>(ix)) *
                          config->input_channels + channel];
        }
        a_panel[depth * 8 + row] = value;
      }
    }
    acl_sgemm_8x12(a_panel, handle->packed_right, c_panel, 1, (int)bblocks, (int)k);
    for (size_t row = 0; row < 8 && row0 + row < gemm.rows; ++row) {
      for (size_t col = 0; col < gemm.columns; ++col) {
        const size_t block = col / 12;
        float value = c_panel[block * 96 + row * 12 + col % 12];
        if (handle->bias != 0) value += handle->bias[col];
        output[(row0 + row) * gemm.columns + col] = value;
      }
    }
  }
  return 0;
}

extern "C" int rutorch_acl_conv2d_run_raw(const Conv2dConfig *config,
                                            const float *input,
                                            const float *weight,
                                            const float *bias, float *output,
                                            void *temporary,
                                            size_t temporary_bytes) {
  if (!valid_conv(config) || input == 0 || weight == 0 || output == 0 ||
      temporary == 0 || temporary_bytes < conv_raw_temporary(config))
    return -1;
  GemmConfig gemm = {config->output_height * config->output_width,
                     conv_columns(config), config->output_channels};
  uint8_t *base = static_cast<uint8_t *>(temporary);
  const size_t packed_bytes = round_up(round_up(gemm.inner, 2) *
                                           round_up(gemm.columns, 12) * sizeof(float),
                                       64);
  float *packed = reinterpret_cast<float *>(base);
  const size_t k = round_up(gemm.inner, 2);
  const size_t n = round_up(gemm.columns, 12);
  for (size_t block = 0; block < n; block += 12) {
    for (size_t depth = 0; depth < k; ++depth) {
      const size_t channel = depth % config->input_channels;
      const size_t kernel_index = depth / config->input_channels;
      const size_t ky = kernel_index / config->kernel_width;
      const size_t kx = kernel_index % config->kernel_width;
      for (size_t col = 0; col < 12; ++col) {
        const size_t output_channel = block + col;
        packed[(block / 12) * k * 12 + depth * 12 + col] =
            depth < gemm.inner && output_channel < gemm.columns
                ? weight[(output_channel * config->input_channels * config->kernel_height * config->kernel_width) +
                         channel * config->kernel_height * config->kernel_width +
                         ky * config->kernel_width + kx]
                : 0.0f;
      }
    }
  }
  const size_t scratch_bytes = gemm_temporary(&gemm);
  const size_t row_blocks = (gemm.rows + 7) / 8;
  const size_t jobs = row_blocks < 4 ? row_blocks : 4;
  struct ConvJobContext {
    const Conv2dConfig *config;
    const GemmConfig *gemm;
    const float *input;
    const float *packed;
    const float *bias;
    float *output;
    uint8_t *scratch;
    size_t scratch_bytes;
    size_t row_blocks;
    size_t jobs;
  } context = {config, &gemm, input, packed, bias, output, base + packed_bytes,
               scratch_bytes, row_blocks, jobs};
  auto run_job = [](void *opaque, size_t job) {
    ConvJobContext *ctx = static_cast<ConvJobContext *>(opaque);
    const size_t first = ctx->row_blocks * job / ctx->jobs;
    const size_t last = ctx->row_blocks * (job + 1) / ctx->jobs;
    const size_t k_local = round_up(ctx->gemm->inner, 2);
    const size_t n_local = round_up(ctx->gemm->columns, 12);
    const size_t a_bytes_local = round_up(8 * k_local * sizeof(float), 64);
    float *a_panel_local = reinterpret_cast<float *>(ctx->scratch + job * ctx->scratch_bytes);
    float *c_panel_local = reinterpret_cast<float *>(
        ctx->scratch + job * ctx->scratch_bytes + a_bytes_local);
    const size_t bblocks_local = (n_local + 11) / 12;
    for (size_t block = first; block < last; ++block) {
      const size_t row0 = block * 8;
      for (size_t row = 0; row < 8; ++row) {
      const size_t output_row = row0 + row;
      const size_t oy = output_row / ctx->config->output_width;
      const size_t ox = output_row % ctx->config->output_width;
      for (size_t depth = 0; depth < k_local; ++depth) {
        float value = 0.0f;
        if (depth < ctx->gemm->inner) {
          const size_t channel = depth % ctx->config->input_channels;
          const size_t kernel_index = depth / ctx->config->input_channels;
          const size_t ky = kernel_index / ctx->config->kernel_width;
          const size_t kx = kernel_index % ctx->config->kernel_width;
          const int64_t iy = static_cast<int64_t>(oy * ctx->config->strides[0] +
                                                   ky * ctx->config->dilations[0]) -
                             static_cast<int64_t>(ctx->config->pads[0]);
          const int64_t ix = static_cast<int64_t>(ox * ctx->config->strides[1] +
                                                   kx * ctx->config->dilations[1]) -
                             static_cast<int64_t>(ctx->config->pads[1]);
          if (iy >= 0 && ix >= 0 && iy < (int64_t)ctx->config->input_height &&
              ix < (int64_t)ctx->config->input_width)
            value = ctx->input[(static_cast<size_t>(iy) * ctx->config->input_width +
                                static_cast<size_t>(ix)) *
                               ctx->config->input_channels + channel];
        }
        a_panel_local[depth * 8 + row] = value;
      }
      }
    acl_sgemm_8x12(a_panel_local, ctx->packed, c_panel_local, 1,
                   (int)bblocks_local, (int)k_local);
      for (size_t row = 0; row < 8 && row0 + row < ctx->gemm->rows; ++row) {
        for (size_t col = 0; col < ctx->gemm->columns; ++col) {
        const size_t block = col / 12;
        float value = c_panel_local[block * 96 + row * 12 + col % 12];
        if (ctx->bias != 0) value += ctx->bias[col];
        ctx->output[(row0 + row) * ctx->gemm->columns + col] = value;
        }
      }
    }
  };
  if (jobs < 2) {
    run_job(&context, 0);
  } else {
    rutorch_parallel_jobs(&context, jobs, run_job);
  }
  return 0;
}

extern "C" void rutorch_acl_conv2d_destroy(void *) {}
