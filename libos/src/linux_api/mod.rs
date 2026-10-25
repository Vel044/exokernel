// SPDX-License-Identifier: GPL-2.0-or-later
//! Linux 驱动接口的同名 Rust 封装，不复制 Linux C ABI 或完整内核对象模型。
//!
//! 驱动保留调用意图，通用行为放在本目录；只有 exo 后端进入现有系统调用。
//! 当前支持单所属 EL0 线程、线性 skb、单发送队列和显式 platform 生命周期。
//! 不支持的 IRQ 标志、资源索引和容量返回错误，不用空实现假装成功。
#![allow(dead_code, non_snake_case, non_camel_case_types)]
pub mod error;
#[cfg(feature = "linux-driver-api")]
pub mod exo;
pub mod interrupt;
pub mod io;
pub mod module;
pub mod netdevice;
pub mod platform;
pub mod printk;
pub mod skbuff;
