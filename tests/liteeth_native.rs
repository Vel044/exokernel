//! 逐函数翻译的实际驱动测试：对照原分支、返回值、MMIO 顺序及资源生命周期。
extern crate alloc;
#[path = "../libos/src/drivers/liteeth.rs"]
mod driver;
#[path = "../libos/src/linux_api/mod.rs"]
mod linux_api;
#[path = "support/liteeth.rs"]
mod support;
use linux_api::{memory, netdevice::*, skbuff::*};
use support::*;
#[test]
fn busy_keeps_skb_and_slot() {
    reset();
    let mut f = fixture();
    f.open();
    let p = packet(f.dev(), 60);
    let live = memory::live();
    STATE.with(|s| s.borrow_mut().csr[0x1c / 4] = 0);
    assert_eq!(
        unsafe { driver::liteeth_start_xmit(p, f.dev()) },
        NETDEV_TX_BUSY
    );
    assert_eq!(memory::live(), live);
    assert_eq!(f.stats().tx_packets, 0);
    tx_done();
    unsafe {
        driver::liteeth_interrupt(42, f.dev().cast());
        assert_eq!(driver::liteeth_start_xmit(p, f.dev()), NETDEV_TX_OK);
    }
    assert_eq!(f.stats().tx_packets, 1);
}
#[test]
fn oversize_consumes_and_counts_after_ready() {
    reset();
    let mut f = fixture();
    f.open();
    let before = memory::live();
    let p = packet(f.dev(), 2049);
    assert_eq!(
        unsafe { driver::liteeth_start_xmit(p, f.dev()) },
        NETDEV_TX_OK
    );
    assert_eq!(memory::live(), before);
    let s = f.stats();
    assert_eq!((s.tx_dropped, s.tx_errors, s.tx_packets), (1, 1, 0));
}
#[test]
fn tx_slot_length_start_and_rotation() {
    reset();
    let mut f = fixture();
    f.open();
    STATE.with(|s| s.borrow_mut().ops.clear());
    for _ in 0..3 {
        tx_done();
        let p = packet(f.dev(), 60);
        assert_eq!(unsafe { driver::liteeth_start_xmit(p, f.dev()) }, 0);
    }
    let ops = STATE.with(|s| s.borrow().ops.clone());
    let slots: Vec<_> = ops
        .iter()
        .filter_map(|op| {
            if let Op::Write(0x24, v) = op {
                Some(*v)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(slots, [0, 1, 0]);
    for i in 0..ops.len() {
        if ops[i] == Op::Write(0x18, 1) {
            assert_eq!(
                ops[i - 2],
                Op::Write(
                    0x24,
                    slots[ops[..i]
                        .iter()
                        .filter(|v| **v == Op::Write(0x18, 1))
                        .count()]
                )
            );
            assert_eq!(ops[i - 1], Op::Write(0x28, 60));
        }
    }
}
#[test]
fn rx_copies_before_w1c_and_preserves_frame() {
    reset();
    let mut f = fixture();
    f.open();
    rx(1, 60);
    STATE.with(|s| s.borrow_mut().ops.clear());
    assert_eq!(unsafe { driver::liteeth_interrupt(42, f.dev().cast()) }, 1);
    let s = f.stats();
    assert_eq!((s.rx_packets, s.rx_bytes), (1, 60));
    unsafe {
        let skb = (*f.dev()).rx_queue.pop_front().unwrap();
        assert_eq!(frame(skb).len(), 60);
        assert_eq!((*skb).len, 46);
        dev_kfree_skb_any(skb);
    }
    let ops = STATE.with(|s| s.borrow().ops.clone());
    assert!(
        ops.iter().position(|o| *o == Op::From(2048, 60)).unwrap()
            < ops.iter().position(|o| *o == Op::Write(0x10, 1)).unwrap()
    );
}
#[test]
fn rx_original_zero_and_oversize_drop_paths() {
    reset();
    let mut f = fixture();
    f.open();
    for len in [0, 2049] {
        rx(0, len);
        assert_eq!(unsafe { driver::liteeth_rx(f.dev()) }, NET_RX_DROP);
    }
    assert_eq!((f.stats().rx_errors, f.stats().rx_dropped), (2, 2));
}
#[test]
fn rx_allocation_failure_uses_drop_join() {
    reset();
    let mut f = fixture();
    f.open();
    rx(0, 60);
    for n in [0, 1] {
        memory::fail_after(n);
        assert_eq!(unsafe { driver::liteeth_rx(f.dev()) }, NET_RX_DROP);
        memory::fail_after(usize::MAX);
    }
    assert_eq!((f.stats().rx_errors, f.stats().rx_dropped), (2, 2));
    assert!(
        STATE.with(|s| s
            .borrow()
            .logs
            .iter()
            .filter(|l| l.contains("couldn't get memory"))
            .count())
            == 2
    );
}
#[test]
fn open_failure_does_not_enable_device() {
    reset();
    let mut f = fixture();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.fail_bind = true;
        s.ops.clear();
    });
    assert_eq!(unsafe { dev_open(f.dev()) }, -5);
    unsafe {
        // 请求 IRQ 失败不能把默认状态误改成 stop 后的状态。
        assert!(!netif_queue_stopped(f.dev()));
        assert!((*f.dev()).carrier);
        assert!(!(*f.dev()).running);
        assert!((*f.dev()).link_events.is_empty());
    }
    assert!(!STATE.with(|s| s
        .borrow()
        .ops
        .iter()
        .any(|o| matches!(o, Op::Write(0x14, 1) | Op::Write(0x34, 1)))));
}
#[test]
fn default_and_explicit_properties_are_in_driver() {
    reset();
    let f = fixture();
    unsafe {
        let p = netdev_priv(f.dev()).cast::<driver::liteeth>();
        assert_eq!(
            ((*p).num_rx_slots, (*p).num_tx_slots, (*p).slot_size),
            (2, 2, 2048)
        );
    }
    drop(f);
    let mut f = Fixture::new(
        vec![
            ("litex,rx-slots", 1),
            ("litex,tx-slots", 3),
            ("litex,slot-size", 1024),
        ],
        Some([2, 0, 0, 0, 0, 1]),
    );
    assert_eq!(f.attach(), 0);
    unsafe {
        let p = netdev_priv(f.dev()).cast::<driver::liteeth>();
        assert_eq!(
            ((*p).num_rx_slots, (*p).num_tx_slots, (*p).slot_size),
            (1, 3, 1024)
        );
    }
}
#[test]
fn probe_allocation_and_map_failures_rollback() {
    reset();
    for n in 0..2 {
        let mut f = Fixture::new(vec![], Some([2, 0, 0, 0, 0, 1]));
        memory::fail_after(n);
        assert_eq!(f.attach(), -12);
        memory::fail_after(usize::MAX);
        assert_eq!(memory::live(), 0);
    }
    for pa in [0x1000, 0x2000] {
        STATE.with(|s| s.borrow_mut().map_fail = Some(pa));
        let mut f = Fixture::new(vec![], Some([2, 0, 0, 0, 0, 1]));
        assert_eq!(f.attach(), -5);
        assert_eq!(memory::live(), 0);
        assert_eq!(STATE.with(|s| s.borrow().maps), 0);
    }
    STATE.with(|s| s.borrow_mut().map_fail = None);
    let mut f = fixture();
    f.detach();
    assert_eq!(memory::live(), 0);
}
#[test]
fn stop_reopen_and_remove_auto_stop() {
    reset();
    let mut f = fixture();
    unsafe {
        assert!(!netif_queue_stopped(f.dev()));
        assert!((*f.dev()).carrier);
        assert!(!(*f.dev()).running);
    }
    f.open();
    unsafe {
        // 首次 open 没有发生 carrier 翻转，不能多报一次链路恢复。
        assert!(!netif_queue_stopped(f.dev()));
        assert!((*f.dev()).carrier);
        assert!((*f.dev()).running);
        assert!((*f.dev()).link_events.is_empty());
        assert!(!(*f.dev()).tx_scheduled);
    }
    f.close();
    unsafe {
        assert!(netif_queue_stopped(f.dev()));
        assert!(!(*f.dev()).carrier);
        assert!(!(*f.dev()).running);
        assert_eq!((*f.dev()).link_events, [false]);
    }
    f.close();
    assert_eq!(unsafe { &(*f.dev()).link_events }, &[false]);
    assert_eq!(STATE.with(|s| s.borrow().bound), 0);
    f.open();
    unsafe {
        assert!(!netif_queue_stopped(f.dev()));
        assert!((*f.dev()).carrier);
        assert!((*f.dev()).running);
        assert_eq!((*f.dev()).link_events, [false, true]);
    }
    f.detach();
    assert_eq!(
        STATE.with(|s| {
            let s = s.borrow();
            (s.bound, s.maps)
        }),
        (0, 0)
    );
    assert_eq!(memory::live(), 0);
}
#[test]
fn mac_read_and_random_fallback_are_executed() {
    reset();
    let f = fixture();
    assert_eq!(unsafe { (*f.dev()).dev_addr }, [2, 1, 2, 3, 4, 5]);
    drop(f);
    let mut f = Fixture::new(vec![], None);
    assert_eq!(f.attach(), 0);
    let address = unsafe { (*f.dev()).dev_addr };
    assert_eq!(address[0] & 3, 2);
    assert_ne!(address, [0; 6]);
}
