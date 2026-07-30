//! 综合实验中的Thread、Endpoint、Reply和Notification子测试。

use core::sync::atomic::{AtomicU64, Ordering};

// 当前两个线程共享同一个EL0地址空间，因此可以通过全局变量传递Handle。
// 使用Release/Acquire保证Worker看到Handle时，主线程此前的写入已经可见。
static ENDPOINT: AtomicU64 = AtomicU64::new(0);
static NOTIFICATION: AtomicU64 = AtomicU64::new(0);

// Kernel首次调度新线程时按AArch64 ABI注入三个参数：
// x0=业务参数，x1=ThreadHandle，x2=该线程独占的IPC Buffer虚拟地址。
extern "C" fn worker(_arg: u64, _thread_handle: u64, ipc_va: u64) -> ! {
    // Handle只是任务内的对象标识；真正的对象、代数和所有权保存在Kernel。
    let notification =
        crate::notification::Notification::from_raw(NOTIFICATION.load(Ordering::Acquire));
    let endpoint = crate::ipc::Endpoint::from_raw(ENDPOINT.load(Ordering::Acquire));
    // 每个线程使用不同的IPC Buffer页，避免主线程和Worker同时覆盖消息正文。
    let mut buffer = unsafe { crate::ipc::IpcBuffer::from_va(ipc_va) };

    // 异步通知主线程“Worker已经启动”。signal本身不等待接收方。
    notification
        .signal(0x2)
        .unwrap_or_else(|error| fail(b"worker signal", error));
    // 在同步Endpoint上等待第一个请求；没有发送者时本线程进入Blocked。
    endpoint
        .recv()
        .unwrap_or_else(|error| fail(b"worker recv", error));
    // recv返回后，Kernel已经把发送者消息复制到Worker的IPC Buffer。
    let request = buffer.read();
    // CALL请求必须携带一次性的ReplyHandle，普通SEND则不会携带。
    if request.label != 0x100 || request.words[0] != 41 || request.reply.0 == 0 {
        fail(b"worker request", 0x401);
    }

    // 将41加一后写回当前线程IPC Buffer，再用ReplyHandle唤醒CALL方。
    buffer.write(exo_abi::IpcMessage {
        label: 0x101,
        words: [request.words[0] + 1, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    // ReplyRecv必须先校验Endpoint。伪造Endpoint失败后，一次性ReplyHandle
    // 仍应有效，下面真正的reply才能正常唤醒主线程。
    let invalid_endpoint = crate::ipc::Endpoint::from_raw(0);
    if invalid_endpoint.reply_recv(request.reply) != Err(exo_abi::SYS_ERR_INVALID) {
        fail(b"reply-recv validation", 0x40a);
    }
    endpoint
        .reply(request.reply)
        .unwrap_or_else(|error| fail(b"worker reply", error));

    // 第二轮验证单向SEND/RECV：Worker接收消息，但不产生ReplyHandle。
    endpoint
        .recv()
        .unwrap_or_else(|error| fail(b"worker second recv", error));
    let message = buffer.read();
    if message.label != 0x200 || message.words[0] != 7 || message.reply.0 != 0 {
        fail(b"worker one-way message", 0x402);
    }
    // Endpoint消息处理完成后，再用独立的异步Notification通知主线程。
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
    // 只结束当前Worker线程；主线程和整个EL0任务继续运行。
    crate::thread::exit(0)
}

// 收到CALL后直接退出，用来验证Kernel会撤销Server持有的一次性ReplyHandle，
// 并以错误唤醒Caller，而不是让Caller永远停留在Blocked状态。
extern "C" fn abandoning_worker(endpoint_handle: u64, _thread_handle: u64, ipc_va: u64) -> ! {
    let endpoint = crate::ipc::Endpoint::from_raw(endpoint_handle);
    let buffer = unsafe { crate::ipc::IpcBuffer::from_va(ipc_va) };
    endpoint
        .recv()
        .unwrap_or_else(|error| fail(b"abandoning recv", error));
    let request = buffer.read();
    if request.label != 0x400 || request.reply.0 == 0 {
        fail(b"abandoning request", 0x40b);
    }
    crate::thread::exit(0)
}

pub fn run() {
    crate::runtime::puts(b"[libos] Thread + IPC + Notification smoke\r\n");
    // 创建一个异步事件对象和一个同步会合对象，返回的都是不透明Handle。
    let notification = crate::notification::Notification::create()
        .unwrap_or_else(|error| fail(b"notif create", error));
    let endpoint =
        crate::ipc::Endpoint::create().unwrap_or_else(|error| fail(b"endpoint create", error));
    // 在创建Worker之前发布Handle，防止Worker先运行却读到0。
    NOTIFICATION.store(notification.raw(), Ordering::Release);
    ENDPOINT.store(endpoint.raw(), Ordering::Release);

    let worker_thread =
        crate::thread::Thread::spawn(worker, 0, crate::thread::ThreadConfig::new(0, 40, 40))
            .unwrap_or_else(|error| fail(b"thread create", error));
    let worker_handle = worker_thread.handle();

    // Worker尚未signal时，主线程在这里阻塞，调度器转而运行Worker。
    let badge = notification
        .wait()
        .unwrap_or_else(|error| fail(b"notif wait", error));
    if badge != 0x2 {
        fail(b"notif badge", badge);
    }
    crate::runtime::puts(b"[libos] Notification wake passed\r\n");
    // Worker已经在Endpoint的RECV队列中阻塞。此时销毁必须返回BUSY，
    // 否则该Worker将永远等待一个已经不存在的对象。
    if endpoint.destroy() != Err(exo_abi::SYS_ERR_BUSY) {
        fail(b"busy endpoint destroy", 0x40e);
    }

    // 主线程拥有启动时固定的IPC Buffer，消息正文不直接作为syscall参数传递。
    let mut buffer = crate::ipc::IpcBuffer::initial();
    buffer.write(exo_abi::IpcMessage {
        label: 0x100,
        words: [41, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    // CALL会一直阻塞到Worker执行REPLY；返回时同一Buffer中已经是回复消息。
    endpoint
        .call()
        .unwrap_or_else(|error| fail(b"endpoint call", error));
    let reply = buffer.read();
    if reply.label != 0x101 || reply.words[0] != 42 {
        fail(b"endpoint reply", 0x402);
    }
    crate::runtime::puts(b"[libos] Endpoint Call/Reply passed\r\n");

    // SEND只要求与RECV配对，不等待服务方再回复一个结果。
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

    // Worker已经THREAD_EXIT，旧slot的generation失效，优先级操作必须拒绝旧Handle。
    if crate::runtime::svc(exo_abi::SYS_THREAD_SET_PRIORITY, worker_handle.0, 1, 0)
        != exo_abi::SYS_ERR_NOT_FOUND
    {
        fail(b"stale thread handle", 0x403);
    }

    // 新服务线程取得CALL请求后不回复而直接退出。Kernel必须让本次CALL
    // 返回NOT_FOUND，同时回收Reply对象；这也是服务崩溃恢复的最小语义。
    let abandoning_thread = crate::thread::Thread::spawn(
        abandoning_worker,
        endpoint.raw(),
        crate::thread::ThreadConfig::new(0, 40, 40),
    )
    .unwrap_or_else(|error| fail(b"abandoning thread create", error));
    let abandoning_handle = abandoning_thread.handle();
    buffer.write(exo_abi::IpcMessage {
        label: 0x400,
        words: [0, 0, 0, 0],
        reply: exo_abi::ReplyHandle(0),
    });
    if endpoint.call() != Err(exo_abi::SYS_ERR_NOT_FOUND) {
        fail(b"abandoned call", 0x40c);
    }
    if crate::runtime::svc(exo_abi::SYS_THREAD_SET_PRIORITY, abandoning_handle.0, 1, 0)
        != exo_abi::SYS_ERR_NOT_FOUND
    {
        fail(b"abandoning stale thread", 0x40d);
    }
    crate::runtime::puts(b"[libos] Endpoint peer-exit recovery passed\r\n");

    // 所有Worker、等待者和一次性ReplyHandle都已退出或消费，现在Endpoint
    // 可以释放。generation递增后，旧Handle上的SEND必须被立即拒绝。
    endpoint
        .destroy()
        .unwrap_or_else(|error| fail(b"endpoint destroy", error));
    if endpoint.send() != Err(exo_abi::SYS_ERR_NOT_FOUND) {
        fail(b"stale endpoint", 0x40f);
    }
    crate::runtime::puts(b"[libos] Endpoint destroy passed\r\n");

    notification
        .signal(0x10)
        .unwrap_or_else(|error| fail(b"notif signal", error));
    // POLL不阻塞：有pending badge就取走，没有事件则立即返回0。
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
    // destroy后再次使用相同数值Handle，generation校验必须返回NOT_FOUND。
    if notification.poll() != Err(exo_abi::SYS_ERR_NOT_FOUND) {
        fail(b"stale notification", 0x406);
    }

    crate::runtime::puts(b"[libos] Thread + IPC + Notification smoke passed\r\n");
}

fn fail(stage: &[u8], error: u64) -> ! {
    crate::runtime::puts(b"[libos] thread smoke failed: ");
    crate::runtime::puts(stage);
    crate::runtime::puts(b" error=");
    crate::runtime::hex(error);
    crate::runtime::puts(b"\r\n");
    crate::runtime::exit(0x400)
}
