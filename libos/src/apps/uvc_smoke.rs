//! UVC单摄像头、单帧验收应用。
//!
//! 这里只定义实验策略：打开第一台UVC设备、抓取一帧并输出摘要。描述符解析、
//! Probe/Commit及payload重组全部由`drivers::uvc`完成。

use crab_usb::{EventHandler, USBHost};

pub(crate) fn run(
    info: &exo_abi::UserBootInfo,
    host: &mut USBHost,
    handler: &EventHandler,
    intid: u32,
    notification: &crate::notification::Notification,
) -> ! {
    let mut stream = match crate::runtime::usb_executor::block_on_usb(
        crate::drivers::uvc::open_first_camera(host, handler),
        handler,
        intid,
        notification,
    ) {
        Ok(stream) => stream,
        Err(error) => fail_uvc(b"open failed", error),
    };

    crate::runtime::puts(b"[libos] UVC streaming ready format=");
    crate::runtime::puts(stream.format.name());
    crate::runtime::puts(b" width=");
    crate::runtime::hex(stream.width as u64);
    crate::runtime::puts(b" height=");
    crate::runtime::hex(stream.height as u64);
    crate::runtime::puts(b" interval_100ns=");
    crate::runtime::hex(stream.interval_100ns as u64);
    crate::runtime::puts(b"\r\n");

    let frame = match crate::runtime::usb_executor::block_on_usb(
        stream.capture_frame(),
        handler,
        intid,
        notification,
    ) {
        Ok(frame) => frame,
        Err(error) => fail_uvc(b"capture failed", error),
    };
    let digest = sha256(frame.as_slice());
    crate::runtime::puts(b"[libos] UVC frame complete bytes=");
    crate::runtime::hex(frame.len() as u64);
    crate::runtime::puts(b" sha256=");
    log_hex_bytes(&digest);
    crate::runtime::puts(b"\r\n");

    crate::runtime::puts(b"[libos] writing /uvc-frame.mjpg to user ext4\r\n");
    let mut volume = crate::fs::CaptureVolume::discover(info)
        .unwrap_or_else(|code| fail_storage(b"capture volume discovery failed", code));
    volume
        .write_frame(frame.as_slice())
        .unwrap_or_else(|code| fail_storage(b"capture file write failed", code));
    crate::runtime::puts(b"[libos] ext4 capture flushed\r\n");
    crate::runtime::puts(b"[libos] UVC smoke passed\r\n");
    crate::runtime::exit(0)
}

fn fail_storage(message: &[u8], code: u64) -> ! {
    crate::runtime::puts(b"[libos] ");
    crate::runtime::puts(message);
    crate::runtime::puts(b" code=");
    crate::runtime::hex(code);
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(code)
}

fn fail_uvc(message: &[u8], error: crate::drivers::uvc::UvcError) -> ! {
    crate::runtime::puts(b"[libos] UVC ");
    crate::runtime::puts(message);
    crate::runtime::puts(b" error=");
    crate::runtime::hex(error as u64);
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(0x500 + error as u64)
}

fn log_hex_bytes(bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        let pair = [HEX[(byte >> 4) as usize], HEX[(byte & 0x0f) as usize]];
        crate::runtime::puts(&pair);
    }
}

/// 小型SHA-256只用于验证抓到的帧内容稳定且非空，不参与USB热路径。
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let total = (data.len() + 9 + 63) & !63;
    let mut block = [0u8; 64];
    for block_index in 0..total / 64 {
        block.fill(0);
        let start = block_index * 64;
        let copied = data.len().saturating_sub(start).min(64);
        if copied > 0 {
            block[..copied].copy_from_slice(&data[start..start + copied]);
        }
        if start <= data.len() && data.len() < start + 64 {
            block[data.len() - start] = 0x80;
        }
        if block_index + 1 == total / 64 {
            block[56..].copy_from_slice(&bit_len.to_be_bytes());
        }

        let mut words = [0u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(chunk.try_into().unwrap());
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let big1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(big1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let big0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = big0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut output = [0u8; 32];
    for (chunk, value) in output.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    output
}
