use core::{alloc::Layout, num::NonZeroUsize, ptr::NonNull, time::Duration};

use dma_api::{
    DmaAddr, DmaAllocHandle, DmaConstraints, DmaDirection, DmaError, DmaMapHandle, DmaOp,
};
use spin::Mutex;

const SLOT_COUNT: usize = 64;

#[derive(Clone, Copy)]
struct Slot {
    va: u64,
    size: usize,
}

impl Slot {
    const EMPTY: Self = Self { va: 0, size: 0 };
}

static SLOTS: Mutex<[Slot; SLOT_COUNT]> = Mutex::new([Slot::EMPTY; SLOT_COUNT]);

pub static USB_KERNEL: UsbKernel = UsbKernel;

pub struct UsbKernel;

impl UsbKernel {
    fn allocate(&self, constraints: DmaConstraints, layout: Layout) -> Option<DmaAllocHandle> {
        let size = page_round(layout.size().max(1))?;
        let alignment = layout
            .align()
            .max(constraints.align)
            .max(exo_abi::PAGE_SIZE as usize);
        if !alignment.is_power_of_two() || alignment > 2 * 1024 * 1024 {
            return None;
        }

        let mut slots = SLOTS.lock();
        let slot_index = slots.iter().position(|slot| slot.size == 0)?;
        let mut candidate = exo_abi::DMA_ARENA_BASE;
        loop {
            candidate = align_up(candidate, alignment as u64)?;
            let end = candidate.checked_add(size as u64)?;
            if end > exo_abi::DMA_ARENA_END {
                return None;
            }
            let mut collision_end = 0u64;
            for slot in slots.iter() {
                let slot_end = slot.va.saturating_add(slot.size as u64);
                if slot.size != 0 && candidate < slot_end && end > slot.va {
                    collision_end = collision_end.max(slot_end);
                }
            }
            if collision_end == 0 {
                break;
            }
            candidate = collision_end;
        }

        let dma = crate::runtime::dma_alloc(size, alignment, candidate).ok()?;
        if !constraints_satisfied(constraints, dma, size) {
            let _ = crate::runtime::dma_free(candidate, size);
            return None;
        }
        slots[slot_index] = Slot {
            va: candidate,
            size,
        };
        let pointer = NonNull::new(candidate as *mut u8)?;
        Some(unsafe { DmaAllocHandle::new(pointer, DmaAddr::from(dma), layout) })
    }

    fn release(&self, pointer: NonNull<u8>) {
        let va = pointer.as_ptr() as u64;
        let mut slots = SLOTS.lock();
        if let Some(slot) = slots
            .iter_mut()
            .find(|slot| slot.va == va && slot.size != 0)
        {
            let size = slot.size;
            if crate::runtime::dma_free(va, size).is_ok() {
                *slot = Slot::EMPTY;
            }
        }
    }
}

impl DmaOp for UsbKernel {
    fn page_size(&self) -> usize {
        exo_abi::PAGE_SIZE as usize
    }

    unsafe fn alloc_contiguous(
        &self,
        constraints: DmaConstraints,
        layout: Layout,
    ) -> Option<DmaAllocHandle> {
        self.allocate(constraints, layout)
    }

    unsafe fn dealloc_contiguous(&self, handle: DmaAllocHandle) {
        self.release(handle.as_ptr());
    }

    unsafe fn alloc_coherent(
        &self,
        constraints: DmaConstraints,
        layout: Layout,
    ) -> Option<DmaAllocHandle> {
        self.allocate(constraints, layout)
    }

    unsafe fn dealloc_coherent(&self, handle: DmaAllocHandle) {
        self.release(handle.as_ptr());
    }

    unsafe fn map_streaming(
        &self,
        constraints: DmaConstraints,
        address: NonNull<u8>,
        size: NonZeroUsize,
        _direction: DmaDirection,
    ) -> Result<DmaMapHandle, DmaError> {
        let layout = Layout::from_size_align(size.get(), 1)?;
        let bounce = self
            .allocate(constraints, layout)
            .ok_or(DmaError::NoMemory)?;
        Ok(unsafe { DmaMapHandle::new(address, bounce.dma_addr(), layout, Some(bounce.as_ptr())) })
    }

    unsafe fn unmap_streaming(&self, handle: DmaMapHandle) {
        if let Some(pointer) = handle.bounce_ptr() {
            self.release(pointer);
        }
    }

    // Kernel 将 DMA 页映射为 Normal Non-cacheable memory。这里仍保留内存屏障，
    // 但跳过 dma-api 默认的 cache flush/invalidate；默认实现会读取 CTR_EL0，
    // 并执行 DC IVAC/CIVAC，而本适配层不需要这些操作。
    fn sync_alloc_for_device(
        &self,
        _handle: &DmaAllocHandle,
        _offset: usize,
        _size: usize,
        _direction: DmaDirection,
    ) {
        mb();
    }

    fn sync_alloc_for_cpu(
        &self,
        _handle: &DmaAllocHandle,
        _offset: usize,
        _size: usize,
        _direction: DmaDirection,
    ) {
        mb();
    }

    fn sync_map_for_device(
        &self,
        handle: &DmaMapHandle,
        offset: usize,
        size: usize,
        direction: DmaDirection,
    ) {
        if matches!(
            direction,
            DmaDirection::ToDevice | DmaDirection::Bidirectional
        ) {
            if let Some(map_virt) = handle.bounce_ptr() {
                if map_virt != handle.as_ptr() {
                    unsafe {
                        map_virt
                            .add(offset)
                            .copy_from_nonoverlapping(handle.as_ptr().add(offset), size);
                    }
                }
            }
        }
        mb();
    }

    fn sync_map_for_cpu(
        &self,
        handle: &DmaMapHandle,
        offset: usize,
        size: usize,
        direction: DmaDirection,
    ) {
        if matches!(
            direction,
            DmaDirection::FromDevice | DmaDirection::Bidirectional
        ) {
            if let Some(map_virt) = handle.bounce_ptr() {
                if map_virt != handle.as_ptr() {
                    unsafe {
                        handle
                            .as_ptr()
                            .add(offset)
                            .copy_from_nonoverlapping(map_virt.add(offset), size);
                    }
                }
            }
        }
        mb();
    }
}

impl crab_usb::KernelOp for UsbKernel {
    fn delay(&self, duration: Duration) {
        crate::runtime::delay_ns(duration.as_nanos().min(u64::MAX as u128) as u64);
    }
}

fn constraints_satisfied(constraints: DmaConstraints, address: u64, size: usize) -> bool {
    let Some(last) = address.checked_add(size.saturating_sub(1) as u64) else {
        return false;
    };
    if (address | last) & !constraints.addr_mask != 0
        || address & (constraints.align.max(1) as u64 - 1) != 0
        || constraints
            .max_segment_size
            .is_some_and(|maximum| size > maximum)
    {
        return false;
    }
    if let Some(boundary) = constraints.boundary {
        if boundary != 0 && address / boundary as u64 != last / boundary as u64 {
            return false;
        }
    }
    true
}

fn page_round(value: usize) -> Option<usize> {
    value
        .checked_add(exo_abi::PAGE_SIZE as usize - 1)
        .map(|value| value & !(exo_abi::PAGE_SIZE as usize - 1))
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

#[inline]
fn mb() {
    unsafe { core::arch::asm!("dmb osh", options(nostack, preserves_flags)) };
}
