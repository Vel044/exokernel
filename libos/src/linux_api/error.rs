// SPDX-License-Identifier: GPL-2.0-or-later
//! 错误判断沿用 Linux 名称，参数改为 Result；不再生成或解引用编码错误指针。
pub const EINVAL: i32 = -22;
pub const EBUSY: i32 = -16;
pub const ENXIO: i32 = -6;
pub const EOPNOTSUPP: i32 = -95;
pub fn IS_ERR<T>(value: &Result<T, i32>) -> bool {
    value.is_err()
}
pub fn PTR_ERR<T>(value: &Result<T, i32>) -> i32 {
    match value {
        Err(e) => *e,
        Ok(_) => 0,
    }
}
/// 仅保留分支含义；稳定 Rust 的这个版本不提供 Linux 的预测优化提示。
pub fn unlikely(condition: bool) -> bool {
    condition
}
