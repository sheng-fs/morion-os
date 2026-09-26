//! IRQ 转发 — 用户态设备驱动框架 (Stage 16)
//!
//! 微内核「中断即 IPC」模型:
//!   - 用户态驱动域通过 `SYS_REGISTER_IRQ` 注册接收某个 IRQ (需 `Capability::Irq(irq)`)。
//!   - 硬件 IRQ 处理器读取设备数据后, 经 `dispatch` 把数据作为 IPC 消息 tag 转发给
//!     注册的驱动域, 再发送 EOI。
//!   - 驱动域循环 `SYS_RECV` 接收中断消息并处理设备数据。
//!
//! 当前支持两类中断源:
//!   - PIC 的 16 个 IRQ (IRQ0..IRQ15, 向量 32..47): 走 `register`/`dispatch`, 以 IPC
//!     把「数据」(如键盘 scancode) 送给驱动。
//!   - MSI/MSI-X 向量 (阶段 39/40, 见 `arch/apic.rs`): 走 `register_vector`/`set_pending`。
//!     这类中断**只带「完成了」这个事实**, 不带数据, 且与驱动收请求的邮箱是同一个 ——
//!     若也投成 IPC 就会和块请求混在一起 (拉取即消费, 还会改写内核记录的回复目标,
//!     导致后续 `reply` 投错域)。故改为置一个「待处理位」, 由驱动经 `SYS_IRQ_POLL` 主动
//!     取走、或经 `SYS_IRQ_WAIT` **阻塞**等 (中断处理器直接唤醒等待的域); 驱动的请求
//!     邮箱完全不受打扰。
//!   - 等待以**向量掩码**为单位 (`take_pending_any`/`set_any_mask`): 多队列设备常要
//!     「这几条队列哪个先完成都行」, 而「掩码里含该向量」正是唤醒的唯一条件。

use spin::Mutex;

/// 每个 IRQ (PIC) 对应的驱动域 id (None 表示未注册, 中断被忽略)。
static HANDLERS: Mutex<[Option<u64>; 16]> = Mutex::new([None; 16]);

/// 每个 MSI/MSI-X 向量对应的驱动域 (以向量号为下标)。
static VECTORS: Mutex<[Option<u64>; 256]> = Mutex::new([None; 256]);

/// 每个向量的「待处理」标志: 中断处理器只置位, 由驱动经 `SYS_IRQ_POLL` 取走。
static PENDING: Mutex<[bool; 256]> = Mutex::new([false; 256]);

/// 「等一组向量中任意一个」的掩码, 以**域 id** 为下标 (0 = 该域没在等)。
///
/// 位 `i` 对应向量 `idt::MSI_VECTOR_BASE + i` —— 与 syscall 的掩码编码一致。
/// 一个域同时只可能有一个任务在等 (每域一个任务), 故按域记一份就够。
const ANY_MAX_DOMAINS: usize = 64;
static ANY_MASK: Mutex<[u64; ANY_MAX_DOMAINS]> = Mutex::new([0; ANY_MAX_DOMAINS]);

/// `vector` 在掩码里的位 (位 `i` ↔ 向量 `idt::MSI_VECTOR_BASE + i`); 不在向量段内则 `None`。
fn vector_bit(vector: u8) -> Option<u64> {
    let base = crate::arch::idt::MSI_VECTOR_BASE;
    if vector >= base && vector < base + crate::arch::idt::MSI_VECTOR_COUNT {
        Some(1 << (vector - base))
    } else {
        None
    }
}

/// 注册 `irq` (PIC, 0..15) 由 `domain` 驱动域接收。
///
/// 调用者须先校验该域持有 `Capability::Irq(irq)` (由 `SYS_REGISTER_IRQ` 完成)。
pub fn register(irq: u8, domain: u64) {
    HANDLERS.lock()[irq as usize] = Some(domain);
}

/// 把 `irq` 的中断数据 (scancode 等) 转发给注册的驱动域。
///
/// 从 IRQ 处理器 (IF=0) 调用; 本函数非阻塞, 不改变中断使能位。
pub fn dispatch(irq: u8, data: u64) {
    if let Some(domain) = HANDLERS.lock()[irq as usize] {
        // from 记为 0 (内核); 中断消息无需回复, 仅作唤醒信号。
        crate::ipc::deliver(0, domain, data, &[]);
    }
}

/// 注册 MSI/MSI-X `vector` 由 `domain` 驱动域接收 (同样需 `Capability::Irq(vector)`)。
pub fn register_vector(vector: u8, domain: u64) {
    VECTORS.lock()[vector as usize] = Some(domain);
}

/// `domain` 是否正是 `vector` 的注册者 (`SYS_IRQ_WAIT` / `SYS_IRQ_POLL` 的额外校验)。
pub fn is_registered_by(vector: u8, domain: u64) -> bool {
    VECTORS.lock()[vector as usize] == Some(domain)
}

/// 取走掩码内**第一个**有待处理标志的向量 (取到即清标志); 没有则 `None`。
///
/// 返回向量号而不是 bool, 驱动因此知道「是哪条队列的中断」。
pub fn take_pending_any(mask: u64, domain: u64) -> Option<u8> {
    let base = crate::arch::idt::MSI_VECTOR_BASE;
    for i in 0..crate::arch::idt::MSI_VECTOR_COUNT {
        if mask & (1 << i) == 0 {
            continue;
        }
        let vector = base + i;
        if VECTORS.lock()[vector as usize] != Some(domain) {
            continue;
        }
        let mut pending = PENDING.lock();
        if pending[vector as usize] {
            pending[vector as usize] = false;
            return Some(vector);
        }
    }
    None
}

/// 登记 `domain` 正在等 `mask` 里的任意一个向量 (`SYS_IRQ_WAIT` 阻塞前调用)。
///
/// 域 id 超出表容量时返回 `false` (调用方放弃阻塞, 返回超时)。
pub fn set_any_mask(domain: u64, mask: u64) -> bool {
    if domain as usize >= ANY_MAX_DOMAINS {
        return false;
    }
    ANY_MASK.lock()[domain as usize] = mask;
    true
}

/// 撤销 `domain` 的等待掩码 (醒来后立刻清, 避免掩码残留导致后续误唤醒)。
pub fn clear_any_mask(domain: u64) {
    if (domain as usize) < ANY_MAX_DOMAINS {
        ANY_MASK.lock()[domain as usize] = 0;
    }
}

/// MSI/MSI-X 向量处理器调用: 置该向量的待处理位, 并唤醒等它的驱动域。
///
/// 从向量处理器 (IF=0) 调用。未注册的向量只做 EOI (无人可读, 标志也不置)。
/// 等待键(`irq_wait_token` 按域取)与 IPC 的域 id 不同, 故不会误唤醒等消息的任务。
pub fn set_pending(vector: u8) {
    // 两个锁各自取用后立即释放 (语句末即析构), 也**不**在持锁状态下进调度器:
    // 调度器的锁在关中断下被多处持有, 与 IRQ 自己的锁不能形成嵌套。
    if VECTORS.lock()[vector as usize].is_none() {
        return;
    }
    PENDING.lock()[vector as usize] = true;

    // 只有掩码里含这个向量的域才唤醒 (掩码命中才算它的中断到了)。
    let Some(bit) = vector_bit(vector) else {
        return;
    };
    let mut hit = [false; ANY_MAX_DOMAINS];
    {
        let any = ANY_MASK.lock();
        for (domain, mask) in any.iter().enumerate() {
            hit[domain] = mask & bit != 0;
        }
    }
    for (domain, waiting) in hit.iter().enumerate() {
        if *waiting {
            crate::scheduler::wake_one(crate::scheduler::irq_wait_token(domain as u64));
        }
    }
}
