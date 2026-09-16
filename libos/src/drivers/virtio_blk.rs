//! QEMU virtio-mmio块设备的EL0驱动适配层。
//!
//! Kernel只从DTB裁剪并授权一段virtio-mmio寄存器窗口。本模块映射该窗口，
//! 读取每个transport的`magic/device_id`，并把Block设备交给`virtio-drivers`。
//! 扇区请求、virtqueue描述符和DMA地址都在用户态完成，不存在文件系统syscall。

use core::ptr::NonNull;

use spin::Mutex;
use virtio_drivers::{
    device::blk::VirtIOBlk,
    transport::{
        mmio::{MmioTransport, VirtIOHeader},
        DeviceType, Transport,
    },
    BufferDirection, Hal, PhysAddr,
};

const TRANSPORT_SIZE: usize = 0x200;
const VIRTIO_MAGIC: u32 = 0x7472_6976;
const DMA_SLOTS: usize = 64;

pub type BlockDevice = VirtIOBlk<ExoVirtioHal, MmioTransport<'static>>;

#[derive(Clone, Copy)]
struct DmaSlot {
    /// EL0 CPU访问DMA页的VA。
    va: u64,
    /// 无IOMMU时设备看到的DMA address，也就是Kernel分配的PA。
    pa: u64,
    /// Kernel记录并要求释放时完全匹配的页对齐字节数。
    size: usize,
}

impl DmaSlot {
    const EMPTY: Self = Self {
        va: 0,
        pa: 0,
        size: 0,
    };
}

static DMA: Mutex<[DmaSlot; DMA_SLOTS]> = Mutex::new([DmaSlot::EMPTY; DMA_SLOTS]);

/// 映射Kernel授权的transport窗口，并逐个探测其中的virtio-blk设备。
pub struct BlockDevices {
    // 已映射到EL0的virtio-mmio窗口起始VA。
    base: u64,
    // 该窗口覆盖的总字节数。
    size: usize,
    // 下一次扫描的transport字节偏移。
    offset: usize,
}

impl BlockDevices {
    pub fn discover(info: &exo_abi::UserBootInfo) -> Result<Self, u64> {
        // 读取EL1传给EL0的virtio-mmio资源描述。
        let resource = info.virtio_mmio;
        // 检查MMIO地址、大小和页对齐是否有效。
        if resource.base == 0
            || resource.size < TRANSPORT_SIZE as u64
            || resource.base & (exo_abi::PAGE_SIZE - 1) != 0
            || resource.size & (exo_abi::PAGE_SIZE - 1) != 0
        {
            return Err(0x520);
        }
        // 请求Kernel把物理MMIO窗口映射到固定的EL0虚拟地址。
        crate::runtime::map_mmio(resource.base, resource.size, exo_abi::VIRTIO_MMIO_VA)
            .map_err(|_| 0x521u64)?;
        // 保存映射地址和窗口范围，供后续逐个扫描transport。
        Ok(Self {
            base: exo_abi::VIRTIO_MMIO_VA,
            size: resource.size as usize,
            offset: 0,
        })
    }

    /// 返回下一个块设备。空transport和非Block类型会被跳过。
    pub fn next(&mut self) -> Option<BlockDevice> {
        while self.offset + TRANSPORT_SIZE <= self.size {
            // 计算当前transport的EL0寄存器地址。
            let address = self.base + self.offset as u64;
            // 推进扫描位置，避免下次重复检查当前transport。
            self.offset += TRANSPORT_SIZE;

            // 读取设备标识寄存器，确认这是一个有效的virtio transport。
            // SAFETY:窗口已由Kernel映射为Device memory，address落在映射内且
            // 4字节对齐。volatile确保编译器真的读取设备寄存器。
            let magic = unsafe { core::ptr::read_volatile(address as *const u32) };
            let device_id = unsafe { core::ptr::read_volatile((address + 8) as *const u32) };
            // 跳过空窗口和非块设备transport。
            if magic != VIRTIO_MAGIC || device_id != DeviceType::Block as u32 {
                continue;
            }
            // 把寄存器地址转换成第三方驱动需要的头指针。
            let Some(header) = NonNull::new(address as *mut VirtIOHeader) else {
                continue;
            };
            // 创建只覆盖当前transport的MMIO访问对象。
            // SAFETY:DTB中的每个transport节点声明0x200字节MMIO区间；该映射
            // 在整个libOS生命周期都有效，且当前迭代器只把该transport交给一个驱动。
            let Ok(transport) = (unsafe { MmioTransport::new(header, TRANSPORT_SIZE) }) else {
                // 一个损坏或不兼容的transport不能阻止继续扫描后面的设备。
                continue;
            };
            // 再次确认transport报告的类型与探测结果一致。
            if transport.device_type() != DeviceType::Block {
                continue;
            }
            // 交给第三方驱动初始化块设备和virtqueue。
            if let Ok(device) = VirtIOBlk::<ExoVirtioHal, _>::new(transport) {
                return Some(device);
            }
        }
        // 已扫描完整个virtio-mmio窗口。
        None
    }
}

/// `virtio-drivers`与外核DMA syscall之间的适配器。
pub struct ExoVirtioHal;

unsafe impl Hal for ExoVirtioHal {
    // 为第三方virtio队列申请连续、设备可见的DMA内存。
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        allocate_dma(pages).unwrap_or((0, NonNull::dangling()))
    }

    // 校验并释放第三方驱动不再使用的DMA内存。
    unsafe fn dma_dealloc(paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        if release_dma(paddr, vaddr, pages) {
            0
        } else {
            -1
        }
    }

    unsafe fn mmio_phys_to_virt(_paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        // 本工程使用MmioTransport，只有PCI transport才会回调这个转换。
        panic!("virtio PCI MMIO translation is not available")
    }

    // 为普通缓存区创建设备可见的DMA bounce buffer。
    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> PhysAddr {
        let len = buffer.len();
        let pages = len.div_ceil(exo_abi::PAGE_SIZE as usize);
        let Some((pa, bounce)) = allocate_dma(pages) else {
            return 0;
        };
        if matches!(
            direction,
            BufferDirection::DriverToDevice | BufferDirection::Both
        ) {
            // SAFETY:Hal调用契约保证原buffer在share期间有效且独占；bounce是
            // 刚分配的至少len字节DMA映射，两段内存不重叠。
            unsafe {
                core::ptr::copy_nonoverlapping(buffer.as_ptr() as *const u8, bounce.as_ptr(), len);
                core::arch::asm!("dmb osh", options(nostack, preserves_flags));
            }
        }
        pa
    }

    // 将设备写入的bounce数据同步回原始CPU缓存区并释放它。
    unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
        let len = buffer.len();
        let slot = {
            let slots = DMA.lock();
            slots
                .iter()
                .copied()
                .find(|slot| slot.pa == paddr && slot.size != 0)
        };
        let Some(slot) = slot else { return };
        if matches!(
            direction,
            BufferDirection::DeviceToDriver | BufferDirection::Both
        ) {
            // 先保证设备写入在Outer Shareable域可见，再复制回普通Cacheable目标。
            unsafe {
                core::arch::asm!("dmb osh", options(nostack, preserves_flags));
                core::ptr::copy_nonoverlapping(
                    slot.va as *const u8,
                    buffer.as_ptr() as *mut u8,
                    len,
                );
            }
        }
        let pages = slot.size / exo_abi::PAGE_SIZE as usize;
        if let Some(pointer) = NonNull::new(slot.va as *mut u8) {
            let _ = release_dma(paddr, pointer, pages);
        }
    }
}

fn allocate_dma(pages: usize) -> Option<(PhysAddr, NonNull<u8>)> {
    // 空请求不能形成有效的DMA映射。
    if pages == 0 {
        return None;
    }
    // 计算分配字节数并预留一个DMA槽位。
    let size = pages.checked_mul(exo_abi::PAGE_SIZE as usize)?;
    let mut slots = DMA.lock();
    let index = slots.iter().position(|slot| slot.size == 0)?;
    // virtio使用DMA arena高4MiB，避免它自己的slot表与CrabUSB的独立
    // slot表选择相同EL0 VA。EL1仍会对每次DMA_ALLOC执行最终重叠校验。
    let mut va = exo_abi::DMA_ARENA_BASE + 8 * 1024 * 1024;
    loop {
        // 检查当前候选VA是否仍在DMA地址窗口内。
        let end = va.checked_add(size as u64)?;
        if end > exo_abi::DMA_ARENA_END {
            return None;
        }
        // 查找与已有DMA映射重叠的区间。
        let collision = slots
            .iter()
            .filter(|slot| slot.size != 0)
            .filter_map(|slot| {
                let slot_end = slot.va.checked_add(slot.size as u64)?;
                (va < slot_end && end > slot.va).then_some(slot_end)
            })
            .max();
        // 有冲突就把候选VA移到冲突区间之后。
        match collision {
            Some(next) => va = next,
            None => break,
        }
    }
    // 通过SVC申请连续物理页，并映射到候选EL0 VA。
    let pa = crate::runtime::dma_alloc(size, exo_abi::PAGE_SIZE as usize, va).ok()?;
    // 记录CPU VA、设备PA和大小，供释放及bounce回写校验。
    slots[index] = DmaSlot { va, pa, size };
    // 同时返回设备使用的PA和CPU访问的VA。
    Some((pa, NonNull::new(va as *mut u8)?))
}

fn release_dma(paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> bool {
    // 将页数转换为必须匹配的DMA字节数。
    let size = match pages.checked_mul(exo_abi::PAGE_SIZE as usize) {
        Some(size) => size,
        None => return false,
    };
    let va = vaddr.as_ptr() as u64;
    // 只接受与本地记录完全匹配的DMA所有者。
    let mut slots = DMA.lock();
    let Some(slot) = slots
        .iter_mut()
        .find(|slot| slot.pa == paddr && slot.va == va && slot.size == size)
    else {
        return false;
    };
    // 请求Kernel撤销DMA映射并释放连续物理页。
    if crate::runtime::dma_free(va, size).is_err() {
        return false;
    }
    // 清空槽位，允许后续DMA请求复用它。
    *slot = DmaSlot::EMPTY;
    true
}
