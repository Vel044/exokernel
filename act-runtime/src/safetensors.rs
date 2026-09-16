//! safetensors只读、零拷贝解析器。
//!
//! safetensors文件由`u64 header_len + JSON header + raw tensor data`组成。
//! 本解析器只实现ACT权重需要的F32和整数shape/offset字段，不在EL0堆中
//! 构造234项HashMap；每次按名字扫描约25KiB header，随后直接借用原始权重。

use crate::Error;

const MAX_DIMS: usize = 4;

pub struct SafeTensors<'a> {
    file: &'a [u8],
    header: &'a [u8],
    data_start: usize,
}

#[derive(Clone, Copy)]
pub struct Tensor<'a> {
    bytes: &'a [u8],
    shape: [usize; MAX_DIMS],
    rank: usize,
}

impl<'a> SafeTensors<'a> {
    pub fn parse(file: &'a [u8]) -> Result<Self, Error> {
        if file.len() < 8 {
            return Err(Error::InvalidSafetensors);
        }
        let header_len = u64::from_le_bytes(file[..8].try_into().unwrap()) as usize;
        let data_start = 8usize
            .checked_add(header_len)
            .ok_or(Error::InvalidSafetensors)?;
        if header_len < 2 || data_start > file.len() || (data_start & 7) != 0 {
            return Err(Error::InvalidSafetensors);
        }
        let padded_header = &file[8..data_start];
        let mut header_end = padded_header.len();
        while header_end != 0 && padded_header[header_end - 1].is_ascii_whitespace() {
            header_end -= 1;
        }
        let header = &padded_header[..header_end];
        if header.first() != Some(&b'{') || header.last() != Some(&b'}') {
            return Err(Error::InvalidSafetensors);
        }
        Ok(Self {
            file,
            header,
            data_start,
        })
    }

    pub fn tensor(&self, name: &str) -> Result<Tensor<'a>, Error> {
        let object = find_tensor_object(self.header, name.as_bytes())?;
        let dtype = field_string(object, b"dtype")?;
        if dtype != b"F32" {
            return Err(Error::UnsupportedDtype);
        }
        let (shape, rank) = field_usize_array::<MAX_DIMS>(object, b"shape")?;
        let (offsets, offset_count) = field_usize_array::<2>(object, b"data_offsets")?;
        if offset_count != 2 || offsets[0] > offsets[1] {
            return Err(Error::InvalidSafetensors);
        }
        let start = self
            .data_start
            .checked_add(offsets[0])
            .ok_or(Error::InvalidSafetensors)?;
        let end = self
            .data_start
            .checked_add(offsets[1])
            .ok_or(Error::InvalidSafetensors)?;
        if end > self.file.len() {
            return Err(Error::InvalidSafetensors);
        }
        let mut elements = 1usize;
        let mut dimension = 0usize;
        while dimension < rank {
            elements = elements
                .checked_mul(shape[dimension])
                .ok_or(Error::InvalidSafetensors)?;
            dimension += 1;
        }
        if end - start != elements.checked_mul(4).ok_or(Error::InvalidSafetensors)? {
            return Err(Error::ShapeMismatch);
        }
        Ok(Tensor {
            bytes: &self.file[start..end],
            shape,
            rank,
        })
    }
}

impl<'a> Tensor<'a> {
    pub fn shape(&self) -> &[usize] {
        &self.shape[..self.rank]
    }

    pub fn element_count(&self) -> usize {
        self.bytes.len() / 4
    }

    pub fn f32_data(&self) -> Result<&'a [f32], Error> {
        if (self.bytes.as_ptr() as usize) & (core::mem::align_of::<f32>() - 1) != 0 {
            return Err(Error::InvalidSafetensors);
        }
        // SAFETY:safetensors数据区按8字节对齐；构造时已验证dtype=F32、长度
        // 为4的倍数且offset位于文件切片内。返回切片的生命周期不超过原文件。
        Ok(unsafe {
            core::slice::from_raw_parts(self.bytes.as_ptr().cast::<f32>(), self.element_count())
        })
    }
}

fn find_tensor_object<'a>(header: &'a [u8], name: &[u8]) -> Result<&'a [u8], Error> {
    let mut cursor = 0usize;
    while cursor < header.len() {
        let Some(relative) = find_byte(&header[cursor..], b'"') else {
            break;
        };
        let name_start = cursor + relative + 1;
        let Some(name_end_relative) = find_byte(&header[name_start..], b'"') else {
            return Err(Error::InvalidSafetensors);
        };
        let name_end = name_start + name_end_relative;
        let mut colon = name_end + 1;
        skip_spaces(header, &mut colon);
        if colon >= header.len() || header[colon] != b':' {
            cursor = name_end + 1;
            continue;
        }
        colon += 1;
        skip_spaces(header, &mut colon);
        if &header[name_start..name_end] == name {
            if colon >= header.len() || header[colon] != b'{' {
                return Err(Error::InvalidSafetensors);
            }
            let end = matching_brace(header, colon)?;
            return Ok(&header[colon..=end]);
        }
        cursor = name_end + 1;
    }
    Err(Error::TensorNotFound)
}

fn field_string<'a>(object: &'a [u8], field: &[u8]) -> Result<&'a [u8], Error> {
    let value = find_field_value(object, field)?;
    if value >= object.len() || object[value] != b'"' {
        return Err(Error::InvalidSafetensors);
    }
    let start = value + 1;
    let end = start + find_byte(&object[start..], b'"').ok_or(Error::InvalidSafetensors)?;
    Ok(&object[start..end])
}

fn field_usize_array<const N: usize>(
    object: &[u8],
    field: &[u8],
) -> Result<([usize; N], usize), Error> {
    let mut cursor = find_field_value(object, field)?;
    if cursor >= object.len() || object[cursor] != b'[' {
        return Err(Error::InvalidSafetensors);
    }
    cursor += 1;
    let mut output = [0usize; N];
    let mut count = 0usize;
    loop {
        skip_spaces(object, &mut cursor);
        if cursor >= object.len() {
            return Err(Error::InvalidSafetensors);
        }
        if object[cursor] == b']' {
            return Ok((output, count));
        }
        if count == N {
            return Err(Error::ShapeMismatch);
        }
        output[count] = parse_usize(object, &mut cursor)?;
        count += 1;
        skip_spaces(object, &mut cursor);
        match object.get(cursor) {
            Some(b',') => cursor += 1,
            Some(b']') => return Ok((output, count)),
            _ => return Err(Error::InvalidSafetensors),
        }
    }
}

fn find_field_value(object: &[u8], field: &[u8]) -> Result<usize, Error> {
    let mut cursor = 0usize;
    while cursor < object.len() {
        let Some(relative) = find_byte(&object[cursor..], b'"') else {
            break;
        };
        let start = cursor + relative + 1;
        let end = start + find_byte(&object[start..], b'"').ok_or(Error::InvalidSafetensors)?;
        let mut colon = end + 1;
        skip_spaces(object, &mut colon);
        if &object[start..end] == field {
            if object.get(colon) != Some(&b':') {
                return Err(Error::InvalidSafetensors);
            }
            colon += 1;
            skip_spaces(object, &mut colon);
            return Ok(colon);
        }
        cursor = end + 1;
    }
    Err(Error::InvalidSafetensors)
}

fn parse_usize(bytes: &[u8], cursor: &mut usize) -> Result<usize, Error> {
    let mut value = 0usize;
    let start = *cursor;
    while let Some(byte @ b'0'..=b'9') = bytes.get(*cursor).copied() {
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add((byte - b'0') as usize))
            .ok_or(Error::InvalidSafetensors)?;
        *cursor += 1;
    }
    if *cursor == start {
        return Err(Error::InvalidSafetensors);
    }
    Ok(value)
}

fn matching_brace(bytes: &[u8], start: usize) -> Result<usize, Error> {
    let mut depth = 0usize;
    let mut index = start;
    let mut in_string = false;
    while index < bytes.len() {
        match bytes[index] {
            b'"' if index == 0 || bytes[index - 1] != b'\\' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth = depth.checked_sub(1).ok_or(Error::InvalidSafetensors)?;
                if depth == 0 {
                    return Ok(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    Err(Error::InvalidSafetensors)
}

fn skip_spaces(bytes: &[u8], cursor: &mut usize) {
    while matches!(bytes.get(*cursor), Some(b' ' | b'\n' | b'\r' | b'\t')) {
        *cursor += 1;
    }
}

fn find_byte(bytes: &[u8], needle: u8) -> Option<usize> {
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == needle {
            return Some(index);
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn parses_f32_tensor_without_copying() {
        let mut header = br#"{"x":{"dtype":"F32","shape":[2],"data_offsets":[0,8]}}"#.to_vec();
        while (8 + header.len()) & 7 != 0 {
            header.push(b' ');
        }
        let mut file = Vec::new();
        file.extend_from_slice(&(header.len() as u64).to_le_bytes());
        file.extend_from_slice(&header);
        file.extend_from_slice(&1.25f32.to_le_bytes());
        file.extend_from_slice(&(-2.5f32).to_le_bytes());
        let tensors = SafeTensors::parse(&file).unwrap();
        let tensor = tensors.tensor("x").unwrap();
        assert_eq!(tensor.shape(), &[2]);
        assert_eq!(tensor.f32_data().unwrap(), &[1.25, -2.5]);
    }

    #[test]
    fn parses_downloaded_act_model_when_available() {
        let directory = std::env::var("ACT_MODEL_DIR").unwrap_or_else(|_| {
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/act/model").into()
        });
        let path = std::path::Path::new(&directory).join("model.safetensors");
        if !path.exists() {
            return;
        }
        let bytes = std::fs::read(path).unwrap();
        let tensors = SafeTensors::parse(&bytes).unwrap();
        assert_eq!(
            tensors
                .tensor("model.backbone.conv1.weight")
                .unwrap()
                .shape(),
            &[64, 3, 7, 7]
        );
        assert_eq!(
            tensors
                .tensor("model.decoder_pos_embed.weight")
                .unwrap()
                .shape(),
            &[100, 512]
        );
        assert_eq!(
            tensors.tensor("model.action_head.weight").unwrap().shape(),
            &[6, 512]
        );
    }
}
