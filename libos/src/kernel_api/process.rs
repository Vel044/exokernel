//! 用户态 ProcessBuilder。
//!
//! 这里实现的是“组合 Kernel 原语”的策略层：Kernel 只提供 VSpace、Frame、
//! Mapping 和 Suspended Thread；ELF 段如何解析、临时映射到哪里、失败后如何
//! 回滚，都由 libOS 决定。这样创建一个进程不需要为每个寄存器访问进入 EL1。

use alloc::vec::Vec;

use super::{frame, thread, vspace};

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PF_W: u32 = 2;
const PF_X: u32 = 1;
const PH_SIZE: usize = 56;

#[derive(Clone, Copy)]
struct LoadSegment {
    offset: u64,
    va: u64,
    filesz: u64,
    memsz: u64,
    flags: u32,
}

pub struct ProcessBuilder<'a> {
    image: &'a [u8],
}

pub struct Process {
    vspace: vspace::VSpace,
    main_thread: thread::Thread,
    frames: Vec<frame::Frame>,
    mappings: Vec<frame::Mapping>,
}

impl<'a> ProcessBuilder<'a> {
    pub fn new(image: &'a [u8]) -> Result<Self, u64> {
        // 先做不会触碰 Kernel 资源的 ELF 头校验，让错误尽早返回。
        parse_header(image)?;
        Ok(Self { image })
    }

    pub fn build(self) -> Result<Process, u64> {
        let (entry, segments) = parse_load_segments(self.image)?;
        let target = vspace::VSpace::create()?;
        let target_handle = target.handle();
        let mut frames = Vec::new();
        let mut mappings = Vec::new();

        let result = (|| {
            for (index, segment) in segments.iter().enumerate() {
                let pages = pages_for(segment.memsz, segment.va & (exo_abi::PAGE_SIZE - 1))?;
                let object = frame::Frame::allocate(pages, 1)?;
                // 临时 RW 映射只存在于当前 VSpace，便于把 ELF 文件内容复制到
                // Frame。复制结束后先撤销 RW，再以最终 RX/RO/RW 权限映射给目标。
                let temp_va = exo_abi::FRAME_ARENA_BASE
                    + (index as u64 + 1) * 0x0200_0000;
                let temp = object.map(0, pages, temp_va, frame::Rights::READ_WRITE)?;
                unsafe {
                    let dst = temp.as_ptr().add((segment.va & 0xfff) as usize);
                    let file_start = segment.offset as usize;
                    core::ptr::copy_nonoverlapping(
                        self.image.as_ptr().add(file_start),
                        dst,
                        segment.filesz as usize,
                    );
                    core::ptr::write_bytes(
                        dst.add(segment.filesz as usize),
                        0,
                        (segment.memsz - segment.filesz) as usize,
                    );
                }
                temp.unmap()?;
                let rights = if segment.flags & PF_W != 0 {
                    frame::Rights::READ_WRITE
                } else if segment.flags & PF_X != 0 {
                    frame::Rights::READ_EXECUTE
                } else {
                    frame::Rights::READ
                };
                let mapped = object.map_to(
                    &target,
                    0,
                    pages,
                    segment.va & !(exo_abi::PAGE_SIZE - 1),
                    rights,
                )?;
                frames.push(object);
                mappings.push(mapped);
            }

            let stack_pages = exo_abi::USER_STACK_PAGES;
            let stack = frame::Frame::allocate(stack_pages, 1)?;
            let stack_temp_va = exo_abi::FRAME_ARENA_BASE + 0x0f00_0000;
            let stack_temp = stack.map(
                0,
                stack_pages,
                stack_temp_va,
                frame::Rights::READ_WRITE,
            )?;
            unsafe { core::ptr::write_bytes(stack_temp.as_ptr(), 0, stack_temp.len()) };
            stack_temp.unmap()?;
            let stack_va = exo_abi::USER_STACK_TOP - stack_pages * exo_abi::PAGE_SIZE;
            let stack_mapping = stack.map_to(
                &target,
                0,
                stack_pages,
                stack_va,
                frame::Rights::READ_WRITE,
            )?;
            frames.push(stack);
            mappings.push(stack_mapping);

            let main_thread = thread::Thread::spawn_in(
                &target,
                entry,
                exo_abi::USER_STACK_TOP,
                0,
                0,
                thread::ThreadConfig::new(0, 32, 63),
            )?;
            Ok::<_, u64>(main_thread)
        })();

        match result {
            Ok(main_thread) => Ok(Process {
                vspace: target,
                main_thread,
                frames,
                mappings,
            }),
            Err(error) => {
                // 事务失败时必须先撤销 Mapping，再释放 Frame，最后销毁 VSpace。
                for mapping in mappings.drain(..) {
                    let _ = mapping.unmap();
                }
                for object in frames.drain(..) {
                    let _ = object.free();
                }
                let _ = crate::runtime::vspace_destroy(target_handle);
                Err(error)
            }
        }
    }
}

impl Process {
    pub fn start(&self) -> Result<(), u64> {
        self.main_thread.start()
    }

    /// 释放一个已经停止的进程。正在运行的线程由 Kernel 返回 BUSY，调用者
    /// 不能绕过这个检查强行释放它正在使用的页表。
    pub fn destroy(mut self) -> Result<(), u64> {
        self.main_thread.destroy()?;
        while let Some(mapping) = self.mappings.pop() {
            mapping.unmap()?;
        }
        while let Some(object) = self.frames.pop() {
            object.free()?;
        }
        self.vspace.destroy()
    }
}

fn parse_header(image: &[u8]) -> Result<(), u64> {
    if image.len() < 64
        || image[0..4] != ELF_MAGIC
        || image[4] != ELFCLASS64
        || image[5] != ELFDATA2LSB
        || read_u16(image, 18)? != EM_AARCH64
    {
        return Err(exo_abi::SYS_ERR_INVALID);
    }
    Ok(())
}

fn parse_load_segments(image: &[u8]) -> Result<(u64, Vec<LoadSegment>), u64> {
    parse_header(image)?;
    let entry = read_u64_at(image, 24)?;
    let phoff = read_u64_at(image, 32)? as usize;
    let phentsize = read_u16(image, 54)? as usize;
    let phnum = read_u16(image, 56)? as usize;
    if phentsize < PH_SIZE || phnum == 0 {
        return Err(exo_abi::SYS_ERR_INVALID);
    }
    let table_size = phentsize
        .checked_mul(phnum)
        .ok_or(exo_abi::SYS_ERR_INVALID)?;
    let table_end = phoff
        .checked_add(table_size)
        .ok_or(exo_abi::SYS_ERR_INVALID)?;
    if table_end > image.len() {
        return Err(exo_abi::SYS_ERR_INVALID);
    }
    let mut segments = Vec::new();
    for index in 0..phnum {
        let base = phoff + index * phentsize;
        if read_u32_at(image, base)? != PT_LOAD {
            continue;
        }
        let flags = read_u32_at(image, base + 4)?;
        let offset = read_u64_at(image, base + 8)?;
        let va = read_u64_at(image, base + 16)?;
        let filesz = read_u64_at(image, base + 32)?;
        let memsz = read_u64_at(image, base + 40)?;
        if memsz == 0
            || filesz > memsz
            || offset.checked_add(filesz).ok_or(exo_abi::SYS_ERR_INVALID)?
                > image.len() as u64
            || va.checked_add(memsz).ok_or(exo_abi::SYS_ERR_INVALID)?
                > exo_abi::USER_BOOT_INFO_VA
            || va < exo_abi::USER_BASE
            || (flags & PF_W != 0 && flags & PF_X != 0)
        {
            return Err(exo_abi::SYS_ERR_DENIED);
        }
        segments.push(LoadSegment {
            offset,
            va,
            filesz,
            memsz,
            flags,
        });
    }
    // 第一版使用固定 Frame 临时窗口，每个窗口 32MiB；保留最后一个窗口给
    // stack，因此限制段数量，避免临时映射彼此覆盖。
    if segments.is_empty()
        || segments.len() > 7
        || !segments.iter().any(|segment| segment.flags & PF_X != 0)
    {
        return Err(exo_abi::SYS_ERR_INVALID);
    }
    Ok((entry, segments))
}

fn pages_for(bytes: u64, offset: u64) -> Result<u64, u64> {
    let total = offset.checked_add(bytes).ok_or(exo_abi::SYS_ERR_INVALID)?;
    let pages = (total + exo_abi::PAGE_SIZE - 1) / exo_abi::PAGE_SIZE;
    if pages == 0 || pages > 4096 {
        return Err(exo_abi::SYS_ERR_INVALID);
    }
    Ok(pages)
}

fn read_u16(image: &[u8], offset: usize) -> Result<u16, u64> {
    Ok(u16::from_le_bytes([
        *image.get(offset).ok_or(exo_abi::SYS_ERR_INVALID)?,
        *image.get(offset + 1).ok_or(exo_abi::SYS_ERR_INVALID)?,
    ]))
}

fn read_u32_at(image: &[u8], offset: usize) -> Result<u32, u64> {
    Ok(u32::from_le_bytes([
        *image.get(offset).ok_or(exo_abi::SYS_ERR_INVALID)?,
        *image.get(offset + 1).ok_or(exo_abi::SYS_ERR_INVALID)?,
        *image.get(offset + 2).ok_or(exo_abi::SYS_ERR_INVALID)?,
        *image.get(offset + 3).ok_or(exo_abi::SYS_ERR_INVALID)?,
    ]))
}

fn read_u64_at(image: &[u8], offset: usize) -> Result<u64, u64> {
    Ok(u64::from_le_bytes([
        *image.get(offset).ok_or(exo_abi::SYS_ERR_INVALID)?,
        *image.get(offset + 1).ok_or(exo_abi::SYS_ERR_INVALID)?,
        *image.get(offset + 2).ok_or(exo_abi::SYS_ERR_INVALID)?,
        *image.get(offset + 3).ok_or(exo_abi::SYS_ERR_INVALID)?,
        *image.get(offset + 4).ok_or(exo_abi::SYS_ERR_INVALID)?,
        *image.get(offset + 5).ok_or(exo_abi::SYS_ERR_INVALID)?,
        *image.get(offset + 6).ok_or(exo_abi::SYS_ERR_INVALID)?,
        *image.get(offset + 7).ok_or(exo_abi::SYS_ERR_INVALID)?,
    ]))
}
