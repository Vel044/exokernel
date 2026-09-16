//! Exokernel 对 RuTorch 规范运行时的兼容包名。
//!
//! ACT 图、Tensor、内存计划和算子不在本仓库复制；此 crate 只转发到相邻
//! rutorch 仓库的同一源码树。离线发布由同步脚本生成带 commit/tree SHA 的
//! vendored 快照，禁止手工维护第二套实现。

#![no_std]

pub use rutorch_runtime_upstream::*;
