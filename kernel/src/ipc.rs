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
///
/// 96 字节: 需容纳 VFS 请求里的绝对路径 (MFS/ext2 的长名可达 ~90 字节);
/// 结构化的请求 (如 32 字节 `BlockReq`/`ReadReq`) 仍只用其前若干字节。
pub const PAYLOAD_LEN: usize = 96;
/// 每域邮箱容量 (超出则发送失败)。
const MAILBOX_CAP: usize = 16;

/// `call` 等待回复时的超时轮询间隔 (ms)。
///
/// 只作「顺便检查目标域还活着没有」的节拍用: 正常回复远早于它; 目标域若在回复前死掉,
/// 客户端最多等这么久就能失败返回, 而不是永久挂起。
const CALL_POLL_MS: u64 = 200;

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

/// 运行时确保域 `id` 有一个**空**邮箱 (ELF 加载建新域时调用)。
///
/// 按域 id 索引, 故须在 `domain::create()` 之后调用; 域表槽位会被复用
/// (见 `domain::slot_for`), 所以这里同时起到"清掉上一个域遗留消息"的作用。
pub fn add_domain(id: u64) {
    set_mailbox(id, VecDeque::new());
}

/// 域销毁时调用: 丢掉该域邮箱里未读的消息。
///
/// 未读消息不会有人再取 (该域已无任务), 留着只会白占内存 —— 且槽位复用后
/// 新域会捡到旧消息。
pub fn remove_domain(id: u64) {
    set_mailbox(id, VecDeque::new());
}

/// 把 `id` 的邮箱整体替换成 `box_` —— 不够长就在表尾补齐空邮箱。
fn set_mailbox(id: u64, box_: VecDeque<Message>) {
    let mut boxes = MAILBOXES.lock();
    let idx = id as usize;
    while boxes.len() <= idx {
        boxes.push(VecDeque::new());
    }
    boxes[idx] = box_;
}

/// 构造一条消息。**`from` 只能由内核填** —— 用户态 syscall 接口里没有这个参数
/// (`SYS_SEND` 只有 `to/tag/payload`), 故 `msg.from` 无法被调用方伪造。
fn make_message(from: u64, to: u64, tag: u64, payload: &[u8]) -> Message {
    let mut msg = Message {
        from,
        to,
        tag,
        payload: [0; PAYLOAD_LEN],
    };
    let n = payload.len().min(PAYLOAD_LEN);
    msg.payload[..n].copy_from_slice(&payload[..n]);
    msg
}

/// 把 `msg` 放进 `to` 的邮箱; 返回是否投递成功。
///
/// `to` 越界 (域表没有这一行) 一律失败 —— 不能用 `boxes[to]` 直接索引, 否则
/// 用户可控的域号会变成内核 panic (DoS)。`respect_cap` 为 true 时遵守邮箱容量上限;
/// `call`/`reply` 走 false (回复路径不能因为队列满而丢弃)。
fn push_message(
    boxes: &mut [VecDeque<Message>],
    to: u64,
    msg: Message,
    respect_cap: bool,
) -> bool {
    let Some(mbox) = boxes.get_mut(to as usize) else {
        return false;
    };
    if respect_cap && mbox.len() >= MAILBOX_CAP {
        return false;
    }
    mbox.push_back(msg);
    true
}

/// 发送消息到目标域 (非阻塞)。
/// 返回是否成功 (目标域不存在 / 无能力 / 邮箱满则失败)。
pub fn send(to: u64, tag: u64, payload: &[u8]) -> bool {
    x86_64::instructions::interrupts::disable();

    let from = crate::scheduler::current_domain();

    // 目标域必须存在: 邮箱表按域 id 索引, 不校验会让用户可控的 `to` 越界 panic。
    // (中断已关, 不会与 `domain::destroy` 抢 `DOMAINS` 锁。)
    if !crate::domain::is_alive(to) {
        x86_64::instructions::interrupts::enable();
        return false;
    }

    // 能力强制: 调用者必须持有向目标域发送消息的能力, 否则拒绝。
    if !crate::cap::has(from, Capability::SendTo(to)) {
        x86_64::instructions::interrupts::enable();
        return false;
    }

    let msg = make_message(from, to, tag, payload);
    let ok = {
        let mut boxes = MAILBOXES.lock();
        push_message(&mut boxes, to, msg, true)
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
///
/// 这里**只做邮箱表边界检查**, 不查 `domain::is_alive`: 本函数可能在中断上下文
/// 被调用 (IRQ 通知), 而 `DOMAINS` 锁不是中断安全的 —— 在那个上下文再取它会把
/// 「持锁时来中断」变成自旋死锁。目标域号来自内核 (缺页域 / 中断注册者), 不是用户输入。
pub fn deliver(from: u64, to: u64, tag: u64, payload: &[u8]) -> bool {
    let msg = make_message(from, to, tag, payload);
    let ok = {
        let mut boxes = MAILBOXES.lock();
        push_message(&mut boxes, to, msg, true)
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
            boxes.get_mut(me as usize).and_then(|mbox| mbox.pop_front())
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

/// 从当前域邮箱接收消息 (**非阻塞**)。
///
/// 与 [`receive`] 的区别: 邮箱为空时**立即返回 `None`**, 不阻塞当前任务。用于驱动/网络
/// 服务这类"既要轮询硬件、又要接 IPC"的循环 (N6: `net_srv` 一边排空 RX、一边服务送帧请求)。
pub fn try_receive() -> Option<Message> {
    x86_64::instructions::interrupts::disable();
    let me = crate::scheduler::current_domain();

    let got = {
        let mut boxes = MAILBOXES.lock();
        boxes.get_mut(me as usize).and_then(|mbox| mbox.pop_front())
    };

    match got {
        Some(msg) => {
            // 与 `receive` 一致: 记录回复目标, 供后续 `reply` 路由回调用者。
            crate::scheduler::set_current_reply_target(msg.from);
            x86_64::instructions::interrupts::enable();
            Some(msg)
        }
        None => {
            x86_64::instructions::interrupts::enable();
            None
        }
    }
}

/// 同步调用: 发送请求到 `to` 并阻塞等待回复, 返回回复消息。
/// 需 `Capability::SendTo(to)`。失败时返回 `tag == u64::MAX` 的空消息 —— 三种情形:
/// 无能力、目标域**已无存活任务** (服务崩了 / 正被监督者重启), 或等待期间目标域死掉。
///
/// ⚠️ **为什么不等无限**: 调用方阻塞在**自己的域**键上 (发往本域的任何消息都会唤醒它),
/// 服务若在回复前退出, 就再没人回这条请求 —— 无限等下去客户端**永久挂死** (这正是
/// 「`gfx_srv` 一崩, shell 的 ipc 卡死」的根因)。故改为**带超时轮询**: 每 [`CALL_POLL_MS`]
/// 醒一次, 顺便看目标域还活着没有; 不活就失败返回, 让客户端能降级 / 重试。
///
/// 正常调用不受影响: 回复通常在 1~2 个时钟 tick 内到达 (500 Hz → 2~4 ms), 早于超时。
pub fn call(to: u64, tag: u64, payload: &[u8]) -> Message {
    x86_64::instructions::interrupts::disable();
    let me = crate::scheduler::current_domain();

    if !crate::cap::has(me, Capability::SendTo(to)) {
        x86_64::instructions::interrupts::enable();
        return failed();
    }
    // 目标域必须存在, 且已无存活任务时直接失败, 不必白等一轮 —— 请求发过去也没人取。
    if !crate::domain::is_alive(to) || crate::scheduler::live_tasks(to) == 0 {
        x86_64::instructions::interrupts::enable();
        return failed();
    }

    // 构造并投递请求 (回复路径不设容量上限: 请求队列满时宁可排队也不能丢)。
    let msg = make_message(me, to, tag, payload);
    let queued = {
        let mut boxes = MAILBOXES.lock();
        push_message(&mut boxes, to, msg, false)
    };
    if !queued {
        x86_64::instructions::interrupts::enable();
        return failed();
    }
    crate::scheduler::wake_one(to);

    // 阻塞等待回复。本域可能既是 client (向 `to` 发请求) 又是 server (接收
    // 其它域的请求), 发往本域的无关消息也会经 `wake_one(me)` 提前唤醒当前任务;
    // 故须循环阻塞, 直到真正收到来自 `to` 的回复为止。被无关消息唤醒时, 该消息
    // 仍留在本域邮箱, 交由后续 `receive` 处理。
    loop {
        crate::scheduler::block_current_timeout_ms(me, CALL_POLL_MS);

        let reply = {
            let mut boxes = MAILBOXES.lock();
            boxes.get_mut(me as usize).and_then(|mbox| {
                let idx = mbox.iter().position(|m| m.from == to);
                idx.and_then(|i| mbox.remove(i))
            })
        };

        if let Some(r) = reply {
            x86_64::instructions::interrupts::enable();
            return r;
        }
        // 没等到回复: 目标域若已无存活任务, 就失败返回 (否则可能是被无关消息唤醒, 继续等)。
        if crate::scheduler::live_tasks(to) == 0 {
            x86_64::instructions::interrupts::enable();
            return failed();
        }
    }
}

/// 构造一条「调用失败」的回复 (无能力 / 目标域已死)。
fn failed() -> Message {
    Message {
        from: 0,
        to: 0,
        tag: u64::MAX,
        payload: [0; PAYLOAD_LEN],
    }
}

/// 回复当前任务最近一次 `receive` 到的调用者 (回复目标由 `receive` 记录)。
///
/// 无有效回复目标 (尚未 `receive`) 时返回 `false`。
pub fn reply(tag: u64, payload: &[u8]) -> bool {
    x86_64::instructions::interrupts::disable();
    let me = crate::scheduler::current_domain();
    let target = crate::scheduler::current_reply_target();

    // 回复目标来自 `receive` 记录的内核侧 `msg.from`; 为 `u64::MAX` 或该域已不存在
    // (调用者在等待期间退出) 时不投递, 也绝不越界索引邮箱表。
    if target == u64::MAX || !crate::domain::is_alive(target) {
        x86_64::instructions::interrupts::enable();
        return false;
    }

    let msg = make_message(me, target, tag, payload);
    let ok = {
        let mut boxes = MAILBOXES.lock();
        push_message(&mut boxes, target, msg, false)
    };
    if ok {
        crate::scheduler::wake_one(target);
    }
    x86_64::instructions::interrupts::enable();
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 域销毁要丢掉邮箱里未读的消息, 且**槽位复用**后新域看不到旧消息。
    #[test]
    fn destroy_and_reuse_domain_drops_mailbox() {
        init(3);
        // 直接往域 1 邮箱塞一条 —— 不经 `deliver`, 那条路会唤醒调度器, 而单测里没有调度器。
        MAILBOXES.lock()[1].push_back(Message {
            from: 0,
            to: 1,
            tag: 0xAA,
            payload: [0; PAYLOAD_LEN],
        });
        assert_eq!(MAILBOXES.lock()[1].len(), 1);

        remove_domain(1);
        assert!(MAILBOXES.lock()[1].is_empty());

        // 槽位复用: 同一个 id 再建域, 邮箱干净, 且**不新增行** (表按域 id 定长)。
        add_domain(1);
        assert!(MAILBOXES.lock()[1].is_empty());
        assert_eq!(MAILBOXES.lock().len(), 3);
    }

    // ---- 以下只测纯函数 (局部邮箱表), 不碰全局 `MAILBOXES`, 避免并行用例互相清表。 ----

    fn local_boxes(n: usize) -> Vec<VecDeque<Message>> {
        (0..n).map(|_| VecDeque::new()).collect()
    }

    /// `msg.from` 由内核填写 (syscall 没有这个入参), payload 超长时按 `PAYLOAD_LEN` 截断。
    #[test]
    fn make_message_sets_from_and_truncates_payload() {
        let long = [0xEEu8; PAYLOAD_LEN + 32];
        let msg = make_message(7, 3, 0xAB, &long);
        assert_eq!(msg.from, 7);
        assert_eq!(msg.to, 3);
        assert_eq!(msg.tag, 0xAB);
        assert!(msg.payload.iter().all(|&b| b == 0xEE));

        let short = make_message(1, 2, 9, &[1, 2, 3]);
        assert_eq!(&short.payload[..3], &[1, 2, 3]);
        assert!(short.payload[3..].iter().all(|&b| b == 0));
    }

    /// 目标域越界必须**失败而不是 panic**, 且不扩表 (用户可控域号的 DoS 防线)。
    #[test]
    fn push_rejects_unknown_target() {
        let mut boxes = local_boxes(2);
        let msg = make_message(0, 5, 1, &[]);
        assert!(!push_message(&mut boxes, 5, msg, true));
        assert_eq!(boxes.len(), 2);
    }

    /// `send` 路径受邮箱容量约束; `call`/`reply` 路径 (respect_cap=false) 不丢弃。
    #[test]
    fn push_respects_capacity_for_send_only() {
        let mut boxes = local_boxes(1);
        for i in 0..MAILBOX_CAP {
            assert!(push_message(&mut boxes, 0, make_message(1, 0, i as u64, &[]), true));
        }
        let full = make_message(1, 0, 0xFF, &[]);
        assert!(!push_message(&mut boxes, 0, full, true), "满队列应拒绝 send");
        assert!(
            push_message(&mut boxes, 0, full, false),
            "回复路径不受容量上限约束"
        );
        assert_eq!(boxes[0].len(), MAILBOX_CAP + 1);
    }
}
