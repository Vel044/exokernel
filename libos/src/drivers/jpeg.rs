//! 用户态MJPEG解码适配层。
//!
//! UVC只负责把压缩payload交给这里；JPEG decoder把结果直接写入Frame映射的
//! RGB缓冲。模型读取的是这块EL0 VA，因此整个过程不复制到固定global heap。

use crate::memory::{Frame, Mapping, Rights};
use zune_jpeg::{
    zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions},
    JpegDecoder,
};

pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 360;
pub const RGB_BYTES: usize = WIDTH * HEIGHT * 3;

/// 一张解码后的RGB图像及其Frame所有权。
pub(crate) struct RgbFrame {
    _backing: Option<Frame>,
    mapping: Option<Mapping>,
}

impl RgbFrame {
    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: mapping覆盖固定640x360x3字节，且self持有Frame和Mapping。
        unsafe { core::slice::from_raw_parts(self.mapping.as_ref().unwrap().as_ptr(), RGB_BYTES) }
    }

    /// 推理完成后撤销RGB映射并释放普通Frame。
    pub(crate) fn release(self) -> Result<(), u64> {
        let mut this = self;
        let mapping = this.mapping.take().unwrap();
        let backing = this._backing.take().unwrap();
        let result = mapping.unmap();
        if result.is_ok() {
            backing.free()
        } else {
            result
        }
    }
}

impl Drop for RgbFrame {
    fn drop(&mut self) {
        if let Some(mapping) = self.mapping.take() {
            let unmapped = mapping.unmap().is_ok();
            if unmapped {
                if let Some(backing) = self._backing.take() {
                    let _ = backing.free();
                }
            }
        }
    }
}

/// 把UVC收到的MJPEG解码为模型需要的640x360 RGB HWC布局。
pub(crate) fn decode_mjpeg(jpeg: &[u8], user_va: u64) -> Result<RgbFrame, u64> {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(jpeg), options);
    decoder.decode_headers().map_err(|_| 0x610u64)?;
    let image = decoder.info().ok_or(0x611u64)?;
    if usize::from(image.width) != WIDTH || usize::from(image.height) != HEIGHT {
        return Err(0x612);
    }
    if decoder.output_buffer_size() != Some(RGB_BYTES) {
        return Err(0x613);
    }

    let pages = RGB_BYTES.div_ceil(exo_abi::PAGE_SIZE as usize) as u64;
    let backing = Frame::allocate(pages, 1).map_err(|_| 0x614u64)?;
    let mapping = match backing.map(0, pages, user_va, Rights::READ_WRITE) {
        Ok(mapping) => mapping,
        Err(error) => {
            let _ = backing.free();
            return Err(error);
        }
    };
    // SAFETY: Kernel已检查VA、页数、PTE为空和RW权限；decoder只在本函数内写入。
    let output = unsafe { core::slice::from_raw_parts_mut(mapping.as_ptr(), RGB_BYTES) };
    if let Err(_) = decoder.decode_into(output) {
        let _ = mapping.unmap();
        let _ = backing.free();
        return Err(0x615);
    }
    Ok(RgbFrame {
        _backing: Some(backing),
        mapping: Some(mapping),
    })
}
