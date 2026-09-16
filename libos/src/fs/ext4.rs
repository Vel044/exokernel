//! 只读ext4文件系统与大文件Frame装载器。

use alloc::{boxed::Box, vec::Vec};
use core::{error::Error, fmt, ptr::NonNull};

use ext4_view::{Ext4, Ext4Read};
use virtio_drivers::device::blk::SECTOR_SIZE;

use crate::{
    drivers::virtio_blk::{BlockDevice, BlockDevices},
    memory::{Frame, Mapping, Rights},
};

const MAX_FRAME_PAGES: u64 = 4096;

#[derive(Clone, Copy, Debug)]
pub enum FsError {
    NoBlockDevice,
    NoExt4,
    InvalidFile,
    TooLarge,
    NoMemory,
    Io,
}

/// 已挂载的只读ext4实例。
pub struct FileSystem {
    // 持有第三方ext4解析状态和底层块设备读取器。
    inner: Ext4,
}

impl FileSystem {
    /// 扫描Kernel授权窗口中的块设备，选择第一个能通过ext4 superblock校验的盘。
    pub fn mount(info: &exo_abi::UserBootInfo) -> Result<Self, FsError> {
        // 枚举Kernel授权的virtio-blk设备。
        let mut devices = BlockDevices::discover(info).map_err(|_| FsError::NoBlockDevice)?;
        // 记录是否找到过块设备。
        let mut saw_block = false;
        // 逐个尝试把块设备解析成ext4。
        while let Some(device) = devices.next() {
            saw_block = true;
            // 把按扇区读的块设备适配成ext4的读取接口。
            let reader = BlockReader { device };
            // 读取并校验ext4元数据，成功后返回文件系统对象。
            if let Ok(inner) = Ext4::load(Box::new(reader)) {
                return Ok(Self { inner });
            }
        }
        // 根据失败原因区分没有设备和没有有效ext4。
        Err(if saw_block {
            FsError::NoExt4
        } else {
            FsError::NoBlockDevice
        })
    }

    /// 打开文件并直接读入一组在EL0 VA连续、PA可分段的普通Frame。
    pub fn read_mapped(
        &self,
        path: &str,
        user_va: u64,
        maximum_size: u64,
    ) -> Result<MappedFile, FsError> {
        // 通过ext4目录查找目标文件。
        let mut file = self.inner.open(path).map_err(|_| FsError::InvalidFile)?;
        // 读取文件长度，后续按这个长度申请Frame和映射。
        let length = file.metadata().len();
        // 拒绝空文件、超限文件和当前平台无法索引的文件。
        if length == 0 || length > maximum_size || length > usize::MAX as u64 {
            return Err(FsError::TooLarge);
        }
        // 为文件内容申请普通Frame并映射到指定的EL0虚拟地址。
        let mut mapped = MappedFile::allocate(user_va, length)?;
        // 循环读取，直到文件内容全部写入映射区域。
        let mut written = 0usize;
        while written < length as usize {
            // 把ext4的字节读取结果写入文件对应的Frame。
            let count = file
                .read_bytes(&mut mapped.as_mut_slice()[written..])
                .map_err(|_| FsError::Io)?;
            // 防止底层返回零字节导致读取循环无法结束。
            if count == 0 {
                return Err(FsError::Io);
            }
            // 累计已经写入的文件字节数。
            written += count;
        }
        // 返回同时持有Frame和Mapping句柄的文件副本。
        Ok(mapped)
    }
}

/// 一个文件在普通Frame中的完整副本。
pub struct MappedFile {
    // 持有文件内容所占的Frame，生命周期覆盖所有文件切片。
    frames: Vec<Frame>,
    // 持有Frame到EL0 VA的映射，销毁时必须先解除映射。
    mappings: Vec<Mapping>,
    // 文件内容在EL0地址空间中的起始VA。
    pointer: NonNull<u8>,
    // 文件内容的有效字节数，不包含末尾页的填充空间。
    length: usize,
}

impl MappedFile {
    fn allocate(user_va: u64, length: u64) -> Result<Self, FsError> {
        // 文件映射起始VA必须按页对齐。
        if user_va & (exo_abi::PAGE_SIZE - 1) != 0 {
            return Err(FsError::InvalidFile);
        }
        // 计算覆盖文件所需的页数和映射结束地址。
        let pages = length.div_ceil(exo_abi::PAGE_SIZE);
        let end = user_va
            .checked_add(
                pages
                    .checked_mul(exo_abi::PAGE_SIZE)
                    .ok_or(FsError::TooLarge)?,
            )
            .ok_or(FsError::TooLarge)?;
        // 确保映射落在专用的普通Frame地址窗口内。
        if user_va < exo_abi::FRAME_ARENA_BASE || end > exo_abi::FRAME_ARENA_END {
            return Err(FsError::TooLarge);
        }

        // 按最大批次预留Frame和Mapping句柄数组。
        let mut frames = Vec::with_capacity(pages.div_ceil(MAX_FRAME_PAGES) as usize);
        let mut mappings = Vec::with_capacity(frames.capacity());
        // 逐批申请Frame并映射到连续的EL0 VA。
        let mut remaining = pages;
        let mut va = user_va;
        while remaining != 0 {
            // 单批大小受限，避免一次请求过大。
            let count = remaining.min(MAX_FRAME_PAGES);
            // 向Kernel申请普通物理Frame。
            let frame = Frame::allocate(count, 1).map_err(|_| FsError::NoMemory)?;
            // 把Frame映射为当前libOS可读写的普通内存。
            let mapping = match frame.map(0, count, va, Rights::READ_WRITE) {
                Ok(mapping) => mapping,
                Err(_) => {
                    // 当前批次映射失败时立即释放已申请资源。
                    let _ = frame.free();
                    cleanup(frames, mappings);
                    return Err(FsError::NoMemory);
                }
            };
            // 保存句柄，保证文件对象销毁时能回收资源。
            frames.push(frame);
            mappings.push(mapping);
            // 移动到下一批Frame的目标VA。
            va += count * exo_abi::PAGE_SIZE;
            remaining -= count;
        }
        // 用首地址和有效长度构造文件内容视图。
        Ok(Self {
            frames,
            mappings,
            pointer: NonNull::new(user_va as *mut u8).ok_or(FsError::NoMemory)?,
            length: length as usize,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        // 将已经建立的EL0映射借用为只读字节切片。
        // SAFETY:Frame映射覆盖pointer起始的length字节，MappedFile持有全部
        // Mapping句柄，共享借用期间无法通过本类型取得可写切片。
        unsafe { core::slice::from_raw_parts(self.pointer.as_ptr(), self.length) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // 将已经建立的EL0映射借用为可写字节切片。
        // SAFETY:`&mut self`保证读取文件期间不存在本对象派生的其他切片。
        unsafe { core::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.length) }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        // 取出句柄并按映射后释放的顺序回收文件资源。
        let frames = core::mem::take(&mut self.frames);
        let mappings = core::mem::take(&mut self.mappings);
        cleanup(frames, mappings);
    }
}

fn cleanup(frames: Vec<Frame>, mappings: Vec<Mapping>) {
    // 先撤销所有用户虚拟地址映射。
    for mapping in mappings.into_iter().rev() {
        let _ = mapping.unmap();
    }
    // 再释放对应的物理Frame。
    for frame in frames.into_iter().rev() {
        let _ = frame.free();
    }
}

/// 把ext4任意字节读取转换成virtio-blk的512字节扇区请求。
struct BlockReader {
    // 持有当前用于读取ext4元数据和文件内容的块设备驱动。
    device: BlockDevice,
}

impl Ext4Read for BlockReader {
    fn read(
        &mut self,
        start_byte: u64,
        mut destination: &mut [u8],
    ) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
        // 先检查字节范围计算是否溢出。
        let end = start_byte
            .checked_add(destination.len() as u64)
            .ok_or_else(boxed_io_error)?;
        // 拒绝超出块设备容量的读取。
        if end > self.device.capacity().saturating_mul(SECTOR_SIZE as u64) {
            return Err(boxed_io_error());
        }
        // 保存当前扇区偏移，并准备处理非对齐读。
        let mut offset = start_byte;
        let mut scratch = [0u8; SECTOR_SIZE];

        // 计算起始地址在扇区内的偏移。
        let prefix = (offset as usize) % SECTOR_SIZE;
        if prefix != 0 && !destination.is_empty() {
            // 读取起始扇区，再截取请求范围中的前半段。
            self.device
                .read_blocks((offset as usize) / SECTOR_SIZE, &mut scratch)
                .map_err(|_| boxed_io_error())?;
            let count = destination.len().min(SECTOR_SIZE - prefix);
            destination[..count].copy_from_slice(&scratch[prefix..prefix + count]);
            destination = &mut destination[count..];
            offset += count as u64;
        }

        // 中间完整扇区直接交给virtio，DMA结果再复制回目标Frame。
        let full = destination.len() / SECTOR_SIZE * SECTOR_SIZE;
        if full != 0 {
            // 一次读取所有连续的完整扇区。
            self.device
                .read_blocks((offset as usize) / SECTOR_SIZE, &mut destination[..full])
                .map_err(|_| boxed_io_error())?;
            destination = &mut destination[full..];
            offset += full as u64;
        }

        if !destination.is_empty() {
            // 读取结尾扇区，再截取请求范围中的剩余字节。
            self.device
                .read_blocks((offset as usize) / SECTOR_SIZE, &mut scratch)
                .map_err(|_| boxed_io_error())?;
            destination.copy_from_slice(&scratch[..destination.len()]);
        }
        // 当前请求的所有字节都已写入目标缓冲区。
        Ok(())
    }
}

// 将底层块设备错误统一转换成ext4读取错误。
#[derive(Debug)]
struct BlockIoError;

impl fmt::Display for BlockIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("virtio block read failed")
    }
}

impl Error for BlockIoError {}

// 创建可跨越第三方库边界的块读取错误对象。
fn boxed_io_error() -> Box<dyn Error + Send + Sync + 'static> {
    Box::new(BlockIoError)
}
