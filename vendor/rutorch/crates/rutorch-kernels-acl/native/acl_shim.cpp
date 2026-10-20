#include "arm_compute/core/TensorInfo.h"
#include "arm_compute/core/Types.h"
#include "arm_compute/runtime/NEON/NEScheduler.h"
#include "arm_compute/runtime/NEON/functions/NEConvolutionLayer.h"
#include "arm_compute/runtime/NEON/functions/NEGEMM.h"
#include "arm_compute/runtime/Tensor.h"

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <exception>
#include <memory>
#include <new>
#include <string>

// Linux Full ACL的OpenMP运行时与其计算线程池相同。加载期校验四个worker。
extern "C" int rutorch_acl_configure_fpcr() {
    int failures = 0;
    #pragma omp parallel num_threads(4) reduction(+:failures)
    {
        uint64_t value;
        asm volatile("mrs %0, fpcr" : "=r"(value));
        value &= ~((1ULL<<24)|(1ULL<<19)|(1ULL<<25)|(3ULL<<22));
        asm volatile("msr fpcr, %0\n isb" :: "r"(value));
        asm volatile("mrs %0, fpcr" : "=r"(value));
        if (value & ((1ULL<<24)|(1ULL<<19)|(1ULL<<25)|(3ULL<<22))) ++failures;
    }
    return failures;
}

namespace {

thread_local std::string last_error;

struct Conv2dConfig {
    std::size_t input_channels;
    std::size_t input_height;
    std::size_t input_width;
    std::size_t output_channels;
    std::size_t kernel_height;
    std::size_t kernel_width;
    std::size_t output_height;
    std::size_t output_width;
    std::size_t strides[2];
    std::size_t pads[4];
    std::size_t dilations[2];
    std::size_t threads;
};

struct GemmConfig {
    std::size_t rows;
    std::size_t inner;
    std::size_t columns;
    std::size_t threads;
};

struct Conv2dHandle {
    arm_compute::Tensor input;
    arm_compute::Tensor weight;
    arm_compute::Tensor bias;
    arm_compute::Tensor output;
    arm_compute::NEConvolutionLayer operation;
    std::size_t input_bytes{};
    std::size_t output_bytes{};
    bool has_bias{};
};

struct GemmHandle {
    arm_compute::Tensor input;
    arm_compute::Tensor right;
    arm_compute::Tensor output;
    arm_compute::NEGEMM operation;
    std::size_t input_bytes{};
    std::size_t output_bytes{};
};

} // namespace

extern "C" void *rutorch_acl_conv2d_create(
    const Conv2dConfig *config,
    const float *weight,
    const float *bias)
{
    try {
        last_error.clear();
        // 创建任一步骤抛异常时由 unique_ptr 回收已经分配的 ACL tensor；只有
        // configure/allocate/prepare 全部成功后才把所有权交给 Rust handle。
        auto handle = std::make_unique<Conv2dHandle>();
        handle->has_bias = bias != nullptr;
        handle->input_bytes = config->input_channels * config->input_height *
                              config->input_width * sizeof(float);
        handle->output_bytes = config->output_channels * config->output_height *
                               config->output_width * sizeof(float);
        const auto input_info = arm_compute::TensorInfo(
            arm_compute::TensorShape(config->input_width, config->input_height,
                                     config->input_channels, 1),
            1, arm_compute::DataType::F32, arm_compute::DataLayout::NCHW);
        const auto weight_info = arm_compute::TensorInfo(
            arm_compute::TensorShape(config->kernel_width, config->kernel_height,
                                     config->input_channels, config->output_channels),
            1, arm_compute::DataType::F32, arm_compute::DataLayout::NCHW);
        const auto output_info = arm_compute::TensorInfo(
            arm_compute::TensorShape(config->output_width, config->output_height,
                                     config->output_channels, 1),
            1, arm_compute::DataType::F32, arm_compute::DataLayout::NCHW);
        handle->input.allocator()->init(input_info);
        handle->weight.allocator()->init(weight_info);
        handle->output.allocator()->init(output_info);
        if (handle->has_bias) {
            handle->bias.allocator()->init(arm_compute::TensorInfo(
                arm_compute::TensorShape(config->output_channels), 1,
                arm_compute::DataType::F32));
        }
        handle->weight.info()->set_are_values_constant(true);
        const arm_compute::PadStrideInfo conv_info(
            static_cast<unsigned int>(config->strides[1]),
            static_cast<unsigned int>(config->strides[0]),
            static_cast<unsigned int>(config->pads[1]),
            static_cast<unsigned int>(config->pads[3]),
            static_cast<unsigned int>(config->pads[0]),
            static_cast<unsigned int>(config->pads[2]),
            arm_compute::DimensionRoundingType::FLOOR);
        handle->operation.configure(
            &handle->input, &handle->weight,
            handle->has_bias ? &handle->bias : nullptr, &handle->output,
            conv_info, arm_compute::WeightsInfo(),
            arm_compute::Size2D(config->dilations[1], config->dilations[0]));
        handle->input.allocator()->allocate();
        handle->weight.allocator()->allocate();
        if (handle->has_bias) {
            handle->bias.allocator()->allocate();
        }
        handle->output.allocator()->allocate();
        const auto weight_bytes = config->output_channels * config->input_channels *
                                  config->kernel_height * config->kernel_width * sizeof(float);
        std::memcpy(handle->weight.buffer(), weight, weight_bytes);
        if (handle->has_bias) {
            std::memcpy(handle->bias.buffer(), bias,
                        config->output_channels * sizeof(float));
        }
        arm_compute::NEScheduler::get().set_num_threads(
            static_cast<unsigned int>(config->threads));
        handle->operation.prepare();
        return handle.release();
    } catch (const std::exception &error) {
        last_error = error.what();
    } catch (...) {
        last_error = "ACL Conv 创建时发生未知异常";
    }
    return nullptr;
}

extern "C" int rutorch_acl_conv2d_run(void *raw, const float *input, float *output)
{
    try {
        last_error.clear();
        auto *handle = static_cast<Conv2dHandle *>(raw);
        std::memcpy(handle->input.buffer(), input, handle->input_bytes);
        handle->operation.run();
        std::memcpy(output, handle->output.buffer(), handle->output_bytes);
        return 0;
    } catch (const std::exception &error) {
        last_error = error.what();
    } catch (...) {
        last_error = "ACL Conv 执行时发生未知异常";
    }
    return 1;
}

extern "C" void rutorch_acl_conv2d_destroy(void *raw)
{
    delete static_cast<Conv2dHandle *>(raw);
}

extern "C" const char *rutorch_acl_last_error()
{
    return last_error.c_str();
}

extern "C" void *rutorch_acl_gemm_create(
    const struct GemmConfig *config,
    const float *right)
{
    try {
        last_error.clear();
        auto handle = std::make_unique<GemmHandle>();
        handle->input_bytes = config->rows * config->inner * sizeof(float);
        handle->output_bytes = config->rows * config->columns * sizeof(float);
        handle->input.allocator()->init(arm_compute::TensorInfo(
            arm_compute::TensorShape(config->inner, config->rows), 1,
            arm_compute::DataType::F32));
        handle->right.allocator()->init(arm_compute::TensorInfo(
            arm_compute::TensorShape(config->columns, config->inner), 1,
            arm_compute::DataType::F32));
        handle->output.allocator()->init(arm_compute::TensorInfo(
            arm_compute::TensorShape(config->columns, config->rows), 1,
            arm_compute::DataType::F32));
        handle->right.info()->set_are_values_constant(true);
        handle->operation.configure(
            &handle->input, &handle->right, nullptr, &handle->output, 1.0f, 0.0f);
        handle->input.allocator()->allocate();
        handle->right.allocator()->allocate();
        handle->output.allocator()->allocate();
        const auto right_bytes = config->inner * config->columns * sizeof(float);
        std::memcpy(handle->right.buffer(), right, right_bytes);
        arm_compute::NEScheduler::get().set_num_threads(
            static_cast<unsigned int>(config->threads));
        handle->operation.prepare();
        return handle.release();
    } catch (const std::exception &error) {
        last_error = error.what();
    } catch (...) {
        last_error = "ACL GEMM 创建时发生未知异常";
    }
    return nullptr;
}

extern "C" int rutorch_acl_gemm_run(void *raw, const float *input, float *output)
{
    try {
        last_error.clear();
        auto *handle = static_cast<GemmHandle *>(raw);
        std::memcpy(handle->input.buffer(), input, handle->input_bytes);
        handle->operation.run();
        std::memcpy(output, handle->output.buffer(), handle->output_bytes);
        return 0;
    } catch (const std::exception &error) {
        last_error = error.what();
    } catch (...) {
        last_error = "ACL GEMM 执行时发生未知异常";
    }
    return 1;
}

extern "C" void rutorch_acl_gemm_destroy(void *raw)
{
    delete static_cast<GemmHandle *>(raw);
}
