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
    /// 注册接收指定 IRQ 的能力 (用户态设备驱动)。
    Irq(u8),
    /// 把指定物理基址 (页对齐) 的 MMIO 区域映射进本域的能力。
    Mmio(u64),
    /// 访问指定 I/O 端口 (x86 IN/OUT) 的能力。
    ///
    /// 与 `Mmio` (MMIO 内存映射寄存器) 区分: I/O 端口是独立地址空间,
    /// 某些设备 (如 PIT / PIC / 传统 IDE PIO) 仅通过 I/O 端口暴露。
    IoPort(u16),
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

#[cfg(test)]
mod tests {
    use super::Capability;

    /// 编码安全契约: IoPort(port) 能力必须能精确区分不同端口, 且与
    /// Mmio/Irq 等其它能力类型不发生意外的相等匹配。
    #[test]
    fn io_port_capability_semantics() {
        let a = Capability::IoPort(0x1F0);
        let b = Capability::IoPort(0x1F0);
        let c = Capability::IoPort(0x1F1);
        let d = Capability::Mmio(0xF000_0000);
        let e = Capability::Irq(0);

        // 同端口 → 相等
        assert_eq!(a, b);
        // 不同端口 → 不等
        assert_ne!(a, c);
        // 与其它能力类型 → 不等
        assert_ne!(a, d);
        assert_ne!(a, e);

        // Clone / Copy 必须保留值
        let a2 = a;
        assert_eq!(a, a2);
    }

    /// 同类型不同 payload 的 Capability 不能互相替代 —
    /// IoPort(0x1F0) != IoPort(0x1F1) 意味着攻击者无法通过持有相邻端口
    /// 的能力来"扩展"权限范围。
    #[test]
    fn capability_payload_is_significant() {
        assert_ne!(Capability::SendTo(1), Capability::SendTo(2));
        assert_ne!(Capability::MapInto(1), Capability::MapInto(2));
        assert_ne!(Capability::Irq(1), Capability::Irq(2));
        assert_ne!(Capability::Mmio(0x1000), Capability::Mmio(0x2000));
        assert_ne!(Capability::IoPort(0x1F0), Capability::IoPort(0x1F7));
    }
}
