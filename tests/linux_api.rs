//! 使用实际 Linux 形状接口验证定义、所有权、分配失败、队列调度及每 CPU 统计。
extern crate alloc;
#[path = "../libos/src/drivers/liteeth.rs"]
mod driver;
#[path = "../libos/src/linux_api/mod.rs"]
mod linux_api;
#[path = "support/liteeth.rs"]
mod support;
use core::{ffi::c_void, ptr};
use linux_api::{error::*, interrupt::*, memory, netdevice::*, platform::*, printk::*, skbuff::*};
use support::*;
#[test]
fn allocation_returns_null_and_private_address_stays_stable() {
    reset();
    for n in [0, 1] {
        let mut f = Fixture::new(vec![], None);
        memory::fail_after(n);
        let dev = unsafe { devm_alloc_etherdev(&mut f.pdev.dev, 32) };
        assert!(dev.is_null());
        memory::fail_after(usize::MAX);
        assert_eq!(memory::live(), 0);
    }
    let f = fixture();
    let p = unsafe { netdev_priv(f.dev()) };
    assert_eq!((p as usize) % 32, 0);
    assert_eq!(p, unsafe { netdev_priv(f.dev()) });
}
#[test]
fn cpu_stats_and_registered_device_directory_are_real() {
    reset();
    let a = fixture();
    let mut b = Fixture::new(vec![], Some([2, 0, 0, 0, 0, 2]));
    b.pdev.resources[0].base = 0x5000;
    b.pdev.resources[1].base = 0x6000;
    assert_eq!(b.attach(), 0);
    assert_ne!(unsafe { &(*a.dev()).name }, unsafe { &(*b.dev()).name });
    assert_eq!(registered_devices(), 2);
    for cpu in 0..4 {
        STATE.with(|s| s.borrow_mut().cpu = cpu);
        unsafe {
            dev_sw_netstats_rx_add(a.dev(), (cpu + 1) as u32);
            dev_sw_netstats_tx_add(a.dev(), 1, 10);
        }
    }
    assert_eq!(
        (
            a.stats().rx_packets,
            a.stats().rx_bytes,
            a.stats().tx_packets,
            a.stats().tx_bytes
        ),
        (4, 10, 4, 40)
    );
    assert_eq!(b.stats().rx_packets, 0);
    drop(a);
    drop(b);
    assert_eq!(registered_devices(), 0);
    assert_eq!(memory::live(), 0);
}
#[test]
fn skb_heap_alignment_put_pointer_and_failed_allocation() {
    reset();
    let f = fixture();
    let baseline = memory::live();
    unsafe {
        let skb = netdev_alloc_skb_ip_align(f.dev(), 60);
        assert!(!skb.is_null());
        assert_eq!(((*skb).data as usize + 14) % 4, 0);
        let old = (*skb).data;
        let data = skb_put(skb, 40);
        assert_eq!(data, old);
        assert_eq!((*skb).len, 40);
        assert_eq!(skb_put(skb, 20), old.add(40));
        assert_eq!((*skb).len, 60);
        dev_kfree_skb_any(skb);
    }
    assert_eq!(memory::live(), baseline);
    memory::fail_after(0);
    assert!(unsafe { netdev_alloc_skb_ip_align(f.dev(), 60) }.is_null());
    memory::fail_after(usize::MAX);
}
#[test]
fn skb_put_overrun_panics_instead_of_changing_error_protocol() {
    reset();
    let f = fixture();
    let skb = unsafe { netdev_alloc_skb_ip_align(f.dev(), 8) };
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { skb_put(skb, 9) }))
            .is_err()
    );
    unsafe {
        dev_kfree_skb_any(skb);
    }
}
#[test]
fn ethernet_header_metadata_and_network_endian_protocol() {
    reset();
    let f = fixture();
    for (field, body, protocol) in [
        (0x0800u16, [0, 0], 0x0800u16),
        (30, [255, 255], 1),
        (30, [0, 0], 4),
    ] {
        let mut bytes = vec![0u8; 60];
        bytes[..6].copy_from_slice(&[255; 6]);
        bytes[12..14].copy_from_slice(&field.to_be_bytes());
        bytes[14..16].copy_from_slice(&body);
        unsafe {
            let skb = skb_from_bytes(f.dev(), &bytes);
            let ret = eth_type_trans(skb, f.dev());
            (*skb).protocol = ret;
            assert_eq!(u16::from_be(ret), protocol);
            assert_eq!((*skb).len, 46);
            assert_eq!((*skb).pkt_type, 1);
            assert_eq!((*skb).dev, f.dev());
            assert_eq!(frame(skb), bytes);
            dev_kfree_skb_any(skb);
        }
    }
}
#[test]
fn netif_rx_transfers_ownership_and_returns_without_consuming_payload() {
    reset();
    let f = fixture();
    let baseline = memory::live();
    unsafe {
        for value in [1, 2] {
            let skb = packet(f.dev(), 60);
            *(*skb).data = value;
            (*skb).protocol = eth_type_trans(skb, f.dev());
            assert_eq!(netif_rx(skb), NET_RX_SUCCESS);
        }
        assert_eq!((*f.dev()).rx_queue.len(), 2);
        for value in [1, 2] {
            let skb = (*f.dev()).rx_queue.pop_front().unwrap();
            assert_eq!(frame(skb)[0], value);
            dev_kfree_skb_any(skb);
        }
    }
    assert_eq!(memory::live(), baseline);
}
#[test]
fn wake_queue_schedules_and_retries_owned_backlog() {
    reset();
    let mut f = fixture();
    f.open();
    STATE.with(|s| s.borrow_mut().csr[0x1c / 4] = 0);
    let p = packet(f.dev(), 60);
    unsafe {
        assert_eq!(enqueue_tx(f.dev(), p), 0);
        assert_eq!((*f.dev()).tx_queue.len(), 1);
        assert!(netif_queue_stopped(f.dev()));
    }
    tx_done();
    unsafe {
        dispatch(42).unwrap(); // 回调返回后由 IRQ 服务自动推进 NET_TX。
        assert!((*f.dev()).tx_queue.is_empty());
    }
    assert_eq!(f.stats().tx_packets, 1);
}
unsafe fn callback(irq: i32, dev_id: *mut c_void) -> irqreturn_t {
    assert_eq!(irq, 77);
    *(dev_id.cast::<u32>()) += 1;
    STATE.with(|s| s.borrow_mut().ops.push(Op::Callback));
    IRQ_HANDLED
}
#[test]
fn request_irq_preserves_callback_irq_and_dev_id_between_wait_and_ack() {
    reset();
    let mut context = 0u32;
    unsafe {
        assert_eq!(
            request_irq(
                77,
                callback,
                0,
                c"other".as_ptr(),
                (&mut context as *mut u32).cast()
            ),
            0
        );
        dispatch(77).unwrap();
        assert_eq!(context, 1);
        free_irq(77, (&mut context as *mut u32).cast());
    }
    assert_eq!(
        STATE.with(|s| s.borrow().ops.clone()),
        [
            Op::Bind(77),
            Op::Wait,
            Op::Callback,
            Op::Ack(77),
            Op::Unbind(77),
            Op::Destroy
        ]
    );
    assert_eq!(bindings(), 0);
}
#[test]
fn failed_bind_flags_and_duplicate_registration_do_not_replace_context() {
    reset();
    let mut context = 0u32;
    let dev = (&mut context as *mut u32).cast();
    unsafe {
        assert_eq!(
            request_irq(77, callback, 1, c"unsupported".as_ptr(), dev),
            -EOPNOTSUPP
        );
        STATE.with(|s| s.borrow_mut().fail_bind = true);
        assert_eq!(request_irq(77, callback, 0, c"fail".as_ptr(), dev), -5);
        assert_eq!(bindings(), 0);
        STATE.with(|s| s.borrow_mut().fail_bind = false);
        assert_eq!(request_irq(77, callback, 0, c"ok".as_ptr(), dev), 0);
        assert_eq!(
            request_irq(77, callback, 0, c"duplicate".as_ptr(), dev),
            -EBUSY
        );
        assert_eq!(free_irq(77, dev), c"ok".as_ptr().cast());
        assert!(free_irq(77, dev).is_null());
    }
}
#[test]
fn properties_mac_and_error_pointers_keep_original_protocol() {
    reset();
    let mut f = fixture();
    let mut value = 123;
    unsafe {
        assert_eq!(
            of_property_read_u32(&*f.node, c"missing".as_ptr(), &mut value),
            -EINVAL
        );
        assert_eq!(value, 123);
        f.node.properties.push(("fifo-depth", 32));
        assert_eq!(
            of_property_read_u32(&*f.node, c"fifo-depth".as_ptr(), &mut value),
            0
        );
        assert_eq!(value, 32);
        assert_eq!(platform_get_irq(&mut *f.pdev, 1), -ENXIO);
        assert!(IS_ERR(ERR_PTR(-ENOMEM as i64)));
        assert_eq!(PTR_ERR(ERR_PTR(-ENOMEM as i64)), -ENOMEM as i64);
        assert!(!IS_ERR(ptr::null()));
        f.node.addresses = [Some([0; 6]), Some([2, 3, 4, 5, 6, 7]), Some([2; 6]), None];
        assert_eq!(
            of_get_ethdev_address((&*f.node as *const device_node).cast_mut(), f.dev()),
            0
        );
        assert_eq!((*f.dev()).dev_addr, [2, 3, 4, 5, 6, 7]);
    }
}
#[test]
fn unregister_closes_running_device_and_framework_unmaps_after_remove() {
    reset();
    let mut f = fixture();
    f.open();
    unsafe {
        driver::liteeth_remove(&mut *f.pdev);
        assert_eq!(
            STATE.with(|s| {
                let s = s.borrow();
                (s.bound, s.maps)
            }),
            (0, 2)
        );
        assert_eq!(registered_devices(), 0);
        devres_release_all(&mut f.pdev.dev);
    }
    assert_eq!(STATE.with(|s| s.borrow().maps), 0);
    assert_eq!(memory::live(), 0);
    std::mem::forget(f);
}
#[test]
fn logs_and_global_rate_limit_keep_call_arguments() {
    reset();
    let f = fixture();
    STATE.with(|s| s.borrow_mut().tick = 100_000);
    for _ in 0..10 {
        assert!(net_ratelimit() != 0);
    }
    assert!(net_ratelimit() == 0);
    STATE.with(|s| s.borrow_mut().tick = 106_000);
    assert!(net_ratelimit() != 0);
    unsafe {
        netdev_err!(f.dev(), "error {}\n", 7);
        dev_dbg!(&f.pdev.dev, "default {}\n", 2);
    }
    assert!(STATE.with(|s| s.borrow().logs.iter().any(|s| s.ends_with("error 7\n"))));
}
#[test]
fn random_mac_uses_seeded_chacha_stream_and_module_keeps_metadata() {
    reset();
    let mut out = [0; 16];
    linux_api::random::seed(&[0; 32]);
    linux_api::random::bytes(&mut out);
    assert_eq!(
        out,
        [
            0x76, 0xb8, 0xe0, 0xad, 0xa0, 0xf1, 0x3d, 0x90, 0x40, 0x5d, 0x6a, 0xe5, 0x53, 0x86,
            0xbd, 0x28
        ]
    );
    assert_eq!(driver::MODULE_LICENSE_VALUE, "GPL");
    let f = fixture();
    unsafe {
        eth_hw_addr_random(f.dev());
        let a = (*f.dev()).dev_addr;
        eth_hw_addr_random(f.dev());
        assert_ne!(a, (*f.dev()).dev_addr);
        assert_eq!((*f.dev()).dev_addr[0] & 3, 2);
    }
}

#[test]
fn mmio_overlap_is_busy_and_unaligned_resource_keeps_offset() {
    reset();
    let mut a = Fixture::new(vec![], None);
    let mut b = Fixture::new(vec![], None);
    a.pdev.resources[0].base = 0x1003;
    a.pdev.resources[0].size = 4;
    unsafe {
        let addr = devm_platform_ioremap_resource_byname(&mut *a.pdev, c"mac".as_ptr());
        let base = STATE.with(|s| s.borrow().csr.as_ptr() as usize);
        assert_eq!(addr as usize, base + 3);
        assert_eq!(
            PTR_ERR(devm_platform_ioremap_resource_byname(
                &mut *b.pdev,
                c"mac".as_ptr()
            )),
            -EBUSY as i64
        );
        devres_release_all(&mut a.pdev.dev);
        let addr = devm_platform_ioremap_resource_byname(&mut *b.pdev, c"mac".as_ptr());
        assert!(!IS_ERR(addr));
        devres_release_all(&mut b.pdev.dev);
    }
    STATE.with(|s| assert_eq!(s.borrow().maps, 0));
}
#[test]
fn irq_free_waits_for_running_callback_and_cancels_pending_wait() {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Condvar, Mutex, OnceLock,
    };
    struct State {
        mode: AtomicUsize,
        entered: Mutex<Option<mpsc::Sender<()>>>,
        unbound: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<bool>,
        changed: Condvar,
        destroyed: AtomicBool,
        callbacks: AtomicUsize,
    }
    static G: OnceLock<State> = OnceLock::new();
    fn g() -> &'static State {
        G.get().unwrap()
    }
    fn wait(_: u64) -> Result<u64, i32> {
        if g().mode.load(Ordering::Acquire) == 2 {
            g().entered
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .send(())
                .unwrap();
            let mut released = g().release.lock().unwrap();
            while !*released {
                released = g().changed.wait(released).unwrap();
            }
        }
        Ok(1)
    }
    unsafe fn callback(_: i32, _: *mut c_void) -> i32 {
        g().callbacks.fetch_add(1, Ordering::AcqRel);
        g().entered
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(())
            .unwrap();
        let mut released = g().release.lock().unwrap();
        while !*released {
            released = g().changed.wait(released).unwrap();
        }
        IRQ_HANDLED
    }
    fn unbind(_: u32) -> Result<(), i32> {
        g().unbound
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(())
            .unwrap();
        Ok(())
    }
    fn cancel(_: u64) -> Result<(), i32> {
        if g().mode.load(Ordering::Acquire) == 2 {
            *g().release.lock().unwrap() = true;
            g().changed.notify_all();
        }
        Ok(())
    }
    fn destroy(_: u64) -> Result<(), i32> {
        g().destroyed.store(true, Ordering::Release);
        Ok(())
    }
    reset();
    let _ = G.set(State {
        mode: AtomicUsize::new(1),
        entered: Mutex::new(None),
        unbound: Mutex::new(None),
        release: Mutex::new(false),
        changed: Condvar::new(),
        destroyed: AtomicBool::new(false),
        callbacks: AtomicUsize::new(0),
    });
    let mut ops = *linux_api::kernel::ops();
    ops.bind = |_| Ok(1);
    ops.wait = wait;
    ops.unbind = unbind;
    ops.cancel_wait = cancel;
    ops.destroy = destroy;
    ops.ack = |_| Ok(());
    ops.yield_now = std::thread::yield_now;
    unsafe {
        linux_api::kernel::install(Box::leak(Box::new(ops)));
    }
    for mode in [1, 2] {
        g().mode.store(mode, Ordering::Release);
        g().destroyed.store(false, Ordering::Release);
        *g().release.lock().unwrap() = false;
        let (et, er) = mpsc::channel();
        *g().entered.lock().unwrap() = Some(et);
        let (ut, ur) = mpsc::channel();
        *g().unbound.lock().unwrap() = Some(ut);
        unsafe {
            assert_eq!(
                request_irq(91, callback, 0, c"sync".as_ptr(), ptr::null_mut()),
                0
            );
        }
        let worker = std::thread::spawn(|| unsafe { dispatch(91).unwrap() });
        er.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        let freeing = std::thread::spawn(|| unsafe {
            free_irq(91, ptr::null_mut());
        });
        ur.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        if mode == 1 {
            assert!(!g().destroyed.load(Ordering::Acquire));
            *g().release.lock().unwrap() = true;
            g().changed.notify_all();
        }
        worker.join().unwrap();
        freeing.join().unwrap();
        assert!(g().destroyed.load(Ordering::Acquire));
        assert_eq!(bindings(), 0);
    }
    assert_eq!(g().callbacks.load(Ordering::Acquire), 1);
    reset();
}
