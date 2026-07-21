//! 最小 ELF64/AArch64 loader，只接受页对齐、互不重叠的 PT_LOAD。

use crate::{mem, mmu, task};

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const MAX_SEGMENTS: usize = 8;

pub struct LoadedElf {
    pub entry: u64,
    pub segments: [task::OwnedPages; MAX_SEGMENTS],
    pub segment_count: usize,
}

pub fn load(root: u64, image: &[u8]) -> Result<LoadedElf, &'static str> {
    if image.len() < 64 || &image[0..4] != ELF_MAGIC || image[4] != 2 || image[5] != 1 {
        return Err("invalid ELF64 image");
    }
    let machine = read_u16(image, 18)?;
    if machine != 183 {
        return Err("ELF is not AArch64");
    }

    let entry = read_u64(image, 24)?;
    let phoff = read_u64(image, 32)? as usize;
    let phentsize = read_u16(image, 54)? as usize;
    let phnum = read_u16(image, 56)? as usize;
    if phentsize < 56 {
        return Err("invalid program header size");
    }

    let mut result = LoadedElf {
        entry,
        segments: [task::OwnedPages::EMPTY; MAX_SEGMENTS],
        segment_count: 0,
    };

    for index in 0..phnum {
        let off = phoff
            .checked_add(
                index
                    .checked_mul(phentsize)
                    .ok_or("program header overflow")?,
            )
            .ok_or("program header overflow")?;
        if read_u32(image, off)? != PT_LOAD {
            continue;
        }
        if result.segment_count == MAX_SEGMENTS {
            return Err("too many PT_LOAD segments");
        }

        let flags = read_u32(image, off + 4)?;
        if (flags & PF_X) != 0 && (flags & PF_W) != 0 {
            return Err("writable executable PT_LOAD rejected");
        }
        let file_off = read_u64(image, off + 8)? as usize;
        let vaddr = read_u64(image, off + 16)?;
        let filesz = read_u64(image, off + 32)?;
        let memsz = read_u64(image, off + 40)?;
        if memsz == 0 {
            continue;
        }
        if (vaddr & 0xfff) != 0 || filesz > memsz {
            return Err("PT_LOAD must be page aligned");
        }
        let end = vaddr.checked_add(memsz).ok_or("segment overflow")?;
        if vaddr < exo_abi::USER_BASE || end > exo_abi::USER_BOOT_INFO_VA {
            return Err("PT_LOAD outside code window");
        }
        for segment in &result.segments[..result.segment_count] {
            let segment_end = segment.va + segment.pages * 4096;
            if vaddr < segment_end && end > segment.va {
                return Err("overlapping PT_LOAD segments");
            }
        }
        let file_end = file_off
            .checked_add(filesz as usize)
            .ok_or("segment file overflow")?;
        if file_end > image.len() {
            return Err("truncated PT_LOAD");
        }

        let pages = (memsz + 4095) / 4096;
        let pa = mem::alloc_pages(pages).ok_or("no memory for PT_LOAD")?;
        unsafe {
            core::ptr::write_bytes(pa as *mut u8, 0, (pages * 4096) as usize);
            core::ptr::copy_nonoverlapping(
                image.as_ptr().add(file_off),
                pa as *mut u8,
                filesz as usize,
            );
        }
        let map_flags = if (flags & PF_X) != 0 {
            mmu::MMU_USER_RX
        } else if (flags & PF_W) != 0 {
            mmu::MMU_USER_RW
        } else {
            mmu::MMU_USER_RO
        };
        mmu::map(root, vaddr, pa, map_flags, pages);
        result.segments[result.segment_count] = task::OwnedPages {
            va: vaddr,
            pa,
            pages,
            executable: (flags & PF_X) != 0,
        };
        result.segment_count += 1;
    }

    if result.segment_count == 0 {
        return Err("ELF has no PT_LOAD");
    }
    Ok(result)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
    let data = bytes.get(offset..offset + 2).ok_or("truncated ELF")?;
    Ok(u16::from_le_bytes([data[0], data[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
    let data = bytes.get(offset..offset + 4).ok_or("truncated ELF")?;
    Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, &'static str> {
    let data = bytes.get(offset..offset + 8).ok_or("truncated ELF")?;
    Ok(u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]))
}
