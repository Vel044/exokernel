//! boot_info.rs —— 硬件资源收集器
//!
//! main.rs 在 EBS 前后把硬件资源收拢成 BootInfo 结构体, 传给 kmain。
//! 这个文件定义了那个结构体长什么样。
//!
//! 外核角度: 这些字段就是"内核手里有什么物理资源"——framebuffer 物理地址、
//! 空闲物理内存数量、DTB(设备树)地址等。以后分给 libOS 的 capability 就从这里来。

use uefi::boot::MemoryType;           // 内存类型枚举 (CONVENTIONAL=空闲)
use uefi::mem::memory_map::MemoryMap;  // MemoryMap 遍历接口

// ═══════════════════════════════════════════════════════════════════
// Framebuffer —— 显卡显存描述
// ═══════════════════════════════════════════════════════════════════
// GOP (Graphics Output Protocol) 是 UEFI 的显卡驱动。
// EBS 之后 GOP 协议就死了, 但 framebuffer 的物理内存还在。
// EBS 之前把 base(物理地址)和 size(总字节数)抄出来,
// EBS 之后内核直接往这个物理地址写像素就行了。

pub const MAX_BOOT_RANGES: usize = 64;

#[derive(Default, Clone, Copy)]
#[repr(C)]
pub struct BootRange {
    pub base: u64,
    pub pages: u64,
}

#[derive(Default, Clone, Copy)]
#[repr(C)]
pub struct Framebuffer {
    pub base: u64,   // framebuffer 物理基址 (CPU 往这写数据就是画屏幕)
    pub size: usize, // 总共多少字节 (base+size 范围内的物理内存不可给别人用)
}

/// 物理内存概况: 来自 EBS 时拿到的 UEFI MemoryMap。
/// free_pages = Type 7(EfiConventionalMemory)总页数, 是物理内存分配器的可用池。
#[derive(Default, Clone, Copy)]
#[repr(C)]
pub struct MemInfo {
    pub free_pages: u64,   // EfiConventionalMemory 总页数 (×4KB = 可用字节)
    pub desc_count: usize, // 内存图有多少个条目
    pub desc_size: usize,  // 每个条目多少字节 (不一定等于 sizeof, 固件决定, 遍历必须用它步进)
}

/// EL2 系统寄存器快照: 接管机器时固件留下的 CPU/MMU 状态。
/// 我们落在 EL2, 真正生效的是 *_EL2 (不是 *_EL1)。
#[derive(Default, Clone, Copy)]
#[repr(C)]
pub struct El2Regs {
    pub current_el: u64, // 当前异常级 (应该 = 2, 即 EL2)
    pub sctlr: u64,      // 系统控制寄存器: MMU/cache/对齐检查的开关
    pub tcr: u64,        // 翻译控制寄存器: 页表粒度/虚拟地址位数
    pub mair: u64,       // 内存属性寄存器: 定义页表里的属性索引对应什么内存类型
    pub ttbr0: u64,      // 页表基址寄存器: EL2 页表的物理地址
    pub vbar: u64,       // 异常向量基址: EL2 异常向量表在哪 (我们要换掉它)
    pub hcr: u64,        // Hypervisor 配置寄存器: 虚拟化相关的开关
}

/// 内核启动信息总成 —— 三样净收获 + CPU 状态
///
/// main.rs 里 EBS 前先收 GOP/DTB, EBS 后再收内存图/EL2 寄存器,
/// 全部装进这个结构体, 最后传给 kmain()。
#[derive(Clone, Copy)]
#[repr(C)]
pub struct BootInfo {
    pub fb: Framebuffer,  // ① framebuffer: 显存物理地址+大小
    pub mem: MemInfo,     // ② 空闲物理内存: 有多少可用页
    pub el2: El2Regs,     // ③ CPU寄存器快照: 固件留下的页表/MMU配置
    pub rsdp: u64,        // ACPI RSDP 表地址 (没有操作系统用 ACPI 就是 0)
    pub dtb: u64,         // DeviceTree 地址 (QEMU 默认没有, Pi5 设 SystemTableMode=0x02 才有)
    pub range_count: usize,
    pub ranges: [BootRange; MAX_BOOT_RANGES],
}

impl Default for BootInfo {
    fn default() -> Self {
        Self {
            fb: Framebuffer::default(),
            mem: MemInfo::default(),
            el2: El2Regs::default(),
            rsdp: 0,
            dtb: 0,
            range_count: 0,
            ranges: [BootRange::default(); MAX_BOOT_RANGES],
        }
    }
}

impl BootInfo {
    /// 读 EL2 系统寄存器, 存快照
    /// 放在 EBS 之后调用, 因为 EBS 不影响这些寄存器
    pub fn read_el2(&mut self) {
        self.el2 = El2Regs {
            current_el: mrs!("CurrentEL"),
            sctlr: mrs!("sctlr_el2"),
            tcr: mrs!("tcr_el2"),
            mair: mrs!("mair_el2"),
            ttbr0: mrs!("ttbr0_el2"),
            vbar: mrs!("vbar_el2"),
            hcr: mrs!("hcr_el2"),
        };
    }

    /// 从 EBS 返回的 MemoryMap 统计可用物理内存总页数
    /// 遍历所有条目, 只加 EfiConventionalMemory 的 page_count
    pub fn fill_memmap(&mut self, mmap: &impl MemoryMap) {
        let mut free = 0u64;
        self.range_count = 0;
        for d in mmap.entries() {
            if d.ty == MemoryType::CONVENTIONAL {
                free += d.page_count;
                if self.range_count < MAX_BOOT_RANGES {
                    self.ranges[self.range_count] = BootRange {
                        base: d.phys_start,
                        pages: d.page_count,
                    };
                    self.range_count += 1;
                }
            }
        }
        self.mem = MemInfo {
            free_pages: free,
            desc_count: mmap.len(),
            desc_size: mmap.meta().desc_size,
        };
    }
}
