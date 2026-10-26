//! 同名封装的独立复用测试：两个非 LiteEth 设备、回调生命周期、失败回滚与线性 skb 边界。
#[path = "../libos/src/linux_api/mod.rs"]
mod linux_api;
use linux_api::{
    error::*, interrupt::*, io::*, module::*, netdevice::*, platform::*, printk::*, skbuff::*,
};
use std::{cell::RefCell, rc::Rc};

#[test]
fn network_instances_keep_private_queue_link_and_stats_independent() {
    let mut a = devm_alloc_etherdev("mock-a", 7);
    let mut b = devm_alloc_etherdev("mock-b", 11);
    *netdev_priv(&mut a) = 9;
    assert_eq!(*netdev_priv(&mut b), 11);
    SET_NETDEV_DEV(&mut a, "platform-a");
    assert_eq!(a.parent, Some("platform-a"));
    assert_eq!(b.parent, None);
    register_netdev(&mut a).unwrap();
    register_netdev(&mut b).unwrap();
    assert_eq!(register_netdev(&mut a), Err(EBUSY));
    netif_carrier_on(&mut a);
    netif_start_queue(&mut a);
    assert!(a.carrier);
    assert!(!b.carrier);
    assert!(!netif_queue_stopped(&a));
    assert!(netif_queue_stopped(&b));
    assert_eq!(unregister_netdev(&mut a), Err(EBUSY));
    netif_stop_queue(&mut a);
    netif_carrier_off(&mut a);
    netif_wake_queue(&mut a);
    assert!(!a.stopped);
    netif_stop_queue(&mut a);
    dev_sw_netstats_rx_add(&mut a, 60);
    dev_sw_netstats_tx_add(&mut a, 2, 120);
    a.stats.rx_errors = 3;
    let mut stats = rtnl_link_stats64::default();
    netdev_stats_to_stats64(&mut stats, &a.stats);
    dev_fetch_sw_netstats(&mut stats, &a.tstats);
    assert_eq!(
        (
            stats.rx_packets,
            stats.rx_bytes,
            stats.tx_packets,
            stats.tx_bytes,
            stats.rx_errors
        ),
        (1, 60, 2, 120, 3)
    );
    assert_eq!(b.stats, rtnl_link_stats64::default());
    assert_eq!(b.tstats.rx_packets, 0);
    unregister_netdev(&mut a).unwrap();
    assert_eq!(unregister_netdev(&mut a), Err(EINVAL));
    unregister_netdev(&mut b).unwrap();
    eth_hw_addr_random(&mut a, [255; 6]);
    assert_eq!(a.address[0] & 3, 2);
}
fn packet<const N: usize>(bytes: &[u8]) -> sk_buff<N> {
    let dev = devm_alloc_etherdev("unrelated", ());
    let mut skb = netdev_alloc_skb_ip_align(&dev, bytes.len()).unwrap();
    skb_put(&mut skb, bytes.len())
        .unwrap()
        .copy_from_slice(bytes);
    skb
}
#[test]
fn skb_exact_capacity_alignment_and_failed_put_do_not_extend_packet() {
    let dev = devm_alloc_etherdev("other", ());
    assert!(netdev_alloc_skb_ip_align::<_, 64>(&dev, 65).is_err());
    let mut skb = netdev_alloc_skb_ip_align::<_, 64>(&dev, 60).unwrap();
    let data = skb_put(&mut skb, 60).unwrap();
    assert_eq!((data.as_ptr() as usize + 14) % 4, 0);
    assert!(skb_put(&mut skb, 1).is_err());
    assert_eq!(skb.frame().len(), 60);
    assert!(skb_put(&mut skb, usize::MAX).is_err());
    dev_kfree_skb_any(skb);
}
#[test]
fn protocol_classification_keeps_full_l2_frame_for_delivery() {
    let dev = devm_alloc_etherdev("other", ());
    for (field, payload, expected) in [
        (0x0800u16, [0, 0], 0x0800),
        (40, [255, 255], 1),
        (40, [0, 0], 4),
    ] {
        let mut frame = [0; 60];
        frame[12..14].copy_from_slice(&field.to_be_bytes());
        frame[14..16].copy_from_slice(&payload);
        let mut skb = packet::<128>(&frame);
        assert_eq!(eth_type_trans(&mut skb, &dev), Ok(expected));
        assert_eq!(skb.data().len(), 46);
        assert_eq!(skb.frame(), &frame);
        assert!(eth_type_trans(&mut skb, &dev).is_err());
    }
    let mut short = packet::<64>(&[0; 13]);
    assert!(eth_type_trans(&mut short, &dev).is_err());
}
#[test]
fn receive_queue_is_fifo_consumes_drop_and_preserves_small_buffer_retry() {
    let mut queue = RxQueue::<64, 2>::new();
    netif_rx(&mut queue, packet(&[1; 60])).unwrap();
    netif_rx(&mut queue, packet(&[2; 60])).unwrap();
    assert_eq!(netif_rx(&mut queue, packet(&[3; 60])), Err(EBUSY));
    assert_eq!(queue.receive(&mut [0; 59]), Err(EINVAL));
    let mut out = [0; 64];
    assert_eq!(queue.receive(&mut out), Ok(60));
    assert_eq!(&out[..60], &[1; 60]);
    netif_rx(&mut queue, packet(&[4; 60])).unwrap();
    queue.receive(&mut out).unwrap();
    assert_eq!(&out[..60], &[2; 60]);
    queue.receive(&mut out).unwrap();
    assert_eq!(&out[..60], &[4; 60]);
    assert_eq!(queue.receive(&mut out), Ok(0));
    let mut empty = RxQueue::<64, 0>::new();
    assert_eq!(netif_rx(&mut empty, packet(&[0; 60])), Err(EBUSY));
}
#[derive(Clone)]
struct Backend {
    events: Rc<RefCell<Vec<&'static str>>>,
    fail_bind: bool,
    fail_free: bool,
    pending: bool,
}
impl IrqBackend for Backend {
    fn bind(&mut self, _: u32) -> Result<(), i32> {
        self.events.borrow_mut().push("bind");
        if self.fail_bind {
            Err(-5)
        } else {
            Ok(())
        }
    }
    fn wait(&mut self) -> Result<bool, i32> {
        self.events.borrow_mut().push("wait");
        Ok(self.pending)
    }
    fn ack(&mut self, _: u32) -> Result<(), i32> {
        self.events.borrow_mut().push("ack");
        Ok(())
    }
    fn unbind(&mut self, _: u32) -> Result<(), i32> {
        self.events.borrow_mut().push("free");
        if self.fail_free {
            Err(-5)
        } else {
            Ok(())
        }
    }
}
fn handler(events: &mut Rc<RefCell<Vec<&'static str>>>) {
    events.borrow_mut().push("callback");
}
fn backend() -> Backend {
    Backend {
        events: Rc::new(RefCell::new(vec![])),
        fail_bind: false,
        fail_free: false,
        pending: true,
    }
}
#[test]
fn irq_callback_runs_between_wait_and_ack_and_stops_after_free() {
    let b = backend();
    let mut context = b.events.clone();
    let mut r = IrqRegistration::new(b);
    request_irq(&mut r, 77, handler, 0, "not-liteeth").unwrap();
    assert_eq!(request_irq(&mut r, 77, handler, 0, "duplicate"), Err(EBUSY));
    r.wait_and_dispatch(&mut context).unwrap();
    free_irq(&mut r).unwrap();
    assert_eq!(r.wait_and_dispatch(&mut context), Err(EINVAL));
    assert_eq!(
        *context.borrow(),
        vec!["bind", "wait", "callback", "ack", "free"]
    );
}
#[test]
fn irq_failed_bind_or_unsupported_flags_never_install_callback() {
    let mut b = backend();
    b.fail_bind = true;
    let events = b.events.clone();
    let mut r = IrqRegistration::new(b);
    assert_eq!(
        request_irq(&mut r, 7, handler, 1, "shared"),
        Err(EOPNOTSUPP)
    );
    assert!(events.borrow().is_empty());
    assert_eq!(request_irq(&mut r, 7, handler, 0, "fail"), Err(-5));
    assert!(!r.is_bound());
}
#[test]
fn irq_failed_free_preserves_binding_and_can_retry() {
    // 本测试通过后端共享开关模拟一次失败，不暴露注册层私有字段。
    struct Retry {
        attempts: Rc<RefCell<usize>>,
    }
    impl IrqBackend for Retry {
        fn bind(&mut self, _: u32) -> Result<(), i32> {
            Ok(())
        }
        fn wait(&mut self) -> Result<bool, i32> {
            Ok(false)
        }
        fn ack(&mut self, _: u32) -> Result<(), i32> {
            panic!("无 badge 不应 ACK")
        }
        fn unbind(&mut self, _: u32) -> Result<(), i32> {
            let mut n = self.attempts.borrow_mut();
            *n += 1;
            if *n == 1 {
                Err(-5)
            } else {
                Ok(())
            }
        }
    }
    let mut r = IrqRegistration::new(Retry {
        attempts: Rc::new(RefCell::new(0)),
    });
    request_irq(
        &mut r,
        9,
        |_: &mut ()| panic!("无 badge 不应回调"),
        0,
        "retry",
    )
    .unwrap();
    r.wait_and_dispatch(&mut ()).unwrap();
    assert_eq!(free_irq(&mut r), Err(-5));
    assert!(r.is_bound());
    free_irq(&mut r).unwrap();
    assert!(!r.is_bound());
}
#[test]
fn irq_registrations_are_independent_and_drop_unbinds_once() {
    let a = backend();
    let b = backend();
    let ea = a.events.clone();
    let eb = b.events.clone();
    let mut ra = IrqRegistration::new(a);
    let mut rb = IrqRegistration::new(b);
    request_irq(&mut ra, 7, handler, 0, "a").unwrap();
    request_irq(&mut rb, 8, handler, 0, "b").unwrap();
    ra.wait_and_dispatch(&mut ea.clone()).unwrap();
    assert_eq!(*eb.borrow(), vec!["bind"]);
    drop(ra);
    drop(rb);
    assert_eq!(ea.borrow().iter().filter(|v| **v == "free").count(), 1);
    assert_eq!(*eb.borrow(), vec!["bind", "free"]);
}
#[test]
fn platform_resources_properties_private_data_and_mac_are_generic() {
    let resources = [NamedResource {
        name: "control",
        base: 0x1000,
        size: 4096,
        user_va: 0x8000,
    }];
    let mut p = platform_device {
        name: "different",
        compatible: "vendor,other",
        resources: &resources,
        irqs: &[73, 74],
        properties: &[("fifo-depth", 32)],
        address: Some([2, 1, 2, 3, 4, 5]),
        drvdata: None,
    };
    platform_set_drvdata(&mut p, 18u32);
    platform_set_drvdata(&mut p, 19);
    assert_eq!(*platform_get_drvdata(&p).unwrap(), 19);
    assert_eq!(platform_get_irq(&p, 1), Ok(74));
    assert_eq!(platform_get_irq(&p, 2), Err(ENXIO));
    assert_eq!(of_property_read_u32(&p, "fifo-depth"), Ok(32));
    assert_eq!(of_property_read_u32(&p, "litex,slot-size"), Err(ENXIO));
    let mut net = devm_alloc_etherdev("different", ());
    of_get_ethdev_address(&p, &mut net).unwrap();
    assert_eq!(net.address, [2, 1, 2, 3, 4, 5]);
    p.address = Some([255; 6]);
    assert_eq!(of_get_ethdev_address(&p, &mut net), Err(EINVAL));
    // MMIO 建立、失败回滚和解除由 QEMU 用真实 MAP/UNMAP 系统调用验收。
}
#[test]
fn mmio_views_use_csr32_and_checked_volatile_byte_copies() {
    let mut words = [0u32; 8];
    // SAFETY: 测试数组存活且独占，不与普通数组访问并发；视图不超出实际数组。
    let region = unsafe { IoRegion::new(words.as_mut_ptr().cast(), 32) };
    litex_write32(&region, 0, 0x12345678);
    assert_eq!(litex_read32(&region, 0), 0x12345678);
    memcpy_toio(&region, 9, &[1, 2, 3]);
    let mut out = [0; 3];
    memcpy_fromio(&region, 9, &mut out);
    assert_eq!(out, [1, 2, 3]);
}
#[test]
#[should_panic]
fn mmio_rejects_out_of_bounds_before_pointer_access() {
    let mut words = [0u32; 2];
    let region = unsafe { IoRegion::new(words.as_mut_ptr().cast(), 8) };
    litex_write32(&region, 8, 0);
}
#[test]
fn errors_logging_rate_limits_and_metadata_keep_meaning() {
    let error: Result<(), i32> = Err(-5);
    assert!(IS_ERR(&error));
    assert_eq!(PTR_ERR(&error), -5);
    assert_eq!(PTR_ERR(&Ok(())), 0);
    assert!(unlikely(true));
    struct Logs(Vec<String>);
    impl LogSink for Logs {
        fn write(&mut self, l: &str, d: &str, m: std::fmt::Arguments<'_>) {
            self.0.push(format!("{l}/{d}/{m}"));
        }
    }
    let mut logs = Logs(vec![]);
    dev_dbg(&mut logs, "a", format_args!("{}", 1));
    dev_err(&mut logs, "a", format_args!("bad"));
    netdev_err(&mut logs, "b", format_args!("rx"));
    netdev_info(&mut logs, "b", format_args!("ready"));
    assert_eq!(
        logs.0,
        vec!["debug/a/1", "error/a/bad", "error/b/rx", "info/b/ready"]
    );
    let mut a = RateLimit::new(10, 1);
    let mut b = RateLimit::new(10, 1);
    assert!(net_ratelimit(&mut a, 0));
    assert!(!net_ratelimit(&mut a, 1));
    assert!(net_ratelimit(&mut b, 1));
    assert!(net_ratelimit(&mut a, 10));
    assert_eq!(MODULE_AUTHOR("author").value, "author");
    assert_eq!(MODULE_DESCRIPTION("driver").key, "description");
    assert_eq!(MODULE_LICENSE("GPL").value, "GPL");
}
#[test]
fn platform_driver_match_probe_remove_runs_once_and_rejects_unknown_bus() {
    type Counter = Rc<RefCell<usize>>;
    fn probe(p: &platform_device<'_, Counter>) -> Result<Counter, i32> {
        let c = platform_get_drvdata(p)?.clone();
        *c.borrow_mut() += 1;
        Ok(c)
    }
    fn remove(c: Counter) -> Result<(), i32> {
        *c.borrow_mut() -= 1;
        Ok(())
    }
    fn driver(kind: &'static str) -> platform_driver<Counter, Counter> {
        platform_driver {
            table: MODULE_DEVICE_TABLE(kind, &["vendor,a", "vendor,b"]),
            probe,
            remove,
        }
    }
    let count = Rc::new(RefCell::new(0));
    let mut p = platform_device {
        name: "other",
        compatible: "vendor,a",
        resources: &[],
        irqs: &[],
        properties: &[],
        address: None,
        drvdata: Some(count.clone()),
    };
    let mut binding = module_platform_driver(driver("of"), &p).unwrap();
    assert_eq!(*binding.device().borrow(), 1);
    binding.remove().unwrap();
    assert_eq!(*count.borrow(), 0);
    p.compatible = "vendor,b";
    drop(module_platform_driver(driver("of"), &p).unwrap());
    assert_eq!(*count.borrow(), 0);
    p.compatible = "vendor,none";
    assert!(module_platform_driver(driver("of"), &p).is_err());
    assert_eq!(*count.borrow(), 0);
    p.compatible = "vendor,a";
    assert!(module_platform_driver(driver("pci"), &p).is_err());
}
