//! 无分配 SHA-256，用于把模型和共享执行计划绑定到运行时元数据。

const ROUND: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

#[inline]
const fn rotr(value: u32, amount: u32) -> u32 {
    value.rotate_right(amount)
}

/// 计算输入字节的 SHA-256；返回值可直接嵌入 `PlanMetadata`。
pub fn digest(input: &[u8]) -> [u8; 32] {
    let mut state = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let full_blocks = input.len() / 64;
    let mut block = 0;
    while block < full_blocks {
        compress(&mut state, &input[block * 64..block * 64 + 64]);
        block += 1;
    }

    let remainder = &input[full_blocks * 64..];
    let mut tail = [0u8; 128];
    let mut index = 0;
    while index < remainder.len() {
        tail[index] = remainder[index];
        index += 1;
    }
    tail[remainder.len()] = 0x80;
    let tail_len = if remainder.len() >= 56 { 128 } else { 64 };
    let bit_len = (input.len() as u64) * 8;
    let mut length_index = tail_len - 8;
    while length_index < tail_len {
        tail[length_index] = (bit_len >> ((tail_len - 1 - length_index) * 8)) as u8;
        length_index += 1;
    }
    compress(&mut state, &tail[..64]);
    if tail_len == 128 {
        compress(&mut state, &tail[64..]);
    }

    let mut output = [0u8; 32];
    let mut word = 0;
    while word < 8 {
        output[word * 4..word * 4 + 4].copy_from_slice(&state[word].to_be_bytes());
        word += 1;
    }
    output
}

fn compress(state: &mut [u32; 8], block: &[u8]) {
    let mut schedule = [0u32; 64];
    let mut index = 0;
    while index < 16 {
        let offset = index * 4;
        schedule[index] = u32::from_be_bytes([
            block[offset],
            block[offset + 1],
            block[offset + 2],
            block[offset + 3],
        ]);
        index += 1;
    }
    while index < 64 {
        let x = schedule[index - 15];
        let y = schedule[index - 2];
        let small_sigma0 = rotr(x, 7) ^ rotr(x, 18) ^ (x >> 3);
        let small_sigma1 = rotr(y, 17) ^ rotr(y, 19) ^ (y >> 10);
        schedule[index] = schedule[index - 16]
            .wrapping_add(small_sigma0)
            .wrapping_add(schedule[index - 7])
            .wrapping_add(small_sigma1);
        index += 1;
    }

    let mut working = *state;
    index = 0;
    while index < 64 {
        let [a, b, c, d, e, f, g, h] = working;
        let big_sigma1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25);
        let choose = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(big_sigma1)
            .wrapping_add(choose)
            .wrapping_add(ROUND[index])
            .wrapping_add(schedule[index]);
        let big_sigma0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = big_sigma0.wrapping_add(majority);
        working = [
            temp1.wrapping_add(temp2),
            a,
            b,
            c,
            d.wrapping_add(temp1),
            e,
            f,
            g,
        ];
        index += 1;
    }
    let mut index = 0;
    while index < 8 {
        state[index] = state[index].wrapping_add(working[index]);
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::digest;

    #[test]
    fn hashes_standard_vectors() {
        assert_eq!(
            digest(b""),
            [
                0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
                0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
                0x78, 0x52, 0xb8, 0x55,
            ]
        );
        assert_eq!(
            digest(b"abc"),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
    }
}
