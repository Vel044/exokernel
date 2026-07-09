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
    mov x4, x0
    ldr x30, [sp]
    add sp, sp, #16
    mov x0, x4
    eret

__vec_el1_irq:
    sub sp, sp, #16
    str x30, [sp]
    bl el1_irq_handler
    ldr x30, [sp]
    add sp, sp, #16
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
