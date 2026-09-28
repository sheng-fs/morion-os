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
    /// 加载可执行文件并启动的能力 (`SYS_SPAWN_ELF`): 允许建新域 + 载入镜像 + 起任务。
    ///
    /// 无参数 —— 该能力本身就是"可以造进程"这张凭证。与其它能力一样默认不授予,
    /// 由信任方显式给（引导期给 shell / 自测域）。
    Spawn,
    /// 访问指定 I/O 端口的能力 (`SYS_PORT_IN8/16` / `SYS_PORT_OUT8/16`)。
    ///
    /// 微内核的 I/O 端口是硬件敏感资源 (IDE/NIC/键盘控制器等), 无能力则任意域
    /// 都能直接读写 —— 是纯安全漏洞。与其它能力一样默认不授予, 由引导器/授权方
    /// 按具体设备所需的端口显式给予。
    PortIo(u16),
}

/// 每域能力槽数量。
const CAP_SLOTS: usize = 16;

/// `SYS_CAP_SEND` 的 `kind` 编码 —— 能力是枚举, 而 syscall 参数只有整数,
/// 故用 `(kind, arg)` 两段表示 (与用户态 `syscall::CAP_KIND_*` 一致)。
pub const CAP_KIND_SEND_TO: u64 = 0;
pub const CAP_KIND_MAP_INTO: u64 = 1;
pub const CAP_KIND_IRQ: u64 = 2;
pub const CAP_KIND_MMIO: u64 = 3;
/// `Spawn` 无参数, 故 `arg` 被忽略（但保留两段式编码, 委派路径才不必特判）。
pub const CAP_KIND_SPAWN: u64 = 4;
/// I/O 端口访问: `arg` 为端口号 (u16)。
pub const CAP_KIND_PORT_IO: u64 = 5;

/// 把 `SYS_CAP_SEND` 的 `(kind, arg)` 解码成 `Capability`; 未知 `kind` 或
/// `arg` 越界返回 `None`。
///
/// 这里对 `arg` 的校验与各能力的使用点保持一致: `Irq` 是 u8 (见 `SYS_REGISTER_IRQ`),
/// `Mmio` 以**页对齐**物理基址标识 (见 `SYS_MAP_MMIO` 的 `bar & !0xFFF`) ——
/// 否则可以造出一个永远匹配不上的能力, 白占对方一个槽位。
pub fn decode(kind: u64, arg: u64) -> Option<Capability> {
    match kind {
        CAP_KIND_SEND_TO => Some(Capability::SendTo(arg)),
        CAP_KIND_MAP_INTO => Some(Capability::MapInto(arg)),
        CAP_KIND_IRQ if arg <= u8::MAX as u64 => Some(Capability::Irq(arg as u8)),
        CAP_KIND_MMIO if arg & 0xFFF == 0 => Some(Capability::Mmio(arg)),
        CAP_KIND_SPAWN => Some(Capability::Spawn),
        CAP_KIND_PORT_IO if arg <= u16::MAX as u64 => Some(Capability::PortIo(arg as u16)),
        _ => None,
    }
}

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

/// 运行时确保域 `id` 的槽位存在且**为空** (ELF 加载建新域时调用)。
///
/// 新域默认**零能力**; 且域表槽位会被复用 (见 `domain::slot_for`), 所以这里不只是
/// 补行, 还必须把复用到的旧行清零。按域 id 索引, 须在 `domain::create()` 之后调用。
pub fn add_domain(id: u64) {
    set_row(&mut CAP_TABLE.lock(), id, [None; CAP_SLOTS]);
    set_row(&mut HANDLE_TABLE.lock(), id, [None; HANDLE_SLOTS]);
}

/// 域销毁时调用: 丢掉该域遗留的能力槽与句柄槽 (行保留, 供槽位复用)。
pub fn remove_domain(id: u64) {
    set_row(&mut CAP_TABLE.lock(), id, [None; CAP_SLOTS]);
    set_row(&mut HANDLE_TABLE.lock(), id, [None; HANDLE_SLOTS]);
}

/// 把 `table` 的第 `id` 行设为 `row` —— 不够长就在表尾补齐 (中间的空缺一并补成 `row`)。
fn set_row<T: Copy>(table: &mut Vec<T>, id: u64, row: T) {
    let idx = id as usize;
    while table.len() <= idx {
        table.push(row);
    }
    table[idx] = row;
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

/// 把 `from` 域句柄槽 `handle` 里的对象标识**移入** `to` 域的空槽,
/// 返回 `to` 域里的新句柄索引。
///
/// 移动语义 (而非复制): 成功后 `from` 域的该槽立即失效 —— 「能力是唯一凭证」,
/// 同一份能力在同一时刻只属于一个域。这也是 fd 传递需要的语义 (交出 fd 后自己不再持有)。
///
/// 失败 (源槽越界 / 已空, 或目标域句柄槽满) 返回 `u64::MAX`, 且**不改变任何状态**:
/// 先取出对象再找空槽, 目标槽满时把对象放回原槽 (回滚), 避免「两边都没有」。
pub fn handle_move(from: u64, to: u64, handle: u64) -> u64 {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let mut table = HANDLE_TABLE.lock();

    let obj = table
        .get_mut(from as usize)
        .and_then(|slots| slots.get_mut(handle as usize))
        .and_then(|slot| slot.take());

    let mut out = u64::MAX;
    match obj {
        None => {}
        Some(o) => {
            if let Some(slots) = table.get_mut(to as usize) {
                for (i, slot) in slots.iter_mut().enumerate() {
                    if slot.is_none() {
                        *slot = Some(o);
                        out = i as u64;
                        break;
                    }
                }
            }
            if out == u64::MAX {
                // 目标槽满: 回滚, 对象仍留在原域。
                if let Some(slots) = table.get_mut(from as usize) {
                    if let Some(slot) = slots.get_mut(handle as usize) {
                        *slot = Some(o);
                    }
                }
            }
        }
    }

    drop(table);
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
    out
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

/// 把 `cap` 从 `from` 域**委派**给 `to` 域 (「能力随 IPC 传递」)。
///
/// 核心约束是**不允许放大**: `from` 必须自己持有 `cap`, 否则一律失败 ——
/// 没有的能力给不出去, 这是能力安全模型的根。检查与写入在**同一把锁**内完成,
/// 避免「检查后被抢先」。
///
/// `to` 已经持有该能力时直接返回成功且不占新槽 (幂等), 免得重复委派把 16 个槽位耗光。
pub fn delegate(from: u64, to: u64, cap: Capability) -> bool {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let mut table = CAP_TABLE.lock();

    let mut ok = false;
    let held = table
        .get(from as usize)
        .map(|slots| slots.contains(&Some(cap)))
        .unwrap_or(false);
    if held {
        if let Some(slots) = table.get_mut(to as usize) {
            if slots.contains(&Some(cap)) {
                ok = true;
            } else {
                for slot in slots.iter_mut() {
                    if slot.is_none() {
                        *slot = Some(cap);
                        ok = true;
                        break;
                    }
                }
            }
        }
    }

    drop(table);
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 域销毁必须把能力槽与句柄槽一起丢掉, 且**槽位复用**时不能捡到上一个域的遗留。
    ///
    /// 直接摆放表格现场而不经 `grant` / `handle_issue` / `handle_lookup`: 那几个会
    /// `cli`/`sti` 关开中断, 是特权指令, 在宿主单测里执行会 SIGSEGV (单测跑在用户态)。
    /// 这里校验的正是它们读的那两张表。
    #[test]
    fn destroy_and_reuse_domain_clears_caps_and_handles() {
        init(3);
        CAP_TABLE.lock()[1][0] = Some(Capability::SendTo(2));
        HANDLE_TABLE.lock()[1][0] = Some(0xDEAD_BEEF);
        assert!(has(1, Capability::SendTo(2)));

        // 销毁域 1: 能力与句柄都不再可用。
        remove_domain(1);
        assert!(!has(1, Capability::SendTo(2)));
        assert!(HANDLE_TABLE.lock()[1].iter().all(|slot| slot.is_none()));

        // 槽位复用 (同一个 id 再建域): 又摆一份旧值, `add_domain` 必须把它清零。
        CAP_TABLE.lock()[1][0] = Some(Capability::Spawn);
        HANDLE_TABLE.lock()[1][0] = Some(0xDEAD_BEEF);
        add_domain(1);
        assert!(!has(1, Capability::Spawn));
        assert!(HANDLE_TABLE.lock()[1].iter().all(|slot| slot.is_none()));
    }
}
