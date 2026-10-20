//! Exokernel 对 RuTorch 规范运行时的兼容包名。
//!
//! 此crate只转发到vendor/rutorch中的规范运行时快照，不依赖相邻仓库。
//! ACT图、Tensor、内存计划和算子以RuTorch为唯一实现来源；同步脚本记录
//! 上游commit、未提交差异和实际源码树SHA，禁止手工维护第二套实现。

#![no_std]

pub use rutorch_runtime_upstream::*;
