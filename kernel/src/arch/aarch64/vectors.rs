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
    // 272 字节 = x0..x30(31*8) + SP_EL0/ELR_EL1/SPSR_EL1。
    sub sp, sp, #272
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x0, sp_el0
    str x0, [sp, #248]
    mrs x0, elr_el1
    str x0, [sp, #256]
    mrs x0, spsr_el1
    str x0, [sp, #264]
    mov x0, sp
    bl el1_sync_handler_frame
    b __vec_el1_restore

__vec_el1_irq:
    sub sp, sp, #272
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x0, sp_el0
    str x0, [sp, #248]
    mrs x0, elr_el1
    str x0, [sp, #256]
    mrs x0, spsr_el1
    str x0, [sp, #264]
    mov x0, sp
    bl el1_irq_handler_frame

__vec_el1_restore:
    // Rust 返回目标线程的异常帧地址。先保存系统寄存器值，释放当前
    // EL1 临时栈帧，再恢复目标线程的全部寄存器并返回 EL0。
    mov x19, x0
    ldr x16, [x19, #248]
    ldr x17, [x19, #256]
    ldr x18, [x19, #264]
    add sp, sp, #272
    msr sp_el0, x16
    msr elr_el1, x17
    msr spsr_el1, x18
    ldp x0, x1, [x19, #0]
    ldp x2, x3, [x19, #16]
    ldp x4, x5, [x19, #32]
    ldp x6, x7, [x19, #48]
    ldp x8, x9, [x19, #64]
    ldp x10, x11, [x19, #80]
    ldp x12, x13, [x19, #96]
    ldp x14, x15, [x19, #112]
    ldp x16, x17, [x19, #128]
    ldr x18, [x19, #144]
    ldp x20, x21, [x19, #160]
    ldp x22, x23, [x19, #176]
    ldp x24, x25, [x19, #192]
    ldp x26, x27, [x19, #208]
    ldp x28, x29, [x19, #224]
    ldr x30, [x19, #240]
    ldr x19, [x19, #152]
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
