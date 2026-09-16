//! UVC实验使用的受限ext4捕获文件写入器。
//!
//! `ext4-view`只负责只读路径解析，不能修改inode。宿主创建镜像时已经为
//! `/uvc-frame.mjpg`分配连续extent，并在ext4保留的第0扇区写入extent位置。
//! 本模块只覆盖这段已有文件数据，再在第0扇区记录有效长度；它不分配块、
//! 不修改目录、inode、位图或journal，因此不是通用ext4写实现。

use alloc::vec;

use virtio_drivers::device::blk::SECTOR_SIZE;

use crate::drivers::virtio_blk::{BlockDevice, BlockDevices};

const CAPTURE_MAGIC: &[u8; 8] = b"EXOUVC01";
const OBSERVATION_MAGIC: &[u8; 8] = b"EXOOBS01";
const START_LBA_OFFSET: usize = 8;
const CAPACITY_OFFSET: usize = 16;
const VALID_LENGTH_OFFSET: usize = 24;
const WRITE_CHUNK: usize = 64 * 1024;

pub(crate) struct CaptureVolume {
    device: BlockDevice,
    start_lba: usize,
    capacity: usize,
}

/// ACT固定观测卷使用的受限写入器。宿主预先创建一个连续的
/// `/observation.bin`；EL0只覆盖其数据区，不修改任何ext4元数据。
pub(crate) struct ObservationVolume {
    device: BlockDevice,
    start_lba: usize,
    capacity: usize,
}

const OBSERVATION_HEADER_SIZE: usize = 4096;
const OBSERVATION_IMAGE_CAPACITY: usize = 2 * 1024 * 1024;
const OBSERVATION_FIXED_OFFSET: usize = OBSERVATION_HEADER_SIZE + OBSERVATION_IMAGE_CAPACITY;

impl CaptureVolume {
    /// 扫描Kernel授权的virtio-mmio窗口，找到带有EXOUVC01元数据的ext4盘。
    pub(crate) fn discover(info: &exo_abi::UserBootInfo) -> Result<Self, u64> {
        let mut devices = BlockDevices::discover(info).map_err(|_| 0x570u64)?;
        while let Some(mut device) = devices.next() {
            let mut sector = [0u8; SECTOR_SIZE];
            if device.read_blocks(0, &mut sector).is_err() || &sector[..8] != CAPTURE_MAGIC {
                continue;
            }
            let start_lba = read_u64(&sector, START_LBA_OFFSET) as usize;
            let capacity = read_u64(&sector, CAPACITY_OFFSET) as usize;
            if start_lba == 0 || capacity == 0 || capacity % SECTOR_SIZE != 0 {
                return Err(0x571);
            }
            return Ok(Self {
                device,
                start_lba,
                capacity,
            });
        }
        Err(0x572)
    }

    /// 覆盖预分配文件的开头并记录精确有效长度。
    pub(crate) fn write_frame(&mut self, frame: &[u8]) -> Result<(), u64> {
        if frame.is_empty() || frame.len() > self.capacity {
            return Err(0x573);
        }

        let mut offset = 0usize;
        while offset < frame.len() {
            let payload = (frame.len() - offset).min(WRITE_CHUNK);
            let padded = payload.div_ceil(SECTOR_SIZE) * SECTOR_SIZE;
            let mut sectors = vec![0u8; padded];
            sectors[..payload].copy_from_slice(&frame[offset..offset + payload]);
            // write_blocks构造virtqueue描述符；Hal把普通Cacheable Vec复制到
            // Kernel分配的连续DMA页，QEMU virtio-blk再把这些扇区写入镜像。
            self.device
                .write_blocks(self.start_lba + offset / SECTOR_SIZE, &sectors)
                .map_err(|_| 0x574u64)?;
            offset += payload;
        }

        // 第0扇区不属于ext4元数据。记录长度后，Mac无需扫描JPEG尾标记即可
        // 从预分配文件中精确截取本次照片。
        let mut metadata = [0u8; SECTOR_SIZE];
        self.device
            .read_blocks(0, &mut metadata)
            .map_err(|_| 0x575u64)?;
        if &metadata[..8] != CAPTURE_MAGIC {
            return Err(0x576);
        }
        metadata[VALID_LENGTH_OFFSET..VALID_LENGTH_OFFSET + 8]
            .copy_from_slice(&(frame.len() as u64).to_le_bytes());
        self.device
            .write_blocks(0, &metadata)
            .map_err(|_| 0x577u64)?;
        self.device.flush().map_err(|_| 0x578u64)
    }
}

impl ObservationVolume {
    pub(crate) fn discover(info: &exo_abi::UserBootInfo) -> Result<Self, u64> {
        let mut devices = BlockDevices::discover(info).map_err(|_| 0x580u64)?;
        while let Some(mut device) = devices.next() {
            let mut sector = [0u8; SECTOR_SIZE];
            if device.read_blocks(0, &mut sector).is_err() || &sector[..8] != OBSERVATION_MAGIC {
                continue;
            }
            let start_lba = read_u64(&sector, START_LBA_OFFSET) as usize;
            let capacity = read_u64(&sector, CAPACITY_OFFSET) as usize;
            let required = OBSERVATION_FIXED_OFFSET + OBSERVATION_IMAGE_CAPACITY;
            if start_lba == 0 || capacity < required || capacity % SECTOR_SIZE != 0 {
                return Err(0x581);
            }
            return Ok(Self {
                device,
                start_lba,
                capacity,
            });
        }
        Err(0x582)
    }

    /// 提交一份不可变ACT观测。先写两张JPEG，最后写header作为提交记录；因此
    /// 宿主只有看到有效header后才会接受本轮数据，不会误读半写入镜像。
    pub(crate) fn write_observation(
        &mut self,
        handeye: &[u8],
        fixed: &[u8],
        state: [f32; 6],
    ) -> Result<(), u64> {
        if handeye.is_empty()
            || fixed.is_empty()
            || handeye.len() > OBSERVATION_IMAGE_CAPACITY
            || fixed.len() > OBSERVATION_IMAGE_CAPACITY
        {
            return Err(0x583);
        }
        self.write_at(OBSERVATION_HEADER_SIZE, handeye)?;
        self.write_at(OBSERVATION_FIXED_OFFSET, fixed)?;

        let mut header = [0u8; OBSERVATION_HEADER_SIZE];
        header[..8].copy_from_slice(b"ACTOBS01");
        header[8..12].copy_from_slice(&1u32.to_le_bytes());
        header[12..16].copy_from_slice(&(handeye.len() as u32).to_le_bytes());
        header[16..20].copy_from_slice(&(fixed.len() as u32).to_le_bytes());
        header[20..24].copy_from_slice(&6u32.to_le_bytes());
        for (index, value) in state.into_iter().enumerate() {
            let offset = 24 + index * 4;
            header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        self.write_at(0, &header)?;
        self.device.flush().map_err(|_| 0x584u64)
    }

    fn write_at(&mut self, file_offset: usize, bytes: &[u8]) -> Result<(), u64> {
        if file_offset % SECTOR_SIZE != 0
            || file_offset.checked_add(bytes.len()).ok_or(0x585u64)? > self.capacity
        {
            return Err(0x585);
        }
        let mut offset = 0usize;
        while offset < bytes.len() {
            let payload = (bytes.len() - offset).min(WRITE_CHUNK);
            let padded = payload.div_ceil(SECTOR_SIZE) * SECTOR_SIZE;
            let mut sectors = vec![0u8; padded];
            sectors[..payload].copy_from_slice(&bytes[offset..offset + payload]);
            self.device
                .write_blocks(
                    self.start_lba + (file_offset + offset) / SECTOR_SIZE,
                    &sectors,
                )
                .map_err(|_| 0x586u64)?;
            offset += payload;
        }
        Ok(())
    }
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    data.get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}
