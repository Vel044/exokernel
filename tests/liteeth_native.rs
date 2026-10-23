//! 直接测试实际参与 EL0 构建的 Rust 状态机；普通数组不替代 QEMU MMIO/IRQ 验证。
#[path = "../libos/src/drivers/liteeth_core.rs"]
mod driver;
#[path = "../libos/src/linux_api/mod.rs"]
mod linux_api;
use driver::*;
use std::{cell::RefCell, rc::Rc};
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Read(usize),
    Write(usize, u32),
    From(usize, usize),
    To(usize, usize),
}
struct State {
    regs: [u32; 64],
    buffer: Vec<u8>,
    ops: Vec<Op>,
}
#[derive(Clone)]
struct Model(Rc<RefCell<State>>);
impl Model {
    fn new() -> Self {
        let mut regs = [0; 64];
        regs[TX_READY / 4] = 1;
        Self(Rc::new(RefCell::new(State {
            regs,
            buffer: vec![0; 8192],
            ops: vec![],
        })))
    }
    fn tx_done(&self) {
        let mut s = self.0.borrow_mut();
        s.regs[TX_READY / 4] = 1;
        s.regs[TX_PENDING / 4] = 1;
    }
    fn rx(&self, slot: u32, length: u32, fill: u8) {
        let mut s = self.0.borrow_mut();
        s.buffer.fill(fill);
        s.regs[RX_SLOT / 4] = slot;
        s.regs[RX_LENGTH / 4] = length;
        s.regs[RX_PENDING / 4] = 1;
    }
    fn clear_ops(&self) {
        self.0.borrow_mut().ops.clear();
    }
}
impl Io for Model {
    fn read(&self, o: usize) -> u32 {
        let mut s = self.0.borrow_mut();
        s.ops.push(Op::Read(o));
        s.regs[o / 4]
    }
    fn write(&self, o: usize, v: u32) {
        let mut s = self.0.borrow_mut();
        s.ops.push(Op::Write(o, v));
        if o == RX_PENDING || o == TX_PENDING {
            s.regs[o / 4] &= !v;
        } else {
            s.regs[o / 4] = v;
        }
        if o == TX_START {
            s.regs[TX_READY / 4] = 0;
        }
    }
    fn copy_from(&self, o: usize, b: &mut [u8]) {
        let mut s = self.0.borrow_mut();
        s.ops.push(Op::From(o, b.len()));
        b.copy_from_slice(&s.buffer[o..o + b.len()]);
    }
    fn copy_to(&self, o: usize, b: &[u8]) {
        let mut s = self.0.borrow_mut();
        s.ops.push(Op::To(o, b.len()));
        s.buffer[o..o + b.len()].copy_from_slice(b);
    }
}
fn closed() -> (Device<Model>, Model) {
    let io = Model::new();
    let d = Device::new(
        io.clone(),
        Layout {
            rx_slots: 2,
            tx_slots: 2,
            slot_size: 2048,
        },
        8192,
    )
    .unwrap();
    (d, io)
}
fn opened() -> (Device<Model>, Model) {
    let (mut d, io) = closed();
    d.prepare_open().unwrap();
    d.enable();
    io.clear_ops();
    (d, io)
}
#[test]
fn rejects_invalid_layout_before_access() {
    for (rx, tx, size, span) in [
        (0, 2, 2048, 8192),
        (2, 0, 2048, 8192),
        (257, 2, 2048, 8192),
        (2, 2, 0, 8192),
        (2, 2, 2049, 8192),
        (2, 2, 2048, 8191),
    ] {
        let io = Model::new();
        assert!(Device::new(
            io.clone(),
            Layout {
                rx_slots: rx,
                tx_slots: tx,
                slot_size: size
            },
            span
        )
        .is_err());
        assert!(io.0.borrow().ops.is_empty());
    }
}
#[test]
fn open_clears_events_before_enable_and_rejects_double_open() {
    let (mut d, io) = closed();
    d.prepare_open().unwrap();
    d.enable();
    assert_eq!(
        io.0.borrow().ops,
        vec![
            Op::Write(RX_PENDING, 1),
            Op::Write(TX_PENDING, 1),
            Op::Write(RX_ENABLE, 1),
            Op::Write(TX_ENABLE, 1)
        ]
    );
    io.clear_ops();
    assert_eq!(d.prepare_open(), Err(-16));
    assert!(io.0.borrow().ops.is_empty());
}
#[test]
fn closed_and_short_tx_do_not_touch_hardware() {
    let (mut d, io) = closed();
    assert_eq!(d.send(&[1; 60]), Err(-22));
    assert!(io.0.borrow().ops.is_empty());
    d.prepare_open().unwrap();
    d.enable();
    io.clear_ops();
    assert_eq!(d.send(&[1; 13]), Err(-22));
    assert!(io.0.borrow().ops.is_empty());
}
#[test]
fn tx_copies_before_register_commit_and_rotates_slots() {
    let (mut d, io) = opened();
    for slot in [0, 1, 0] {
        io.tx_done();
        d.handle_irq();
        io.clear_ops();
        assert_eq!(d.send(&[7; 60]), Ok(true));
        assert_eq!(
            io.0.borrow().ops,
            vec![
                Op::Read(TX_READY),
                Op::To(4096 + slot * 2048, 60),
                Op::Write(TX_SLOT, slot as u32),
                Op::Write(TX_LENGTH, 60),
                Op::Write(TX_START, 1)
            ]
        );
        assert_eq!(
            &io.0.borrow().buffer[4096 + slot * 2048..4096 + slot * 2048 + 60],
            &[7; 60]
        );
    }
    assert_eq!(d.stats().tx_packets, 3);
}
#[test]
fn busy_keeps_packet_slot_and_stats_until_completion() {
    let (mut d, io) = opened();
    io.0.borrow_mut().regs[TX_READY / 4] = 0;
    assert_eq!(d.send(&[8; 60]), Ok(false));
    assert_eq!(d.stats(), Stats::default());
    io.0.borrow_mut().regs[TX_READY / 4] = 1;
    io.clear_ops();
    assert_eq!(d.send(&[8; 60]), Ok(false));
    assert!(io.0.borrow().ops.is_empty());
    io.tx_done();
    d.handle_irq();
    assert_eq!(d.send(&[8; 60]), Ok(true));
    assert_eq!(io.0.borrow().regs[TX_SLOT / 4], 0);
}
#[test]
fn oversize_is_consumed_and_counted_without_tx() {
    let (mut d, io) = opened();
    assert_eq!(d.send(&[0; 2049]), Ok(true));
    assert_eq!(io.0.borrow().ops, vec![Op::Read(TX_READY)]);
    let s = d.stats();
    assert_eq!((s.tx_packets, s.tx_errors, s.tx_dropped), (0, 1, 1));
}
#[test]
fn irq_handles_tx_then_copies_rx_before_w1c() {
    let (mut d, io) = opened();
    io.tx_done();
    io.rx(1, 60, 9);
    d.handle_irq();
    assert_eq!(
        io.0.borrow().ops,
        vec![
            Op::Read(TX_PENDING),
            Op::Write(TX_PENDING, 1),
            Op::Read(RX_PENDING),
            Op::Read(RX_SLOT),
            Op::Read(RX_LENGTH),
            Op::From(2048, 60),
            Op::Write(RX_PENDING, 1)
        ]
    );
    let mut out = [0; 60];
    assert_eq!(d.receive(&mut out), Ok(60));
    assert_eq!(out, [9; 60]);
    assert_eq!(d.stats().rx_bytes, 60);
}
#[test]
fn invalid_device_slot_or_length_is_dropped_without_sram_access() {
    let (mut d, io) = opened();
    for (slot, n) in [
        (0, 0),
        (0, 13),
        (0, 2049),
        (0, u32::MAX),
        (2, 60),
        (255, 60),
    ] {
        io.clear_ops();
        io.rx(slot, n, 0);
        d.handle_irq();
        assert!(!io.0.borrow().ops.iter().any(|o| matches!(o, Op::From(..))));
        assert_eq!(io.0.borrow().regs[RX_PENDING / 4], 0);
    }
    assert_eq!((d.stats().rx_errors, d.stats().rx_dropped), (6, 6));
}
#[test]
fn small_output_preserves_frame_for_retry() {
    let (mut d, io) = opened();
    io.rx(0, 60, 11);
    d.handle_irq();
    assert_eq!(d.receive(&mut [0; 59]), Err(-22));
    let mut out = [0; 60];
    assert_eq!(d.receive(&mut out), Ok(60));
    assert_eq!(out, [11; 60]);
    assert_eq!(d.receive(&mut out), Ok(0));
}
#[test]
fn full_delivery_slot_keeps_first_frame_and_records_drop() {
    let (mut d, io) = opened();
    io.rx(0, 60, 12);
    d.handle_irq();
    io.rx(1, 60, 13);
    d.handle_irq();
    let mut out = [0; 60];
    d.receive(&mut out).unwrap();
    assert_eq!(out, [12; 60]);
    assert_eq!(d.delivery_drops, 1);
    assert_eq!(d.stats().rx_packets, 2);
}
#[test]
fn stop_reopen_keeps_tx_rotation_and_disables_both_event_sources() {
    let (mut d, io) = opened();
    d.send(&[0; 60]).unwrap();
    io.tx_done();
    d.handle_irq();
    io.clear_ops();
    d.stop();
    assert_eq!(
        io.0.borrow().ops,
        vec![Op::Write(RX_ENABLE, 0), Op::Write(TX_ENABLE, 0)]
    );
    assert_eq!(d.send(&[0; 60]), Err(-22));
    d.prepare_open().unwrap();
    d.enable();
    d.send(&[0; 60]).unwrap();
    assert_eq!(io.0.borrow().regs[TX_SLOT / 4], 1);
    d.stop();
    d.unregister().unwrap();
    io.clear_ops();
    assert_eq!(d.prepare_open(), Err(-22));
    assert!(io.0.borrow().ops.is_empty());
}
