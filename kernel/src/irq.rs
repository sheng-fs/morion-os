//! IRQ 转发 — 用户态设备驱动框架 (Stage 16)
//!
//! 微内核「中断即 IPC」模型:
//!   - 用户态驱动域通过 `SYS_REGISTER_IRQ` 注册接收某个 IRQ (需 `Capability::Irq(irq)`)。
//!   - 硬件 IRQ 处理器读取设备数据后, 经 `dispatch` 把数据作为 IPC 消息 tag 转发给
//!     注册的驱动域, 再发送 EOI。
//!   - 驱动域循环 `SYS_RECV` 接收中断消息并处理设备数据。
//!
//! 当前仅支持最多 16 个 IRQ (PIC master 8 + slave 8)。

use spin::Mutex;

/// 每个 IRQ 对应的驱动域 id (None 表示未注册, 中断被忽略)。
static HANDLERS: Mutex<[Option<u64>; 16]> = Mutex::new([None; 16]);

/// 注册 `irq` 由 `domain` 驱动域接收。
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
