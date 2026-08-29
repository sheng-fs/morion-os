//! 能力系统 (微内核核心原语 · 第 5 小步)
//!
//! 能力 (Capability) 是访问内核对象的唯一凭证。每个域拥有一个能力槽表,
//! 内核在 IPC 等路径上强制校验调用者是否持有对应能力, 实现"无能力即不可访问"。
//!
//! 最小权限: 新域默认不持有任何能力, 由授权方通过 `grant` 显式授予。

use alloc::vec::Vec;
use spin::Mutex;

/// 能力类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Capability {
    /// 向指定域发送 IPC 消息的能力。
    SendTo(u64),
    /// 把内存页映射进指定域的能力。
    MapInto(u64),
}

/// 每域能力槽数量。
const CAP_SLOTS: usize = 16;

/// 全局能力表: 每个域一个能力槽数组。
static CAP_TABLE: Mutex<Vec<[Option<Capability>; CAP_SLOTS]>> = Mutex::new(Vec::new());

/// 初始化能力系统 (创建 `domain_count` 个域的能力槽表)。
pub fn init(domain_count: usize) {
    let mut table = CAP_TABLE.lock();
    table.clear();
    for _ in 0..domain_count {
        table.push([None; CAP_SLOTS]);
    }
}

/// 检查某域是否持有指定能力 (须在关中断下调用)。
pub fn has(domain: u64, cap: Capability) -> bool {
    let table = CAP_TABLE.lock();
    table[domain as usize].iter().any(|c| *c == Some(cap))
}

/// 向某域授予能力 (占用一个空槽)。
pub fn grant(domain: u64, cap: Capability) -> bool {
    // 保存/恢复中断状态: boot 期 (IF=0) 调用时不能提前开启中断,
    // 否则 PIT 会在调度器尚未就绪时触发 schedule 导致 panic。
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let mut table = CAP_TABLE.lock();
    let mut ok = false;
    for slot in table[domain as usize].iter_mut() {
        if slot.is_none() {
            *slot = Some(cap);
            ok = true;
            break;
        }
    }
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
    ok
}

/// 撤销某域的指定能力。
pub fn revoke(domain: u64, cap: Capability) -> bool {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let mut table = CAP_TABLE.lock();
    let mut ok = false;
    for slot in table[domain as usize].iter_mut() {
        if *slot == Some(cap) {
            *slot = None;
            ok = true;
            break;
        }
    }
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
    ok
}
