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
    /// 访问 I/O 端口区间 `[base, base + len)` 的能力（`SYS_PORT_IN8/IN16/OUT8/OUT16`，D0）。
    ///
    /// 端口是**纯平坦地址空间**（没有 MMIO 那种页对齐的"页基址"），故按**半开区间**授权：
    /// 一次授权覆盖一段连续端口（IDE 的 `0x1F0..0x1F8`、CMOS RTC 的 `0x70..0x72`），
    /// 而不是像 `Mmio` 那样一页一条 —— 否则 IDE 的 8 个寄存器要占 8 个能力槽。
    ///
    /// 与 `Mmio` 同样是**默认不授予**的资源凭证：此前端口 syscall 是**无门禁**的，
    /// 任何域都能读写任意端口（D0 之前的真实缺口）。
    IoPort(u16, u16),
    /// 访问**帧缓冲**的能力（`SYS_FB_INFO` / `SYS_FB_MAP` / `SYS_FB_TAKEOVER`）。
    ///
    /// 无参数 —— 帧缓冲是全局唯一资源。与 `Mmio` 的区别：MMIO 能力按「页对齐物理基址」
    /// 逐页匹配（设备 BAR 一页一条），而帧缓冲是**一整块**（可达数百页），逐页授权既塞不下
    /// 能力槽也无意义，故单列一类。
    Fb,
    /// 加载可执行文件并启动的能力 (`SYS_SPAWN_ELF`): 允许建新域 + 载入镜像 + 起任务。
    ///
    /// 无参数 —— 该能力本身就是"可以造进程"这张凭证。与其它能力一样默认不授予,
    /// 由信任方显式给（引导期给 shell / 自测域）。
    Spawn,
}

/// 每域能力槽数量。
pub const CAP_SLOTS: usize = 32;

/// `SYS_CAP_SEND` 的 `kind` 编码 —— 能力是枚举, 而 syscall 参数只有整数,
/// 故用 `(kind, arg)` 两段表示 (与用户态 `syscall::CAP_KIND_*` 一致)。
pub const CAP_KIND_SEND_TO: u64 = 0;
pub const CAP_KIND_MAP_INTO: u64 = 1;
pub const CAP_KIND_IRQ: u64 = 2;
pub const CAP_KIND_MMIO: u64 = 3;
/// `Spawn` 无参数, 故 `arg` 被忽略（但保留两段式编码, 委派路径才不必特判）。
pub const CAP_KIND_SPAWN: u64 = 4;
/// `Fb` 无参数（帧缓冲全局唯一），`arg` 同样被忽略。
pub const CAP_KIND_FB: u64 = 5;
/// `IoPort` 是**二维**的 (base, len)，而 `SYS_CAP_SEND` 只有一个 `arg`，故编码成
/// `(base << 16) | len`（各占 16 位）。
pub const CAP_KIND_IO_PORT: u64 = 6;

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
        CAP_KIND_FB => Some(Capability::Fb),
        // `IoPort`: `(base << 16) | len`。要求 len != 0、base/len 各占 16 位，
        // 且 `base + len <= 0x1_0000`（区间不越过端口空间末尾，否则永远匹配不上）。
        CAP_KIND_IO_PORT if arg >> 32 == 0 && arg & 0xFFFF != 0 => {
            let base = (arg >> 16) as u16;
            let len = (arg & 0xFFFF) as u16;
            if base as u64 + len as u64 <= 0x1_0000 {
                Some(Capability::IoPort(base, len))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 能力审计编码 (`SYS_CAP_AUDIT`): 把一条能力打包进一个 `u64` ——
/// 高 8 位存 `种类 + 1`, 低 56 位存参数 (`IoPort` 编成 `(base << 16) | len`)。
///
/// `+1` 是刻意的: 空槽约定返回 `0`, 而 `SendTo(0)` 的 `kind = 0`、`arg = 0` 打包后也是
/// `1 << 56` 而非 `0`, 故「空槽」与任何真实能力都不撞 (纯函数, 便于单测)。
pub fn pack_audit(cap: Capability) -> u64 {
    let (kind, arg) = match cap {
        Capability::SendTo(d) => (CAP_KIND_SEND_TO, d),
        Capability::MapInto(d) => (CAP_KIND_MAP_INTO, d),
        Capability::Irq(v) => (CAP_KIND_IRQ, v as u64),
        Capability::Mmio(p) => (CAP_KIND_MMIO, p),
        Capability::Spawn => (CAP_KIND_SPAWN, 0),
        Capability::Fb => (CAP_KIND_FB, 0),
        Capability::IoPort(base, len) => (CAP_KIND_IO_PORT, ((base as u64) << 16) | len as u64),
    };
    ((kind + 1) << 56) | arg
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

/// 域 `domain` 是否持有**覆盖 I/O 端口 `port`** 的能力（D0）。
///
/// 与 [`has`] 的**精确匹配**不同：`IoPort` 是区间能力，端口落进任一已授权区间
/// `[base, base + len)` 即放行。这是 `SYS_PORT_*` 的门禁判据。
pub fn has_port(domain: u64, port: u16) -> bool {
    let table = CAP_TABLE.lock();
    table.get(domain as usize).is_some_and(|slots| {
        slots.iter().any(|slot| match slot {
            Some(Capability::IoPort(base, len)) => port_in_range(*base, *len, port),
            _ => false,
        })
    })
}

/// `port` 是否落在半开区间 `[base, base + len)`（`IoPort` 的匹配判据）。
///
/// 纯函数（不碰全局表、不关中断），故可直接单测。
fn port_in_range(base: u16, len: u16, port: u16) -> bool {
    port >= base && (port as u32) < base as u32 + len as u32
}

/// 能力审计 (②): 只读地取出域 `domain` 第 `slot` 个能力槽的内容。
///
/// 返回 `Some(Some(cap))` = 该槽持有能力; `Some(None)` = 空槽; `None` = 域不存在或槽越界
/// (审计者据此判定"表尾", 见 `SYS_CAP_AUDIT`)。**不修改任何状态**, 供监督者按最小权限
/// 策略核对引导期的能力授权。
pub fn audit_slot(domain: u64, slot: u64) -> Option<Option<Capability>> {
    let table = CAP_TABLE.lock();
    let row = table.get(domain as usize)?;
    // 槽越界必须与"空槽"区分开: 前者返回 `None` (审计者据此判定表尾), 后者是 `Some(None)`。
    // 若这里用 `.flatten()` 把两者都压成 `None`, 越界会被误报成空槽 -> 审计者扫不到表尾。
    let cell = row.get(slot as usize)?;
    Some(*cell)
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
    drop(table);
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
    if !ok {
        // 能力槽耗尽 (该域需要的凭证比 CAP_SLOTS 多)。必须吵出来: 调用方拿到 false 后
        // 若静静丢掉, 表现就是运行期莫名其妙的"权限缺失", 极难定位。
        crate::video::println("[WARN] capability table full: grant dropped");
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

    /// D0: `IoPort` 是**半开区间** `[base, base + len)`；`decode` 用 `(base << 16) | len`
    /// 编码，对 `len = 0` / 区间越界 / base 超 16 位一律拒绝。
    ///
    /// 只测两个纯函数（不碰全局表），避免与上面那条用例在并行单测里互相清表。
    #[test]
    fn io_port_range_and_encoding() {
        assert!(port_in_range(0x1F0, 8, 0x1F0)); // 下界含
        assert!(port_in_range(0x1F0, 8, 0x1F7)); // 上界 - 1 含
        assert!(!port_in_range(0x1F0, 8, 0x1F8)); // 上界不含（半开）
        assert!(!port_in_range(0x1F0, 8, 0x1EF));
        assert!(!port_in_range(0x70, 2, 0x72));

        assert_eq!(
            decode(CAP_KIND_IO_PORT, (0x70 << 16) | 2),
            Some(Capability::IoPort(0x70, 2))
        );
        assert_eq!(decode(CAP_KIND_IO_PORT, 0), None); // len = 0
        assert_eq!(decode(CAP_KIND_IO_PORT, (0xFFFF << 16) | 2), None); // 区间越界
        assert_eq!(decode(CAP_KIND_IO_PORT, 1 << 32), None); // base 超 16 位
    }

    /// 审计编码: 高 8 位是 `种类 + 1`, 低 56 位是参数; `IoPort` 编成 `(base << 16) | len`。
    /// 关键不变式: 任何真实能力打包后都**非 0** (0 被空槽占用) —— 连 `SendTo(0)` 也不撞 0。
    #[test]
    fn audit_pack_encodes_kind_and_arg_without_zero_collision() {
        assert_eq!(pack_audit(Capability::SendTo(0)), 1 << 56); // 非 0
        assert_eq!(pack_audit(Capability::SendTo(5)), (1 << 56) | 5);
        assert_eq!(pack_audit(Capability::MapInto(9)), (2 << 56) | 9);
        assert_eq!(pack_audit(Capability::Irq(0x21)), (3 << 56) | 0x21);
        assert_eq!(
            pack_audit(Capability::Mmio(0x8000_0000)),
            (4 << 56) | 0x8000_0000
        );
        assert_eq!(pack_audit(Capability::Spawn), 5 << 56);
        assert_eq!(pack_audit(Capability::Fb), 6 << 56);
        assert_eq!(
            pack_audit(Capability::IoPort(0x1F0, 8)),
            (7 << 56) | (0x1F0 << 16) | 8
        );

        // 解出 `种类 + 1` 与参数后应能还原 (与 `decode` 的 `IoPort` 编码一致)。
        let packed = pack_audit(Capability::IoPort(0x70, 2));
        let arg = packed & 0x00FF_FFFF_FFFF_FFFF;
        assert_eq!(
            decode(CAP_KIND_IO_PORT, arg),
            Some(Capability::IoPort(0x70, 2))
        );
    }
}
