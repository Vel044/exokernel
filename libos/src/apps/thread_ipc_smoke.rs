//! Thread、Endpoint 和 Notification 的无设备 QEMU 验证。

use core::sync::atomic::{AtomicU64, Ordering};

static ENDPOINT: AtomicU64 = AtomicU64::new(0);
static NOTIFICATION: AtomicU64 = AtomicU64::new(0);

extern "C" fn worker(_: u64, _: u64, ipc_va: u64) -> ! {
    let notification =
        crate::notification::Notification::from_raw(NOTIFICATION.load(Ordering::Acquire));
    let endpoint = crate::ipc::Endpoint::from_raw(ENDPOINT.load(Ordering::Acquire));
    let mut buffer = unsafe { crate::ipc::IpcBuffer::from_va(ipc_va) };

    notification
        .signal(0x2)
        .unwrap_or_else(|error| fail(b"worker signal", error));
    endpoint
        .recv()
        .unwrap_or_else(|error| fail(b"worker recv", error));
    let request = buffer.read();
    if request.label != 0x100 || request.words[0] != 41 || request.reply.0 == 0 {
        fail(b"worker request", 0x401);
    }

    buffer.write(exo_abi::IpcMessage {
        label: 0x101,
        words: [request.words[0] + 1, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    endpoint
        .reply(request.reply)
        .unwrap_or_else(|error| fail(b"worker reply", error));

    endpoint
        .recv()
        .unwrap_or_else(|error| fail(b"worker second recv", error));
    let message = buffer.read();
    if message.label != 0x200 || message.words[0] != 7 || message.reply.0 != 0 {
        fail(b"worker one-way message", 0x402);
    }
    notification
        .signal(0x4)
        .unwrap_or_else(|error| fail(b"worker completion", error));

    // REPLY_RECV 会先把第三个请求的回复交给调用者，再把服务线程
    // 放回 Endpoint 接收队列；主线程随后发送的收尾消息会唤醒这里。
    endpoint
        .recv()
        .unwrap_or_else(|error| fail(b"worker reply-recv request", error));
    let request = buffer.read();
    if request.label != 0x300 || request.words[0] != 9 || request.reply.0 == 0 {
        fail(b"worker reply-recv request", 0x407);
    }
    buffer.write(exo_abi::IpcMessage {
        label: 0x301,
        words: [request.words[0] + 1, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    endpoint
        .reply_recv(request.reply)
        .unwrap_or_else(|error| fail(b"worker reply-recv", error));
    let follow_up = buffer.read();
    if follow_up.label != 0x302 || follow_up.words[0] != 10 || follow_up.reply.0 != 0 {
        fail(b"worker reply-recv follow-up", 0x408);
    }
    notification
        .signal(0x8)
        .unwrap_or_else(|error| fail(b"worker reply-recv completion", error));
    crate::thread::exit(0)
}

pub fn run() -> ! {
    crate::runtime::puts(b"[libos] Thread + IPC + Notification smoke\r\n");
    let notification = crate::notification::Notification::create()
        .unwrap_or_else(|error| fail(b"notif create", error));
    let endpoint =
        crate::ipc::Endpoint::create().unwrap_or_else(|error| fail(b"endpoint create", error));
    NOTIFICATION.store(notification.raw(), Ordering::Release);
    ENDPOINT.store(endpoint.raw(), Ordering::Release);

    let worker_handle =
        crate::thread::spawn(worker, 0, 128).unwrap_or_else(|error| fail(b"thread create", error));
    crate::thread::set_priority(worker_handle, 128)
        .unwrap_or_else(|error| fail(b"thread priority", error));

    let badge = notification
        .wait()
        .unwrap_or_else(|error| fail(b"notif wait", error));
    if badge != 0x2 {
        fail(b"notif badge", badge);
    }
    crate::runtime::puts(b"[libos] Notification wake passed\r\n");

    let mut buffer = crate::ipc::IpcBuffer::initial();
    buffer.write(exo_abi::IpcMessage {
        label: 0x100,
        words: [41, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    endpoint
        .call()
        .unwrap_or_else(|error| fail(b"endpoint call", error));
    let reply = buffer.read();
    if reply.label != 0x101 || reply.words[0] != 42 {
        fail(b"endpoint reply", 0x402);
    }
    crate::runtime::puts(b"[libos] Endpoint Call/Reply passed\r\n");

    buffer.write(exo_abi::IpcMessage {
        label: 0x200,
        words: [7, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    endpoint
        .send()
        .unwrap_or_else(|error| fail(b"endpoint send", error));
    let completion = notification
        .wait()
        .unwrap_or_else(|error| fail(b"worker completion wait", error));
    if completion != 0x4 {
        fail(b"worker completion badge", completion);
    }
    crate::runtime::puts(b"[libos] Endpoint Send/Recv passed\r\n");

    buffer.write(exo_abi::IpcMessage {
        label: 0x300,
        words: [9, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    endpoint
        .call()
        .unwrap_or_else(|error| fail(b"endpoint reply-recv call", error));
    let reply = buffer.read();
    if reply.label != 0x301 || reply.words[0] != 10 || reply.reply.0 != 0 {
        fail(b"endpoint reply-recv reply", 0x409);
    }
    buffer.write(exo_abi::IpcMessage {
        label: 0x302,
        words: [10, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    endpoint
        .send()
        .unwrap_or_else(|error| fail(b"endpoint reply-recv follow-up", error));
    let completion = notification
        .wait()
        .unwrap_or_else(|error| fail(b"reply-recv completion wait", error));
    if completion != 0x8 {
        fail(b"reply-recv completion badge", completion);
    }
    crate::runtime::puts(b"[libos] Endpoint Reply/Recv passed\r\n");

    if crate::thread::set_priority(worker_handle, 128) != Err(exo_abi::SYS_ERR_NOT_FOUND) {
        fail(b"stale thread handle", 0x403);
    }

    notification
        .signal(0x10)
        .unwrap_or_else(|error| fail(b"notif signal", error));
    if notification
        .poll()
        .unwrap_or_else(|error| fail(b"notif poll", error))
        != 0x10
    {
        fail(b"notif poll badge", 0x404);
    }
    // 事件先于 WAIT 到达时，Kernel 直接在本次异常帧中返回 badge；
    // 这个路径不能使用上一次调度保存的旧线程上下文。
    notification
        .signal(0x20)
        .unwrap_or_else(|error| fail(b"notif pending signal", error));
    if notification
        .wait()
        .unwrap_or_else(|error| fail(b"notif pending wait", error))
        != 0x20
    {
        fail(b"notif pending badge", 0x405);
    }
    notification
        .destroy()
        .unwrap_or_else(|error| fail(b"notif destroy", error));
    if notification.poll() != Err(exo_abi::SYS_ERR_NOT_FOUND) {
        fail(b"stale notification", 0x406);
    }

    crate::runtime::puts(b"[libos] Thread + IPC + Notification smoke passed\r\n");
    crate::runtime::exit(0)
}

fn fail(stage: &[u8], error: u64) -> ! {
    crate::runtime::puts(b"[libos] thread smoke failed: ");
    crate::runtime::puts(stage);
    crate::runtime::puts(b" error=");
    crate::runtime::hex(error);
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(0x400)
}
