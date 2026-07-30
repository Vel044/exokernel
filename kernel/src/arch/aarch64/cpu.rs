//! AArch64 CPU-local编号。
//!
//! `TPIDR_EL1`保存当前核的`CpuLocal`指针，只供Kernel读取；
//! `TPIDRRO_EL0`把逻辑编号只读暴露给libOS。两者都不是物理资源权限句柄。

/// 每个逻辑CPU都拥有一份只读身份对象。
///
/// 后续每核Ready Queue、当前线程和重调度标志仍由调度器自己的原子对象保存；
/// 需要增加更多CPU-local字段时可继续扩展本结构，而不改变`TPIDR_EL1`用法。
#[repr(C)]
struct CpuLocal {
    /// DTB CPU顺序对应的逻辑编号，也是GICv2 target bit编号。
    id: usize,
}

static CPU_LOCALS: [CpuLocal; exo_abi::MAX_CPUS] = [
    CpuLocal { id: 0 },
    CpuLocal { id: 1 },
    CpuLocal { id: 2 },
    CpuLocal { id: 3 },
];

pub fn set_id(cpu: usize) {
    assert!(cpu < CPU_LOCALS.len());
    let local = &CPU_LOCALS[cpu] as *const CpuLocal as u64;
    unsafe {
        core::arch::asm!(
            "msr tpidr_el1, {local}",
            "msr tpidrro_el0, {id}",
            "isb",
            local = in(reg) local,
            id = in(reg) cpu as u64,
            options(nomem, nostack)
        );
    }
}

#[inline(always)]
pub fn id() -> usize {
    let local: *const CpuLocal;
    unsafe {
        core::arch::asm!(
            "mrs {local}, tpidr_el1",
            local = out(reg) local,
            options(nomem, nostack)
        );
        // 安全前提：每个CPU在进入共享Kernel路径前都调用set_id；CPU_LOCALS
        // 是生命周期覆盖整个Kernel的只读静态数组，不会移动或被释放。
        (*local).id
    }
}
