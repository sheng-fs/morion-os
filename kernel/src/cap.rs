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
    /// 绑定网络端口闭区间 `[port_lo, port_hi]` 的能力（N6）。
    ///
    /// 与 `IoPort` 同型（区间能力），但端口空间是 16 位、且这里是**闭区间**：网络里
    /// "绑一个端口"即 `lo == hi`，给一段（如 `1024..=65535`）则一次授权覆盖非特权端口段。
    /// `SYS_NET_BIND` 用它判定调用者能否绑定某端口；`netstack_srv` 收 `bind` 请求时用
    /// `SYS_NET_OWNER` 核对归属域（`msg.from`），二者配合实现不可伪造的端口门禁。
    Net(u16, u16),
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

/// 能力槽项: 能力本身 + **委派来源**。
///
/// 来源是「撤销」正确性的根。`Capability` 是纯值、没有身份, 只记值的话 `revoke`
/// 无法区分「本域自己持有的根能力」与「别人复制给我的一分子」—— 要么漏掉副本,
/// 要么误伤其它域的独立授权。
///
/// - `parent = None`: 内核/权威方 `grant` 的根能力;
/// - `parent = Some((域, 槽))`: 由该槽经 `delegate` **复制**而来 (`SYS_CAP_SEND`)。
///
/// 槽号用 `u16` 足够 (`CAP_SLOTS = 32`)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct CapEntry {
    cap: Capability,
    parent: Option<(u64, u16)>,
}

/// 一个域的能力行 (定长, 按槽号索引)。
type CapRow = [Option<CapEntry>; CAP_SLOTS];

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
/// `Net` (N6): 与 `IoPort` 同型, `arg = (port_lo << 16) | port_hi`（闭区间）。
pub const CAP_KIND_NET: u64 = 7;

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
        // `Net`: `(lo << 16) | hi`（闭区间）。要求 hi 占低 16 位且 `lo <= hi`。
        CAP_KIND_NET if arg >> 32 == 0 => {
            let lo = (arg >> 16) as u16;
            let hi = (arg & 0xFFFF) as u16;
            if lo <= hi {
                Some(Capability::Net(lo, hi))
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
        Capability::Net(lo, hi) => (CAP_KIND_NET, ((lo as u64) << 16) | hi as u64),
    };
    ((kind + 1) << 56) | arg
}

/// 全局能力表: 每个域一个能力槽数组。
static CAP_TABLE: Mutex<Vec<CapRow>> = Mutex::new(Vec::new());

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

/// `table` 中域 `domain` 是否持有 `cap`。域不在表里 (未建/已销毁) 一律 false ——
/// 不能用 `table[domain]` 直接索引: 域号来自 syscall 参数, 越界会 panic (内核 DoS)。
fn holds_in(table: &[CapRow], domain: u64, cap: Capability) -> bool {
    table
        .get(domain as usize)
        .is_some_and(|row| row.iter().flatten().any(|e| e.cap == cap))
}

/// 检查某域是否持有指定能力 (须在关中断下调用)。
pub fn has(domain: u64, cap: Capability) -> bool {
    holds_in(&CAP_TABLE.lock(), domain, cap)
}

/// 域 `domain` 是否持有**覆盖 I/O 端口 `port`** 的能力（D0）。
///
/// 与 [`has`] 的**精确匹配**不同：`IoPort` 是区间能力，端口落进任一已授权区间
/// `[base, base + len)` 即放行。这是 `SYS_PORT_*` 的门禁判据。
pub fn has_port(domain: u64, port: u16) -> bool {
    let table = CAP_TABLE.lock();
    table.get(domain as usize).is_some_and(|row| {
        row.iter().flatten().any(|e| match e.cap {
            Capability::IoPort(base, len) => port_in_range(base, len, port),
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

/// 域 `domain` 是否持有**覆盖网络端口 `port`** 的能力（N6）。
///
/// 与 [`has_port`] 同型；`Net` 是**闭区间** `[lo, hi]`（`lo == hi` = 单个端口）。
/// 这是 `SYS_NET_BIND` 的门禁判据。
pub fn has_net_port(domain: u64, port: u16) -> bool {
    let table = CAP_TABLE.lock();
    table.get(domain as usize).is_some_and(|row| {
        row.iter().flatten().any(|e| match e.cap {
            Capability::Net(lo, hi) => net_port_in_range(lo, hi, port),
            _ => false,
        })
    })
}

/// `port` 是否落在闭区间 `[lo, hi]`（`Net` 的匹配判据；纯函数, 可单测）。
fn net_port_in_range(lo: u16, hi: u16, port: u16) -> bool {
    port >= lo && port <= hi
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
    Some(cell.map(|e| e.cap))
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

/// 在 `table` 里给 `domain` 签发一条**根**能力 (来源为空), 成功返回 true。
///
/// 域不在表里 (已销毁/越界) 或槽位耗尽返回 false (不 panic)。
fn grant_in(table: &mut [CapRow], domain: u64, cap: Capability) -> bool {
    let Some(row) = table.get_mut(domain as usize) else {
        return false;
    };
    for slot in row.iter_mut() {
        if slot.is_none() {
            *slot = Some(CapEntry { cap, parent: None });
            return true;
        }
    }
    false
}

/// 向某域授予能力 (占用一个空槽)。
pub fn grant(domain: u64, cap: Capability) -> bool {
    // 保存/恢复中断状态: boot 期 (IF=0) 调用时不能提前开启中断,
    // 否则 PIT 会在调度器尚未就绪时触发 schedule 导致 panic。
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let mut table = CAP_TABLE.lock();
    let ok = grant_in(&mut table, domain, cap);
    drop(table);
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
    if !ok {
        // 能力槽耗尽 (该域需要的凭证比 CAP_SLOTS 多)，或域已不存在。必须吵出来:
        // 调用方拿到 false 后若静静丢掉, 表现就是运行期莫名其妙的"权限缺失", 极难定位。
        crate::video::println("[WARN] capability table full: grant dropped");
    }
    ok
}

/// 撤销域 `domain` 的 `cap` 及其**全部派生副本**。
///
/// 语义:
///   1. 摘掉该域所有 `cap` 槽 (同一能力可能被重复 `grant`, 不止一个槽);
///   2. 沿委派来源做**级联**: 任何以这些槽为 `parent` 的复制件 (可能跨域、可多跳)
///      一并失效 —— 否则撤销只作用于本域, 已 `SYS_CAP_SEND` 出去的副本还能继续用。
///   3. **不动**其它域自己 `grant` 得到的同值能力 (它们没有这条来源链)。
///
/// 返回 false = 该域没有这条能力 / 域不存在。
fn revoke_in(table: &mut [CapRow], domain: u64, cap: Capability) -> bool {
    if table.get(domain as usize).is_none() {
        return false;
    }

    // 第一步: 本域所有直接命中的槽 (可能就是多个)。
    let mut revoked: Vec<(u64, u16)> = Vec::new();
    {
        let row = &mut table[domain as usize];
        for (i, slot) in row.iter_mut().enumerate() {
            if slot.is_some_and(|e| e.cap == cap) {
                *slot = None;
                revoked.push((domain, i as u16));
            }
        }
    }
    if revoked.is_empty() {
        return false;
    }

    // 第二步: 广度优先找派生副本。每个槽只会被摘一次 (摘掉后变 `None`), 故必然终止。
    let mut i = 0;
    while i < revoked.len() {
        let parent = revoked[i];
        i += 1;
        for (d, row) in table.iter_mut().enumerate() {
            for (s, slot) in row.iter_mut().enumerate() {
                if slot.is_some_and(|e| e.parent == Some(parent)) {
                    *slot = None;
                    revoked.push((d as u64, s as u16));
                }
            }
        }
    }
    true
}

/// 撤销某域的指定能力 (含已委派出去的副本)。
pub fn revoke(domain: u64, cap: Capability) -> bool {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let mut table = CAP_TABLE.lock();
    let ok = revoke_in(&mut table, domain, cap);
    drop(table);
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
    ok
}

/// 在 `table` 里把 `cap` 从 `from` 复制给 `to`, 并记录来源槽。
///
/// 约束:
///   - **不允许放大**: `from` 没持有 `cap` 就失败 (没有的能力给不出去);
///   - `to` 已持有同值能力时幂等返回 true, 不占新槽;
///   - `to` 不在表里 (不存在) 或槽满返回 false, 不改变状态。
fn delegate_in(table: &mut [CapRow], from: u64, to: u64, cap: Capability) -> bool {
    // 先定位来源槽 (复制件要记住它, 撤销才能级联)。
    let Some(parent) = table.get(from as usize).and_then(|row| {
        row.iter()
            .position(|slot| slot.is_some_and(|e| e.cap == cap))
    }) else {
        return false;
    };
    let Some(row) = table.get_mut(to as usize) else {
        return false;
    };
    if row.iter().flatten().any(|e| e.cap == cap) {
        return true; // 幂等: 不重复占槽
    }
    for slot in row.iter_mut() {
        if slot.is_none() {
            *slot = Some(CapEntry {
                cap,
                parent: Some((from, parent as u16)),
            });
            return true;
        }
    }
    false
}

/// 把 `cap` 从 `from` 域**委派**给 `to` 域 (「能力随 IPC 传递」)。
///
/// 核心约束是**不允许放大**: `from` 必须自己持有 `cap`, 否则一律失败 ——
/// 没有的能力给不出去, 这是能力安全模型的根。检查与写入在**同一把锁**内完成,
/// 避免「检查后被抢先」。
///
/// `to` 已经持有该能力时直接返回成功且不占新槽 (幂等), 免得重复委派把槽位耗光。
pub fn delegate(from: u64, to: u64, cap: Capability) -> bool {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let mut table = CAP_TABLE.lock();
    let ok = delegate_in(&mut table, from, to, cap);
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
        CAP_TABLE.lock()[1][0] = Some(CapEntry {
            cap: Capability::SendTo(2),
            parent: None,
        });
        HANDLE_TABLE.lock()[1][0] = Some(0xDEAD_BEEF);
        assert!(has(1, Capability::SendTo(2)));

        // 销毁域 1: 能力与句柄都不再可用。
        remove_domain(1);
        assert!(!has(1, Capability::SendTo(2)));
        assert!(HANDLE_TABLE.lock()[1].iter().all(|slot| slot.is_none()));

        // 槽位复用 (同一个 id 再建域): 又摆一份旧值, `add_domain` 必须把它清零。
        CAP_TABLE.lock()[1][0] = Some(CapEntry {
            cap: Capability::Spawn,
            parent: None,
        });
        HANDLE_TABLE.lock()[1][0] = Some(0xDEAD_BEEF);
        add_domain(1);
        assert!(!has(1, Capability::Spawn));
        assert!(HANDLE_TABLE.lock()[1].iter().all(|slot| slot.is_none()));
    }

    // ---- 以下是纯函数测试: 只用局部的表, 不碰全局 `CAP_TABLE`, 不与并行用例互相清表。 ----

    fn local_table(n: usize) -> Vec<CapRow> {
        let mut t = Vec::new();
        for _ in 0..n {
            t.push([None; CAP_SLOTS]);
        }
        t
    }

    fn entry(cap: Capability, parent: Option<(u64, u16)>) -> Option<CapEntry> {
        Some(CapEntry { cap, parent })
    }

    /// 委派必须是**复制**且**不可放大**: 自己没持有就失败; 成功后源仍持有, 副本记下来源;
    /// 重复委派幂等, 不占新槽。
    #[test]
    fn delegate_cannot_amplify_and_records_provenance() {
        let mut t = local_table(2);
        assert!(!delegate_in(&mut t, 0, 1, Capability::SendTo(2))); // 没有的能力给不出去
        assert!(!holds_in(&t, 1, Capability::SendTo(2)));

        assert!(grant_in(&mut t, 0, Capability::SendTo(2)));
        assert!(delegate_in(&mut t, 0, 1, Capability::SendTo(2)));
        assert!(holds_in(&t, 0, Capability::SendTo(2)), "复制语义: 源仍持有");
        assert!(holds_in(&t, 1, Capability::SendTo(2)));
        assert_eq!(t[1][0].unwrap().parent, Some((0, 0)));

        // 幂等: 再来一次不新增槽。
        assert!(delegate_in(&mut t, 0, 1, Capability::SendTo(2)));
        assert_eq!(t[1].iter().flatten().count(), 1);
    }

    /// 撤销必须让**后续使用**失败: 本域直接槽 (含重复 `grant` 的多份) 与经 `SYS_CAP_SEND`
    /// 复制出去的多跳副本一并失效。
    #[test]
    fn revoke_clears_all_copies_and_delegated_descendants() {
        let mut t = local_table(4);
        // 域 0: 两条 mfs 根能力 (重复 grant); 域 1 复制自 (0,0); 域 2 再复制自 (1,0)。
        t[0][0] = entry(Capability::SendTo(3), None);
        t[0][1] = entry(Capability::SendTo(3), None);
        t[1][0] = entry(Capability::SendTo(3), Some((0, 0)));
        t[2][0] = entry(Capability::SendTo(3), Some((1, 0)));
        // 不相关的能力: 必须原样保留。
        t[3][0] = entry(Capability::MapInto(9), None);

        assert!(revoke_in(&mut t, 0, Capability::SendTo(3)));
        for d in 0..3 {
            assert!(
                !holds_in(&t, d, Capability::SendTo(3)),
                "域 {d} 的副本应随撤销失效"
            );
        }
        assert!(holds_in(&t, 3, Capability::MapInto(9)), "无关能力不受影响");
        assert!(t[0].iter().all(Option::is_none));
    }

    /// 撤销**不受牵连**别的域独立 `grant` 的同值能力 (没有委派来源链); 也拒绝不存在的域。
    #[test]
    fn revoke_spares_independent_grants_and_rejects_unknown_domain() {
        let mut t = local_table(2);
        t[0][0] = entry(Capability::SendTo(7), None);
        t[1][0] = entry(Capability::SendTo(7), None); // 另一条权威根, 非域 0 的副本

        assert!(revoke_in(&mut t, 0, Capability::SendTo(7)));
        assert!(!holds_in(&t, 0, Capability::SendTo(7)));
        assert!(holds_in(&t, 1, Capability::SendTo(7)), "独立授权不应被误伤");

        // 不存在的域: 一律失败且不 panic / 不扩表。
        assert!(!revoke_in(&mut t, 9, Capability::SendTo(7)));
        assert!(!grant_in(&mut t, 9, Capability::Spawn));
        assert!(!delegate_in(&mut t, 9, 0, Capability::Spawn));
        assert!(!holds_in(&t, 9, Capability::Spawn));
        assert_eq!(t.len(), 2);
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

    /// N6: `Net` 是**闭区间** `[lo, hi]`（与 `IoPort` 的半开区间不同）；`decode` 用
    /// `(lo << 16) | hi` 编码，`lo > hi` 或超 32 位一律拒绝。
    #[test]
    fn net_port_range_and_encoding() {
        assert!(net_port_in_range(1024, 65535, 1024)); // 下界含
        assert!(net_port_in_range(1024, 65535, 65535)); // 上界**含**（闭区间）
        assert!(!net_port_in_range(1024, 65535, 1023));
        assert!(net_port_in_range(7, 7, 7)); // 单端口
        assert!(!net_port_in_range(7, 7, 8));

        assert_eq!(
            decode(CAP_KIND_NET, (53 << 16) | 53),
            Some(Capability::Net(53, 53))
        );
        assert_eq!(
            decode(CAP_KIND_NET, (1024 << 16) | 65535),
            Some(Capability::Net(1024, 65535))
        );
        assert_eq!(decode(CAP_KIND_NET, (1024 << 16) | 80), None); // lo > hi
        assert_eq!(decode(CAP_KIND_NET, 1 << 32), None); // 超 32 位

        // 审计打包: 种类 7 → 包成 `8 << 56`，闭区间编码自洽。
        assert_eq!(
            pack_audit(Capability::Net(1024, 65535)),
            (8 << 56) | (1024 << 16) | 65535
        );
    }
}
