//! 真机日志环形缓冲 —— 收口路线 **Phase 0 / P0.1**。
//!
//! 背景：真机（笔记本）**没有物理串口**，而现有取证全靠 COM1（`-serial file:`）。
//! 真机上跑驱动时日志拿不出来 → 驱动开发在真机上是**全盲**的。
//!
//! 做法：把**所有控制台输出**（内核与用户态 —— 用户输出经 `SYS_PUTS` 汇入
//! [`crate::video::print`]）额外追加进一个**有界环形缓冲**；新增两个 syscall
//! （`SYS_LOG_TOTAL` / `SYS_LOG_READ`）让用户态把整段日志读出来，shell 的
//! `dmesg` 命令据此打印、并可重定向写入文件（如 U 盘上的 `boot.log`）。
//! 真机流程 = 从 U 盘启动 → `dmesg /boot.log` → 拔盘插到宿主机 → 取文件。
//!
//! 缓冲满了**挤掉最老的字节**：真机排障关心的是启动尾部（最近的日志）。

/// 环形缓冲容量 (64 KiB)；超出的最老日志被覆盖。
const LOG_CAP: usize = 64 * 1024;
/// 单次 `SYS_LOG_READ` 允许的最大字节数（防调用方传入超大长度）。
const LOG_READ_MAX: u64 = 1 << 20;

static mut LOG_BUF: [u8; LOG_CAP] = [0; LOG_CAP];
/// 下一个写入位置 (环形下标)。
static mut LOG_HEAD: usize = 0;
/// 历史累计写入字节数 (绝对偏移的基准, 只增不减)。
static mut LOG_TOTAL: u64 = 0;

/// 追加一段字节到日志缓冲。
///
/// 调用方 [`crate::video::print`] 已关中断并持打印锁, 故此处无需再加锁。
pub fn capture_bytes(bytes: &[u8]) {
    unsafe {
        for &b in bytes {
            LOG_BUF[LOG_HEAD] = b;
            LOG_HEAD = (LOG_HEAD + 1) % LOG_CAP;
            LOG_TOTAL += 1;
        }
    }
}

/// 累计写入字节数 (`SYS_LOG_TOTAL`)。
pub fn total() -> u64 {
    unsafe { LOG_TOTAL }
}

/// 从**绝对偏移** `start` 起读最多 `dst.len()` 字节, 返回实际字节数。
///
/// 环形里现存的最老绝对偏移 = `total - LOG_CAP`（`total > LOG_CAP` 时）；`start`
/// 早于它会被夹到最老处。`start >= total` 返回 0（已读到头）。
///
/// 绝对偏移 `s` 的字节恒在环形下标 `s % LOG_CAP` —— 缓冲从 0 顺序写入、写满从头
/// 覆盖，故写入位置 `LOG_HEAD == LOG_TOTAL % LOG_CAP`，与 `s % LOG_CAP` 一致。
pub fn read(start: u64, dst: &mut [u8]) -> usize {
    unsafe {
        let total = LOG_TOTAL;
        let cap = LOG_CAP as u64;
        let oldest = total.saturating_sub(cap);
        let mut s = if start < oldest { oldest } else { start };
        if s >= total {
            return 0;
        }
        let mut n = 0usize;
        while s < total && n < dst.len() {
            dst[n] = LOG_BUF[(s % cap) as usize];
            n += 1;
            s += 1;
        }
        n
    }
}

/// `SYS_LOG_READ` 的处理入口：`a1` = 用户缓冲指针、`a2` = 长度、`a3` = 起始绝对偏移。
///
/// 返回写入字节数；参数非法 / 越界 / 读到头一律返回 0。复制前按
/// [`crate::memory::paging::is_user_address`] 校验用户区间（与 `SYS_UNAME` 同口径）。
pub fn handle_read(a1: u64, a2: u64, a3: u64) -> u64 {
    if a1 == 0 || a2 == 0 {
        return 0;
    }
    let len = if a2 > LOG_READ_MAX { LOG_READ_MAX } else { a2 };
    if !crate::memory::paging::is_user_address(a1)
        || !crate::memory::paging::is_user_address(a1 + len - 1)
    {
        return 0;
    }
    let dst = unsafe { core::slice::from_raw_parts_mut(a1 as *mut u8, len as usize) };
    read(a3, dst) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 重置全局状态。所有断言集中在**同一个** `#[test]` 内 —— 测试跑在同一进程,
    /// 环形缓冲是全局静态, 拆成多个测试会并行互相污染。
    fn reset() {
        unsafe {
            LOG_HEAD = 0;
            LOG_TOTAL = 0;
            LOG_BUF.fill(0);
        }
    }

    /// P0.1 契约 + shell `dmesg` 排空边界回归。
    ///
    /// 缺陷: `dmesg` 打印日志时, 输出经 `SYS_PUTS` → `video::print` → `capture_bytes`
    /// **再捕获回同一个环形缓冲**, 使 `LOG_TOTAL` 随打印持续增长。若排空循环以"即时
    /// total"为终点, 打印出的日志又变成新的待读日志 → 永远追不上 (死循环)。修复 = 以
    /// 进入命令时的 **total 快照** 为终点。本测试用带步数上限的循环复现"自产自读"场景,
    /// 验证按快照边界能恰好读完并终止 (旧实现会在此触顶)。
    #[test]
    fn snapshot_bounded_drain_and_wrap_contract() {
        // ---- 1. 自产自读: 以快照为终点必须终止并恰好读完 ----
        reset();
        let first = b"[OK] PCI devices found: 9\n";
        capture_bytes(first);
        let snapshot = total();
        assert_eq!(snapshot, first.len() as u64);

        let mut dst = [0u8; 8];
        assert_eq!(read(snapshot, &mut dst), 0, "到达 total 即读到头");

        let mut out = [0u8; 8];
        let mut readback = [0u8; 64];
        let mut off = 0u64;
        let mut pos = 0usize;
        let mut steps = 0u32;
        while off < snapshot {
            steps += 1;
            assert!(steps < 64, "排空未终止 —— 快照边界失效 (自产自读死循环)");
            let n = read(off, &mut out) as u64;
            let n = core::cmp::min(n, snapshot - off);
            assert!(n > 0);
            readback[pos..pos + n as usize].copy_from_slice(&out[..n as usize]);
            pos += n as usize;
            // "打印": 读到什么就再捕获回缓冲 (等价于 dmesg 经 SYS_PUTS 的回声)。
            capture_bytes(&out[..n as usize]);
            off += n;
        }
        assert_eq!(&readback[..pos], first);
        // 回声确实把 total 推高了 —— 这正是"以即时 total 为终点永不终止"的根源。
        assert!(total() > snapshot);
        assert!(read(snapshot, &mut dst) > 0, "回声使快照之后仍有字节可读");

        // ---- 2. 回绕: 写满后最老字节被覆盖, 早于 oldest 的偏移夹到最老处 ----
        reset();
        let total_bytes = LOG_CAP + 16;
        let val = |abs: usize| -> u8 { (abs % 251) as u8 };
        let mut chunk = [0u8; 256];
        let mut i = 0usize;
        while i < total_bytes {
            let n = core::cmp::min(chunk.len(), total_bytes - i);
            for k in 0..n {
                chunk[k] = val(i + k);
            }
            capture_bytes(&chunk[..n]);
            i += n;
        }
        let total2 = total();
        assert_eq!(total2, total_bytes as u64);
        let oldest = total2 - LOG_CAP as u64;

        let mut d = [0u8; 8];
        // 起始偏移早于 oldest: 夹到最老处, 读回的是绝对偏移 oldest 处的字节。
        let n = read(oldest - 8, &mut d);
        assert!(n > 0);
        assert_eq!(d[0], val(oldest as usize));
        // 从 oldest 起逐字节与写入内容一致。
        let n = read(oldest, &mut d);
        for (k, b) in d[..n].iter().enumerate() {
            assert_eq!(*b, val(oldest as usize + k));
        }
        // 读到头: 起始偏移 >= total 一律返回 0。
        assert_eq!(read(total2, &mut d), 0);
        assert_eq!(read(total2 + 7, &mut d), 0);
    }
}
