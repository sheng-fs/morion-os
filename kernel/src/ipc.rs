//! IPC — 域间同步消息传递 (微内核核心原语 · 第 4 小步)
//!
//! 提供 `send` (非阻塞) 与 `receive` (阻塞) 两个原语:
//!   - 每个域拥有一个内核态消息邮箱 (固定容量)。
//!   - `send` 将消息放入目标域邮箱, 并唤醒一个等待该域消息的任务。
//!   - `receive` 从当前域邮箱取消息; 邮箱为空时阻塞当前任务, 由 `send` 唤醒。
//!
//! 第 5 小步: `send` 增加能力强制 — 调用者须持有向目标域发送的能力。
//! 消息为固定大小结构, 避免内核堆分配 (与架构文档一致)。

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use spin::Mutex;

use crate::cap::Capability;

/// 消息 payload 固定大小 (字节)。
pub const PAYLOAD_LEN: usize = 32;
/// 每域邮箱容量 (超出则发送失败)。
const MAILBOX_CAP: usize = 16;

/// IPC 消息。
/// `#[repr(C)]` 保证与用户态同名字段结构体布局一致 (可由 `SYS_RECV` 写回)。
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Message {
    pub from: u64,
    pub to: u64,
    pub tag: u64,
    pub payload: [u8; PAYLOAD_LEN],
}

/// 全局邮箱表: 每个域一个消息队列。
static MAILBOXES: Mutex<Vec<VecDeque<Message>>> = Mutex::new(Vec::new());

/// 初始化 IPC (创建 `domain_count` 个邮箱)。
pub fn init(domain_count: usize) {
    let mut boxes = MAILBOXES.lock();
    boxes.clear();
    for _ in 0..domain_count {
        boxes.push(VecDeque::new());
    }
}

/// 发送消息到目标域 (非阻塞)。
/// 返回是否成功 (无能力或邮箱满则失败)。
pub fn send(to: u64, tag: u64, payload: &[u8]) -> bool {
    x86_64::instructions::interrupts::disable();

    let from = crate::scheduler::current_domain();

    // 能力强制: 调用者必须持有向目标域发送消息的能力, 否则拒绝。
    if !crate::cap::has(from, Capability::SendTo(to)) {
        x86_64::instructions::interrupts::enable();
        return false;
    }

    let mut msg = Message {
        from,
        to,
        tag,
        payload: [0; PAYLOAD_LEN],
    };
    let n = payload.len().min(PAYLOAD_LEN);
    msg.payload[..n].copy_from_slice(&payload[..n]);

    let ok = {
        let mut boxes = MAILBOXES.lock();
        let mbox = &mut boxes[to as usize];
        if mbox.len() >= MAILBOX_CAP {
            false
        } else {
            mbox.push_back(msg);
            true
        }
    };

    if ok {
        crate::scheduler::wake_one(to);
    }
    x86_64::instructions::interrupts::enable();
    ok
}

/// 内核内部投递: 将一条消息送入 `to` 域邮箱并唤醒 (不检查能力)。
///
/// 用于缺页异常等内核发起的消息转发 (区别于 `send`, 后者做能力强制且
/// 以当前域为发送者)。调用者须保证中断已关闭 (IF=0), 本函数不改变中断位。
pub fn deliver(from: u64, to: u64, tag: u64, payload: &[u8]) -> bool {
    let mut msg = Message {
        from,
        to,
        tag,
        payload: [0; PAYLOAD_LEN],
    };
    let n = payload.len().min(PAYLOAD_LEN);
    msg.payload[..n].copy_from_slice(&payload[..n]);

    let ok = {
        let mut boxes = MAILBOXES.lock();
        let mbox = &mut boxes[to as usize];
        if mbox.len() >= MAILBOX_CAP {
            false
        } else {
            mbox.push_back(msg);
            true
        }
    };

    if ok {
        crate::scheduler::wake_one(to);
    }
    ok
}

/// 从当前域邮箱接收消息 (阻塞)。
/// 邮箱为空时阻塞当前任务, 直到有消息到达。
pub fn receive() -> Message {
    loop {
        x86_64::instructions::interrupts::disable();
        let me = crate::scheduler::current_domain();

        let got = {
            let mut boxes = MAILBOXES.lock();
            boxes[me as usize].pop_front()
        };

        match got {
            Some(msg) => {
                // 记录回复目标, 供后续 `reply` 路由回调用者。
                crate::scheduler::set_current_reply_target(msg.from);
                x86_64::instructions::interrupts::enable();
                return msg;
            }
            None => {
                // 邮箱为空: 阻塞当前任务, 等待 `send` 唤醒。
                // block_current 会切换到其他任务; 被唤醒后回到 loop 顶部。
                crate::scheduler::block_current(me);
            }
        }
    }
}

/// 同步调用: 发送请求到 `to` 并阻塞等待回复, 返回回复消息。
///
/// 需 `Capability::SendTo(to)`。失败 (无能力) 时返回 `tag == u64::MAX` 的空消息。
pub fn call(to: u64, tag: u64, payload: &[u8]) -> Message {
    x86_64::instructions::interrupts::disable();
    let me = crate::scheduler::current_domain();

    if !crate::cap::has(me, Capability::SendTo(to)) {
        x86_64::instructions::interrupts::enable();
        return Message {
            from: 0,
            to: 0,
            tag: u64::MAX,
            payload: [0; PAYLOAD_LEN],
        };
    }

    // 构造并投递请求。
    let mut msg = Message {
        from: me,
        to,
        tag,
        payload: [0; PAYLOAD_LEN],
    };
    let n = payload.len().min(PAYLOAD_LEN);
    msg.payload[..n].copy_from_slice(&payload[..n]);
    {
        let mut boxes = MAILBOXES.lock();
        boxes[to as usize].push_back(msg);
    }
    crate::scheduler::wake_one(to);

    // 阻塞等待回复; 被 `reply` 唤醒后回到此处, 从自己邮箱取回复。
    crate::scheduler::block_current(me);

    let reply = {
        let mut boxes = MAILBOXES.lock();
        boxes[me as usize].pop_front()
    };
    x86_64::instructions::interrupts::enable();

    reply.unwrap_or(Message {
        from: 0,
        to: 0,
        tag: u64::MAX,
        payload: [0; PAYLOAD_LEN],
    })
}

/// 回复当前任务最近一次 `receive` 到的调用者 (回复目标由 `receive` 记录)。
///
/// 无有效回复目标 (尚未 `receive`) 时返回 `false`。
pub fn reply(tag: u64, payload: &[u8]) -> bool {
    x86_64::instructions::interrupts::disable();
    let me = crate::scheduler::current_domain();
    let target = crate::scheduler::current_reply_target();

    if target == u64::MAX {
        x86_64::instructions::interrupts::enable();
        return false;
    }

    let mut msg = Message {
        from: me,
        to: target,
        tag,
        payload: [0; PAYLOAD_LEN],
    };
    let n = payload.len().min(PAYLOAD_LEN);
    msg.payload[..n].copy_from_slice(&payload[..n]);
    {
        let mut boxes = MAILBOXES.lock();
        boxes[target as usize].push_back(msg);
    }
    crate::scheduler::wake_one(target);
    x86_64::instructions::interrupts::enable();
    true
}
