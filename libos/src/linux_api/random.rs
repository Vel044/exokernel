// SPDX-License-Identifier: GPL-2.0-or-later
//! ChaCha20 随机字节流；必须先用固件/虚拟机的真实随机种子初始化，禁止以计数器冒充熵。
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};
struct Pool {
    key: [u32; 8],
    counter: u64,
    seeded: bool,
}
struct Locked(UnsafeCell<Pool>);
// SAFETY: LOCK 保护整个随机池，禁止并发读写未加锁的状态。
unsafe impl Sync for Locked {}
static LOCK: AtomicBool = AtomicBool::new(false);
static POOL: Locked = Locked(UnsafeCell::new(Pool {
    key: [0; 8],
    counter: 0,
    seeded: false,
}));
fn enter() {
    while LOCK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}
pub fn seed(seed: &[u8]) {
    assert!(seed.len() >= 32, "随机种子不足32字节");
    enter();
    unsafe {
        let p = &mut *POOL.0.get();
        for i in 0..8 {
            p.key[i] = u32::from_le_bytes(seed[i * 4..i * 4 + 4].try_into().unwrap());
        }
        p.counter = 0;
        p.seeded = true;
    }
    LOCK.store(false, Ordering::Release);
}
fn qr(x: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(16);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(12);
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(8);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(7);
}
pub fn bytes(out: &mut [u8]) {
    enter();
    unsafe {
        let p = &mut *POOL.0.get();
        if !p.seeded {
            LOCK.store(false, Ordering::Release);
            panic!("缺少可信随机种子，不能伪造随机MAC");
        }
        for chunk in out.chunks_mut(64) {
            let mut s = [0u32; 16];
            s[..4].copy_from_slice(&[0x61707865, 0x3320646e, 0x79622d32, 0x6b206574]);
            s[4..12].copy_from_slice(&p.key);
            s[12] = p.counter as u32;
            s[13] = (p.counter >> 32) as u32;
            let mut x = s;
            for _ in 0..10 {
                qr(&mut x, 0, 4, 8, 12);
                qr(&mut x, 1, 5, 9, 13);
                qr(&mut x, 2, 6, 10, 14);
                qr(&mut x, 3, 7, 11, 15);
                qr(&mut x, 0, 5, 10, 15);
                qr(&mut x, 1, 6, 11, 12);
                qr(&mut x, 2, 7, 8, 13);
                qr(&mut x, 3, 4, 9, 14);
            }
            let mut block = [0; 64];
            for i in 0..16 {
                block[i * 4..i * 4 + 4].copy_from_slice(&x[i].wrapping_add(s[i]).to_le_bytes());
            }
            chunk.copy_from_slice(&block[..chunk.len()]);
            p.counter = p.counter.checked_add(1).expect("随机流耗尽");
        }
    }
    LOCK.store(false, Ordering::Release);
}
