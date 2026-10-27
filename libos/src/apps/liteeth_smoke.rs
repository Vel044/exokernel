//! LiteEth C/Rust 两版本共用的 L2 回显验收，不实现 IP、ARP、socket 或 POSIX read/write。
//! 应用收到处理完成通知后取帧；原生 Rust 版由独立 IRQ 线程自动派发回调。
#[cfg(feature = "app-liteeth-smoke")]
use crate::drivers::linux_liteeth::Driver;
#[cfg(feature = "app-liteeth-rust-smoke")]
use crate::drivers::liteeth::Driver;
use crate::{drivers::platform::DeviceInfo, runtime};
fn fail(stage: &[u8], error: i32) -> ! {
    runtime::puts(b"[liteeth] FAIL ");
    runtime::puts(stage);
    runtime::hex(error as u64);
    runtime::puts(b"\r\n");
    runtime::exit(0x702)
}
fn closed_tx_accepted(driver: &mut Driver) -> bool {
    driver.send(&[0; 60]).is_ok()
}
fn transmit(driver: &mut Driver, frame: &[u8], stage: &[u8]) {
    while !driver.send(frame).unwrap_or_else(|e| fail(stage, e)) {
        driver
            .wait_irq()
            .unwrap_or_else(|e| fail(b"TX retry IRQ", e));
    }
}
pub fn run(info: &DeviceInfo) -> ! {
    #[cfg(feature = "app-liteeth-smoke")]
    runtime::puts(b"[liteeth] implementation=c-reference\r\n");
    #[cfg(feature = "app-liteeth-rust-smoke")]
    runtime::puts(b"[liteeth] implementation=rust-native\r\n");
    let resource = info.liteeth.unwrap_or_else(|| fail(b"DTB missing", -1));
    // 验证真实 EL1 拒绝未授权 PA 时的失败回滚；两版本随后都必须能重新 attach。
    for buffer_failure in [false, true] {
        let mut invalid = resource;
        if buffer_failure {
            invalid.buffer.base = 0x0b04_0000;
        } else {
            invalid.mac.base = 0x0b03_0000;
        }
        if Driver::attach(invalid).is_ok() {
            fail(b"unauthorized mapping accepted", -1);
        }
    }
    runtime::puts(b"[liteeth] mapping rollback passed\r\n");
    // 所有网络硬件访问仍在 EL0；此场景使用已有 SYS_PUTS 诊断。
    let mut driver = Driver::attach(resource).unwrap_or_else(|e| fail(b"probe", e));
    if Driver::attach(resource).is_ok() {
        fail(b"duplicate attach accepted", -1);
    }
    if closed_tx_accepted(&mut driver) {
        fail(b"closed TX accepted", -1);
    }
    driver.open().unwrap_or_else(|e| fail(b"open", e));
    if driver.open().is_ok() {
        fail(b"duplicate open accepted", -1);
    }
    runtime::puts(b"[liteeth] READY raw Ethernet echo\r\n");
    #[cfg(feature = "app-liteeth-rust-smoke")]
    {
        // 尚未调用 wait_irq；验证注册的 IRQ 线程能够自动接收第一帧。
        let deadline = runtime::counter() + runtime::counter_frequency() * 2;
        while driver
            .stats()
            .unwrap_or_else(|e| fail(b"automatic IRQ stats", e))
            .rx_packets
            == 0
        {
            if runtime::counter() > deadline {
                fail(b"automatic IRQ callback before WAIT", -1);
            }
        }
        runtime::puts(b"[liteeth] automatic IRQ callback before WAIT passed\r\n");
    }
    let mut frame = [0u8; 2048];
    let mut echoed = 0;
    let mut bytes = 0u64;
    loop {
        driver.wait_irq().unwrap_or_else(|e| fail(b"IRQ", e));
        let n = driver.receive(&mut frame);
        if n == 0 {
            continue;
        }
        transmit(&mut driver, &frame[..n], b"TX");
        echoed += 1;
        bytes += n as u64;
        if echoed == 16 {
            break;
        }
    }
    let oversized = [0u8; 2049];
    transmit(&mut driver, &oversized, b"oversize");
    let stats = driver.stats().unwrap_or_else(|e| fail(b"stats", e));
    if stats.rx_packets != 16
        || stats.tx_packets != 16
        || stats.rx_bytes != bytes
        || stats.tx_bytes != bytes
        || stats.rx_errors != 0
        || stats.tx_errors != 1
        || stats.tx_dropped != 1
    {
        fail(b"stats mismatch", -1);
    }
    driver.stop().unwrap_or_else(|e| fail(b"stop", e));
    if closed_tx_accepted(&mut driver) {
        fail(b"stopped TX accepted", -1);
    }
    // 重开验证 request_irq/free_irq 及 Notification 旧 badge 不破坏生命周期。
    driver.open().unwrap_or_else(|e| fail(b"reopen", e));
    runtime::puts(b"[liteeth] REOPEN ready\r\n");
    loop {
        driver.wait_irq().unwrap_or_else(|e| fail(b"reopen IRQ", e));
        let n = driver.receive(&mut frame);
        if n == 0 {
            continue;
        }
        transmit(&mut driver, &frame[..n], b"reopen TX");
        bytes += n as u64;
        break;
    }
    let stats = driver.stats().unwrap_or_else(|e| fail(b"final stats", e));
    if stats.rx_packets != 17
        || stats.tx_packets != 17
        || stats.rx_bytes != bytes
        || stats.tx_bytes != bytes
        || stats.rx_errors != 0
        || stats.tx_errors != 1
        || stats.rx_dropped != 0
        || stats.tx_dropped != 1
    {
        fail(b"final stats mismatch", -1);
    }
    runtime::puts(alloc::format!("[liteeth] STATS {{\"rx_packets\":{},\"tx_packets\":{},\"rx_bytes\":{},\"tx_bytes\":{},\"rx_errors\":{},\"tx_errors\":{},\"rx_dropped\":{},\"tx_dropped\":{}}}\r\n",stats.rx_packets,stats.tx_packets,stats.rx_bytes,stats.tx_bytes,stats.rx_errors,stats.tx_errors,stats.rx_dropped,stats.tx_dropped).as_bytes());
    driver.remove().unwrap_or_else(|e| fail(b"remove", e));
    // 同一任务重新 probe，再 remove，验证 devm 映射/堆资源已释放。
    Driver::attach(resource)
        .unwrap_or_else(|e| fail(b"reprobe", e))
        .remove()
        .unwrap_or_else(|e| fail(b"remove twice", e));
    runtime::puts(b"[liteeth] PASS RX=17 TX=17 stop/reopen/remove/reprobe\r\n");
    runtime::exit(0)
}
