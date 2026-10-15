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
    use alloc::vec::Vec;

    /// 回归 (Phase 0 / P0.1): shell 的 `dmesg` 无参打印经 `SYS_PUTS` → `video::print` 把输出
    /// **回灌进同一个环形缓冲** (`capture_bytes`)。于是「读一段、打印一段」的循环里 `off` 与
    /// `total` 同速前进、保留窗口长度恒定, `read` 永远返回非 0 —— 以**实时** `total` 为界会死循环。
    /// 本测 (a) 复现该缺陷行为, (b) 证明以**进入时的 `total` 快照**为界则必在有限步内结束、且只
    /// 吐快照窗口 (现 shell 实现)。
    ///
    /// 全局环形缓冲: 本文件唯一测试, 独占 `LOG_*` 状态且两步串行, 无并行竞争。
    #[test]
    fn dmesg_echo_loop_must_be_snapshot_bounded() {
        let payload: Vec<u8> = (0..(LOG_CAP + 4096)).map(|i| (i % 251) as u8).collect();
        capture_bytes(&payload);

        let mut buf = [0u8; 512];

        // (a) 缺陷行为: 以实时 `total` 为界 (旧 shell 实现) —— 4096 步仍读不到头。
        let mut off = 0u64;
        let mut steps = 0u32;
        loop {
            let n = read(off, &mut buf);
            if n == 0 {
                break;
            }
            capture_bytes(&buf[..n]); // 模拟打印回灌
            off += n as u64;
            steps += 1;
            if steps == 4096 {
                break;
            }
        }
        assert_eq!(steps, 4096, "无快照界时应永远读不到头 (缺陷行为)");

        // (b) 修复行为: 以进入时的 `total` 作快照为界 (现 shell 实现), 有限步结束, 只吐窗口。
        let end = total();
        let mut off = end.saturating_sub(LOG_CAP as u64);
        let mut emitted: Vec<u8> = Vec::new();
        let mut steps = 0u32;
        while off < end {
            let want = ((end - off) as usize).min(buf.len());
            let n = read(off, &mut buf[..want]);
            if n == 0 {
                break;
            }
            capture_bytes(&buf[..n]); // 打印回灌也不影响: 界是快照, 不随 total 增长
            emitted.extend_from_slice(&buf[..n]);
            off += n as u64;
            steps += 1;
            assert!(
                steps <= (LOG_CAP / buf.len()) as u32 + 2,
                "快照界应有限步结束"
            );
        }
        assert_eq!(emitted.len(), LOG_CAP, "只吐 64 KiB 快照窗口");
    }
}
