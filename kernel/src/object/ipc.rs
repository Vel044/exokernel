//! Endpoint/Reply 同步 IPC 与 Notification 异步通知。

use crate::{thread, trap::TrapFrame};

const MAX_ENDPOINTS: usize = 16;
const MAX_NOTIFICATIONS: usize = 32;
const MAX_REPLIES: usize = 16;
const MAX_WAITERS: usize = thread::MAX_THREADS;
const MAX_GENERATION: u32 = 0x7fff_ffff;

#[derive(Clone, Copy)]
struct WaitQueue {
    slots: [u8; MAX_WAITERS],
    count: usize,
}

impl WaitQueue {
    const EMPTY: Self = Self {
        slots: [0; MAX_WAITERS],
        count: 0,
    };

    fn push(&mut self, slot: usize) -> bool {
        if self.count == MAX_WAITERS || self.slots[..self.count].contains(&(slot as u8)) {
            return false;
        }
        self.slots[self.count] = slot as u8;
        self.count += 1;
        true
    }

    fn pop(&mut self) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let slot = self.slots[0] as usize;
        let mut index = 1usize;
        while index < self.count {
            self.slots[index - 1] = self.slots[index];
            index += 1;
        }
        self.count -= 1;
        Some(slot)
    }

    fn remove(&mut self, slot: usize) {
        let mut index = 0usize;
        while index < self.count {
            if self.slots[index] as usize == slot {
                self.count -= 1;
                self.slots[index] = self.slots[self.count];
                return;
            }
            index += 1;
        }
    }
}

#[derive(Clone, Copy)]
struct PendingSender {
    slot: u8,
    is_call: bool,
    message: exo_abi::IpcMessage,
}

impl PendingSender {
    const EMPTY: Self = Self {
        slot: 0xff,
        is_call: false,
        message: exo_abi::IpcMessage {
            label: 0,
            words: [0; exo_abi::IPC_MESSAGE_WORDS],
            reply: exo_abi::ReplyHandle(0),
        },
    };
}

#[derive(Clone, Copy)]
struct Endpoint {
    owner: u32,
    generation: u32,
    receivers: WaitQueue,
    senders: [PendingSender; MAX_WAITERS],
    sender_count: usize,
}

impl Endpoint {
    const EMPTY: Self = Self {
        owner: 0,
        generation: 1,
        receivers: WaitQueue::EMPTY,
        senders: [PendingSender::EMPTY; MAX_WAITERS],
        sender_count: 0,
    };

    fn push_sender(&mut self, sender: PendingSender) -> bool {
        if self.sender_count == MAX_WAITERS {
            return false;
        }
        self.senders[self.sender_count] = sender;
        self.sender_count += 1;
        true
    }

    fn pop_sender(&mut self) -> Option<PendingSender> {
        if self.sender_count == 0 {
            return None;
        }
        let sender = self.senders[0];
        let mut index = 1usize;
        while index < self.sender_count {
            self.senders[index - 1] = self.senders[index];
            index += 1;
        }
        self.sender_count -= 1;
        Some(sender)
    }
}

#[derive(Clone, Copy)]
struct Notification {
    owner: u32,
    generation: u32,
    pending: u64,
    waiters: WaitQueue,
}

impl Notification {
    const EMPTY: Self = Self {
        owner: 0,
        generation: 1,
        pending: 0,
        waiters: WaitQueue::EMPTY,
    };
}

#[derive(Clone, Copy)]
struct Reply {
    owner: u32,
    generation: u32,
    caller: u8,
    server: u8,
}

impl Reply {
    const EMPTY: Self = Self {
        owner: 0,
        generation: 1,
        caller: 0xff,
        server: 0xff,
    };
}

static mut ENDPOINTS: [Endpoint; MAX_ENDPOINTS] = [Endpoint::EMPTY; MAX_ENDPOINTS];
static mut NOTIFICATIONS: [Notification; MAX_NOTIFICATIONS] =
    [Notification::EMPTY; MAX_NOTIFICATIONS];
static mut REPLIES: [Reply; MAX_REPLIES] = [Reply::EMPTY; MAX_REPLIES];

/// 任务启动或退出后重置 IPC 对象表，同时保留每个 slot 的 generation，
/// 让旧 Handle 即使在 slot 复用后也不会重新获得访问权限。
pub fn init() {
    unsafe {
        let mut slot = 0usize;
        while slot < MAX_ENDPOINTS {
            let generation = ENDPOINTS[slot].generation;
            ENDPOINTS[slot] = Endpoint {
                generation,
                ..Endpoint::EMPTY
            };
            slot += 1;
        }
        slot = 0;
        while slot < MAX_NOTIFICATIONS {
            let generation = NOTIFICATIONS[slot].generation;
            NOTIFICATIONS[slot] = Notification {
                generation,
                ..Notification::EMPTY
            };
            slot += 1;
        }
        slot = 0;
        while slot < MAX_REPLIES {
            let generation = REPLIES[slot].generation;
            REPLIES[slot] = Reply {
                generation,
                ..Reply::EMPTY
            };
            slot += 1;
        }
    }
}

fn next_generation(value: u32) -> u32 {
    if value >= MAX_GENERATION {
        1
    } else {
        value + 1
    }
}

fn make_handle(slot: usize, generation: u32) -> u64 {
    ((generation as u64) << 32) | (slot as u64 + 1)
}

fn decode_handle(handle: u64, capacity: usize) -> Option<(usize, u32)> {
    let raw_slot = handle as u32;
    let generation = (handle >> 32) as u32;
    if raw_slot == 0 || raw_slot as usize > capacity || generation == 0 {
        return None;
    }
    Some((raw_slot as usize - 1, generation))
}

fn endpoint_slot(handle: u64) -> Result<usize, u64> {
    let Some((slot, generation)) = decode_handle(handle, MAX_ENDPOINTS) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    unsafe {
        let endpoint = ENDPOINTS[slot];
        if endpoint.owner != crate::task::current_owner() || endpoint.generation != generation {
            return Err(exo_abi::SYS_ERR_NOT_FOUND);
        }
    }
    Ok(slot)
}

fn notification_slot(handle: u64) -> Result<usize, u64> {
    let Some((slot, generation)) = decode_handle(handle, MAX_NOTIFICATIONS) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    unsafe {
        let notification = NOTIFICATIONS[slot];
        if notification.owner != crate::task::current_owner()
            || notification.generation != generation
        {
            return Err(exo_abi::SYS_ERR_NOT_FOUND);
        }
    }
    Ok(slot)
}

pub fn notification_valid(handle: u64) -> bool {
    notification_slot(handle).is_ok()
}

pub fn endpoint_create() -> u64 {
    unsafe {
        let Some(slot) = (0..MAX_ENDPOINTS).find(|&slot| ENDPOINTS[slot].owner == 0) else {
            return exo_abi::SYS_ERR_NO_SLOT;
        };
        let generation = ENDPOINTS[slot].generation;
        ENDPOINTS[slot] = Endpoint {
            owner: crate::task::current_owner(),
            generation,
            ..Endpoint::EMPTY
        };
        make_handle(slot, generation)
    }
}

fn allocate_reply(caller: usize, server: usize) -> Option<u64> {
    unsafe {
        let slot = (0..MAX_REPLIES).find(|&slot| REPLIES[slot].owner == 0)?;
        let generation = REPLIES[slot].generation;
        REPLIES[slot] = Reply {
            owner: crate::task::current_owner(),
            generation,
            caller: caller as u8,
            server: server as u8,
        };
        Some(make_handle(slot, generation))
    }
}

fn deliver(sender: PendingSender, receiver: usize) -> Result<(), u64> {
    let mut message = sender.message;
    if sender.is_call {
        let Some(reply) = allocate_reply(sender.slot as usize, receiver) else {
            return Err(exo_abi::SYS_ERR_NO_SLOT);
        };
        message.reply = exo_abi::ReplyHandle(reply);
    } else {
        message.reply = exo_abi::ReplyHandle(0);
        thread::wake(sender.slot as usize, 0);
    }
    thread::write_message(receiver, message);
    thread::wake(receiver, 0);
    Ok(())
}

fn send_common(handle: u64, is_call: bool, frame: &TrapFrame) -> Result<*mut TrapFrame, u64> {
    let endpoint_slot = endpoint_slot(handle)?;
    let sender = PendingSender {
        slot: thread::current_slot() as u8,
        is_call,
        message: thread::current_message(),
    };
    unsafe {
        if let Some(receiver) = ENDPOINTS[endpoint_slot].receivers.pop() {
            if let Err(error) = deliver(sender, receiver) {
                // deliver 可能因为 Reply 表已满失败；此时不能丢掉已经
                // 从接收队列取出的 receiver，否则后续消息会永久失配。
                let _ = ENDPOINTS[endpoint_slot].receivers.push(receiver);
                return Err(error);
            }
            if is_call {
                Ok(thread::block_current(frame))
            } else {
                let mut return_frame = *frame;
                return_frame.x[0] = 0;
                thread::save_current(&return_frame);
                Ok(thread::yield_current(&return_frame))
            }
        } else {
            if !ENDPOINTS[endpoint_slot].push_sender(sender) {
                return Err(exo_abi::SYS_ERR_NO_SLOT);
            }
            Ok(thread::block_current(frame))
        }
    }
}

pub fn endpoint_send(handle: u64, frame: &TrapFrame) -> Result<*mut TrapFrame, u64> {
    send_common(handle, false, frame)
}

pub fn endpoint_call(handle: u64, frame: &TrapFrame) -> Result<*mut TrapFrame, u64> {
    send_common(handle, true, frame)
}

pub fn endpoint_recv(handle: u64, frame: &TrapFrame) -> Result<*mut TrapFrame, u64> {
    let endpoint_slot = endpoint_slot(handle)?;
    unsafe {
        if let Some(sender) = ENDPOINTS[endpoint_slot].pop_sender() {
            let receiver = thread::current_slot();
            if let Err(error) = deliver(sender, receiver) {
                let _ = ENDPOINTS[endpoint_slot].push_sender(sender);
                return Err(error);
            }
            let mut return_frame = *frame;
            return_frame.x[0] = 0;
            thread::save_current(&return_frame);
            Ok(thread::yield_current(&return_frame))
        } else {
            if !ENDPOINTS[endpoint_slot]
                .receivers
                .push(thread::current_slot())
            {
                return Err(exo_abi::SYS_ERR_BUSY);
            }
            Ok(thread::block_current(frame))
        }
    }
}

pub fn endpoint_reply(handle: u64, frame: &TrapFrame) -> Result<*mut TrapFrame, u64> {
    reply_inner(handle)?;
    let mut return_frame = *frame;
    return_frame.x[0] = 0;
    thread::save_current(&return_frame);
    Ok(thread::yield_current(&return_frame))
}

fn reply_inner(handle: u64) -> Result<(), u64> {
    let Some((slot, generation)) = decode_handle(handle, MAX_REPLIES) else {
        return Err(exo_abi::SYS_ERR_INVALID);
    };
    unsafe {
        let reply = REPLIES[slot];
        if reply.owner != crate::task::current_owner()
            || reply.generation != generation
            || reply.server as usize != thread::current_slot()
        {
            return Err(exo_abi::SYS_ERR_NOT_FOUND);
        }
        thread::write_message(reply.caller as usize, thread::current_message());
        thread::wake(reply.caller as usize, 0);
        REPLIES[slot] = Reply {
            generation: next_generation(reply.generation),
            ..Reply::EMPTY
        };
    }
    Ok(())
}

pub fn endpoint_reply_recv(
    endpoint: u64,
    reply: u64,
    frame: &TrapFrame,
) -> Result<*mut TrapFrame, u64> {
    reply_inner(reply)?;
    let endpoint_slot = endpoint_slot(endpoint)?;
    unsafe {
        if let Some(sender) = ENDPOINTS[endpoint_slot].pop_sender() {
            let receiver = thread::current_slot();
            if let Err(error) = deliver(sender, receiver) {
                let _ = ENDPOINTS[endpoint_slot].push_sender(sender);
                return Err(error);
            }
            let mut return_frame = *frame;
            return_frame.x[0] = 0;
            thread::save_current(&return_frame);
            Ok(thread::yield_current(&return_frame))
        } else {
            if !ENDPOINTS[endpoint_slot]
                .receivers
                .push(thread::current_slot())
            {
                return Err(exo_abi::SYS_ERR_BUSY);
            }
            Ok(thread::block_current(frame))
        }
    }
}

pub fn notification_create() -> u64 {
    unsafe {
        let Some(slot) = (0..MAX_NOTIFICATIONS).find(|&slot| NOTIFICATIONS[slot].owner == 0) else {
            return exo_abi::SYS_ERR_NO_SLOT;
        };
        let generation = NOTIFICATIONS[slot].generation;
        NOTIFICATIONS[slot] = Notification {
            owner: crate::task::current_owner(),
            generation,
            ..Notification::EMPTY
        };
        make_handle(slot, generation)
    }
}

pub fn notification_signal(handle: u64, badge: u64) -> u64 {
    if badge == 0 {
        return exo_abi::SYS_ERR_INVALID;
    }
    let Ok(slot) = notification_slot(handle) else {
        return exo_abi::SYS_ERR_NOT_FOUND;
    };
    unsafe {
        NOTIFICATIONS[slot].pending |= badge;
        if let Some(waiter) = NOTIFICATIONS[slot].waiters.pop() {
            let pending = NOTIFICATIONS[slot].pending;
            NOTIFICATIONS[slot].pending = 0;
            thread::wake(waiter, pending);
        }
    }
    0
}

pub fn notification_wait(handle: u64, frame: &mut TrapFrame) -> Result<*mut TrapFrame, u64> {
    let slot = notification_slot(handle)?;
    unsafe {
        if NOTIFICATIONS[slot].pending != 0 {
            let pending = NOTIFICATIONS[slot].pending;
            NOTIFICATIONS[slot].pending = 0;
            // 没有发生调度时必须返回本次异常的真实帧；线程表里的
            // context 可能仍是上一次切换前的旧 PC。
            frame.x[0] = pending;
            return Ok(frame as *mut TrapFrame);
        }
        if !NOTIFICATIONS[slot].waiters.push(thread::current_slot()) {
            return Err(exo_abi::SYS_ERR_BUSY);
        }
    }
    Ok(thread::block_current(frame))
}

pub fn notification_poll(handle: u64) -> u64 {
    let Ok(slot) = notification_slot(handle) else {
        return exo_abi::SYS_ERR_NOT_FOUND;
    };
    unsafe {
        if NOTIFICATIONS[slot].pending == 0 {
            exo_abi::SYS_ERR_WOULD_BLOCK
        } else {
            let pending = NOTIFICATIONS[slot].pending;
            NOTIFICATIONS[slot].pending = 0;
            pending
        }
    }
}

pub fn notification_destroy(handle: u64) -> u64 {
    let Ok(slot) = notification_slot(handle) else {
        return exo_abi::SYS_ERR_NOT_FOUND;
    };
    if crate::task::notification_is_bound(handle) {
        return exo_abi::SYS_ERR_BUSY;
    }
    unsafe {
        let notification = NOTIFICATIONS[slot];
        if notification.waiters.count != 0 {
            return exo_abi::SYS_ERR_BUSY;
        }
        NOTIFICATIONS[slot] = Notification {
            generation: next_generation(notification.generation),
            ..Notification::EMPTY
        };
    }
    0
}

pub fn cancel_thread(slot: usize) {
    unsafe {
        let mut object_slot = 0usize;
        while object_slot < MAX_ENDPOINTS {
            let endpoint = &mut ENDPOINTS[object_slot];
            endpoint.receivers.remove(slot);
            let mut index = 0usize;
            while index < endpoint.sender_count {
                if endpoint.senders[index].slot as usize == slot {
                    endpoint.sender_count -= 1;
                    endpoint.senders[index] = endpoint.senders[endpoint.sender_count];
                } else {
                    index += 1;
                }
            }
            object_slot += 1;
        }
        object_slot = 0;
        while object_slot < MAX_NOTIFICATIONS {
            NOTIFICATIONS[object_slot].waiters.remove(slot);
            object_slot += 1;
        }
        object_slot = 0;
        while object_slot < MAX_REPLIES {
            let reply = REPLIES[object_slot];
            if reply.owner != 0 && reply.caller as usize == slot {
                REPLIES[object_slot] = Reply {
                    generation: next_generation(reply.generation),
                    ..Reply::EMPTY
                };
            }
            object_slot += 1;
        }
    }
}

pub fn cleanup_owner(owner: u32) {
    unsafe {
        let mut slot = 0usize;
        while slot < MAX_ENDPOINTS {
            let endpoint = ENDPOINTS[slot];
            if endpoint.owner == owner {
                ENDPOINTS[slot] = Endpoint {
                    generation: next_generation(endpoint.generation),
                    ..Endpoint::EMPTY
                };
            }
            slot += 1;
        }
        slot = 0;
        while slot < MAX_NOTIFICATIONS {
            let notification = NOTIFICATIONS[slot];
            if notification.owner == owner {
                NOTIFICATIONS[slot] = Notification {
                    generation: next_generation(notification.generation),
                    ..Notification::EMPTY
                };
            }
            slot += 1;
        }
        slot = 0;
        while slot < MAX_REPLIES {
            let reply = REPLIES[slot];
            if reply.owner == owner {
                REPLIES[slot] = Reply {
                    generation: next_generation(reply.generation),
                    ..Reply::EMPTY
                };
            }
            slot += 1;
        }
    }
}
