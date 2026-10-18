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

    /// 从绝对偏移 `start` 起连续读到 `stop`（模拟 shell 的读回循环，不含回灌）。
    fn read_all_from(start: u64, stop: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut dst = [0u8; 4096];
        let mut off = start;
        while off < stop {
            let n = read(off, &mut dst);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&dst[..n]);
            off += n as u64;
        }
        out
    }

    /// `dmesg` 读回的两项回归（合并为单测，避免多个测试并发改动全局环形缓冲）：
    ///
    /// (1) 打印分支的「读回 → 回灌」循环必须以「打印前快照」为界终止；
    /// (2) 日志超过容量后必须从最老处（`total - LOG_CAP`）起读，否则 `read` 的夹取
    ///     会让 `off` 与真实位置错位、内容重复。
    #[test]
    fn dmesg_readback_regressions() {
        // ---- (1) 回灌循环的终止性 ----
        let payload = [b'x'; 40_000];
        capture_bytes(&payload);
        let snapshot = total();
        let cap = LOG_CAP as u64;

        let mut dst = [0u8; 4096];
        let mut off = snapshot.saturating_sub(cap); // 从最老处起读
        let mut steps = 0u32;
        while off < snapshot {
            let rem = ((snapshot - off) as usize).min(dst.len());
            let n = read(off, &mut dst[..rem]);
            if n == 0 {
                break;
            }
            capture_bytes(&dst[..n]); // 模拟 sys_puts → print 的回灌
            off += n as u64;
            steps += 1;
            assert!(
                steps <= 1_000_000,
                "feedback loop must terminate (steps={steps})"
            );
        }
        assert!(off >= snapshot, "loop must reach the snapshot boundary");
        // 回灌让 total 超过快照，故 off < total —— `n == 0` 永不成立，不能作终止条件。
        assert!(read(off, &mut dst) > 0);

        // ---- (2) 起读位置必须是最老处，而非 0 ----
        // 造一段长度 > 容量的可辨识内容：回绕后保留的正是其后 LOG_CAP 字节。
        let mut payload2 = Vec::with_capacity(100_000);
        for i in 0..100_000u32 {
            payload2.push((i % 251) as u8);
        }
        capture_bytes(&payload2);
        let total_now = total();
        let cap2 = (LOG_CAP as u64).min(total_now);
        let expected = &payload2[payload2.len() - cap2 as usize..];

        // 从 0 起读（修复前的写法）：被夹到最老处后与真实位置错位 → 与期望不符。
        assert_ne!(read_all_from(0, total_now), expected.to_vec());
        // 从最老处起读（修复后）：得到正确的最近 `cap2` 字节。
        assert_eq!(
            read_all_from(total_now - cap2, total_now),
            expected.to_vec()
        );
    }
}
