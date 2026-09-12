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
}

/// 每域能力槽数量。
const CAP_SLOTS: usize = 16;

/// 全局能力表: 每个域一个能力槽数组。
static CAP_TABLE: Mutex<Vec<[Option<Capability>; CAP_SLOTS]>> = Mutex::new(Vec::new());

/// 每域句柄槽数量 (「能力即句柄」: 每个打开的内核对象/服务句柄占一个槽)。
const HANDLE_SLOTS: usize = 32;

/// 句柄槽表: 每域一组, 槽内存放**不透明**对象标识 (`None` = 空槽)。
///
/// 微内核不知道「文件」是什么, 故这里只保管调用方给出的对象标识 (libvfs 传入
/// `(服务域 << 32) | 服务内 fd`), 由持有者凭句柄索引访问。句柄被撤销后, 凭它
/// 发起的操作一律失败 —— libvfs 在每次 I/O 前用 `SYS_CAP_LOOKUP` 校验句柄,
/// 这就是「能力即句柄」的执行点。
static HANDLE_TABLE: Mutex<Vec<[Option<u64>; HANDLE_SLOTS]>> = Mutex::new(Vec::new());

/// 初始化能力系统 (创建 `domain_count` 个域的能力槽表与句柄槽表)。
pub fn init(domain_count: usize) {
    let mut table = CAP_TABLE.lock();
    table.clear();
    for _ in 0..domain_count {
        table.push([None; CAP_SLOTS]);
    }
    drop(table);

    let mut handles = HANDLE_TABLE.lock();
    handles.clear();
    for _ in 0..domain_count {
        handles.push([None; HANDLE_SLOTS]);
    }
}

/// 为域 `domain` 的不透明对象 `obj` 签发句柄, 返回句柄索引 (0 起);
/// 槽位耗尽返回 `u64::MAX`。
pub fn handle_issue(domain: u64, obj: u64) -> u64 {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let mut table = HANDLE_TABLE.lock();
    let mut out = u64::MAX;
    if let Some(slots) = table.get_mut(domain as usize) {
        for (i, slot) in slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(obj);
                out = i as u64;
                break;
            }
        }
    }
    drop(table);
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
    out
}

/// 查句柄 `handle` 指向的对象标识; 句柄非法 (越界 / 已被撤销) 返回 `None`。
pub fn handle_lookup(domain: u64, handle: u64) -> Option<u64> {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let table = HANDLE_TABLE.lock();
    let out = table
        .get(domain as usize)
        .and_then(|slots| slots.get(handle as usize))
        .copied()
        .flatten();
    drop(table);
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
    out
}

/// 撤销句柄 `handle` (关闭打开对象时调用), 成功返回 true。
pub fn handle_drop(domain: u64, handle: u64) -> bool {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let mut table = HANDLE_TABLE.lock();
    let mut ok = false;
    if let Some(slots) = table.get_mut(domain as usize) {
        if let Some(slot) = slots.get_mut(handle as usize) {
            if slot.is_some() {
                *slot = None;
                ok = true;
            }
        }
    }
    drop(table);
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
    ok
}

/// 检查某域是否持有指定能力 (须在关中断下调用)。
pub fn has(domain: u64, cap: Capability) -> bool {
    let table = CAP_TABLE.lock();
    table[domain as usize].contains(&Some(cap))
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
