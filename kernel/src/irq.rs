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
//!   - MSI/MSI-X 向量 (阶段 4, 见 `arch/apic.rs`): 走 `register_vector`/`set_pending`。
//!     这类中断**只带「完成了」这个事实**, 不带数据, 且与驱动收请求的邮箱是同一个 ——
//!     若也投成 IPC 就会和块请求混在一起 (拉取即消费, 还会改写内核记录的回复目标,
//!     导致后续 `reply` 投错域)。故改为置一个「待处理位」, 由驱动经 `SYS_IRQ_POLL`
//!     主动取走; 驱动的请求邮箱完全不受打扰。

use spin::Mutex;

/// 每个 IRQ (PIC) 对应的驱动域 id (None 表示未注册, 中断被忽略)。
static HANDLERS: Mutex<[Option<u64>; 16]> = Mutex::new([None; 16]);

/// 每个 MSI/MSI-X 向量对应的驱动域 (以向量号为下标)。
static VECTORS: Mutex<[Option<u64>; 256]> = Mutex::new([None; 256]);

/// 每个向量的「待处理」标志: 中断处理器只置位, 由驱动经 `SYS_IRQ_POLL` 取走。
static PENDING: Mutex<[bool; 256]> = Mutex::new([false; 256]);

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

/// MSI/MSI-X 向量处理器调用: 置该向量的待处理位。
///
/// 从向量处理器 (IF=0) 调用。未注册的向量只做 EOI (无人可读, 标志也不置)。
pub fn set_pending(vector: u8) {
    if VECTORS.lock()[vector as usize].is_some() {
        PENDING.lock()[vector as usize] = true;
    }
}

/// 取走 `vector` 的待处理标志 (`SYS_IRQ_POLL` 的落点); 取到返回 `true` 并清标志。
///
/// 要求 `domain` 正是该向量的注册者 —— 与能力校验一致, 别的域读不到别人的中断。
pub fn take_pending(vector: u8, domain: u64) -> bool {
    if VECTORS.lock()[vector as usize] != Some(domain) {
        return false;
    }
    let mut pending = PENDING.lock();
    if pending[vector as usize] {
        pending[vector as usize] = false;
        true
    } else {
        false
    }
}
