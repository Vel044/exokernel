//! vectors.rs —— EL2/EL1 两套异常向量表
//!
//! EL2 表只做 boot shim 的意外异常诊断。
//! EL1 表处理来自 EL0 的 SVC/Data Abort/IRQ。

use core::arch::global_asm;

global_asm!(
    r#"
.section .text.vectors,"ax"

.balign 2048
.global vector_table_el2
vector_table_el2:
    // Current EL, SP_EL0
    .balign 128
    b .
    .balign 128
    b .
    .balign 128
    b .
    .balign 128
    b .

    // Current EL, SP_ELx
    .balign 128
    b __vec_el2_sync
    .balign 128
    b __vec_el2_irq
    .balign 128
    b .
    .balign 128
    b .

    // Lower EL, AArch64
    .balign 128
    b __vec_el2_sync
    .balign 128
    b __vec_el2_irq
    .balign 128
    b .
    .balign 128
    b .

    // Lower EL, AArch32
    .balign 128
    b .
    .balign 128
    b .
    .balign 128
    b .
    .balign 128
    b .

__vec_el2_sync:
    mov x0, x2
    bl el2_sync_handler
    eret

__vec_el2_irq:
    bl el2_irq_handler
    eret

.balign 2048
.global vector_table_el1
vector_table_el1:
    // Current EL, SP_EL0
    .balign 128
    b __vec_el1_sync
    .balign 128
    b __vec_el1_irq
    .balign 128
    b .
    .balign 128
    b .

    // Current EL, SP_ELx
    .balign 128
    b __vec_el1_sync
    .balign 128
    b __vec_el1_irq
    .balign 128
    b .
    .balign 128
    b .

    // Lower EL, AArch64: EL0 的 SVC/缺页/中断走这里
    .balign 128
    b __vec_el1_sync
    .balign 128
    b __vec_el1_irq
    .balign 128
    b .
    .balign 128
    b .

    // Lower EL, AArch32
    .balign 128
    b .
    .balign 128
    b .
    .balign 128
    b .
    .balign 128
    b .

__vec_el1_sync:
    sub sp, sp, #16
    str x30, [sp]
    mov x3, x8
    bl el1_sync_handler
    // Rust handler 按 AAPCS64 在 x0 返回 syscall 结果。把结果放到异常栈，
    // 避免借用任意 EL0 通用寄存器作为临时值，再恢复到 eret 返回时的 x0。
    str x0, [sp, #8]
    ldr x30, [sp]
    ldr x0, [sp, #8]
    add sp, sp, #16
    eret

__vec_el1_irq:
    // 保存 caller-saved 寄存器 + x30 (AAPCS64)，确保 IRQ handler 可以
    // 安全调用 gic::acknowledge() / task::record_iar() 等 Rust 函数。
    sub sp, sp, #160
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    str x18, [sp, #144]
    str x30, [sp, #152]
    bl el1_irq_handler
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldr x18, [sp, #144]
    ldr x30, [sp, #152]
    add sp, sp, #160
    eret
"#
);

extern "C" {
    pub static vector_table_el2: u8;
    pub static vector_table_el1: u8;
}

pub fn install_el2() -> u64 {
    let vbar = unsafe { &vector_table_el2 as *const u8 as u64 };
    unsafe {
        core::arch::asm!("msr vbar_el2, {}", "isb", in(reg) vbar, options(nomem, nostack));
    }
    vbar
}

pub fn install_el1() -> u64 {
    let vbar = unsafe { &vector_table_el1 as *const u8 as u64 };
    unsafe {
        core::arch::asm!("msr vbar_el1, {}", "isb", in(reg) vbar, options(nomem, nostack));
    }
    vbar
}

/// 从 EL2 预装 EL1 的 VBAR。
///
/// 用于调试 EL2->EL1 eret: 如果 EL1 第一条指令附近就异常,
/// 还没跑到 kmain::el1_main() 里的 install_el1(), 也能进入同一套 EL1 handler。
pub fn install_el1_from_el2() -> u64 {
    install_el1()
}
