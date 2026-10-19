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

    /// 回归: `shell` 的 `dmesg`(无参) 打印循环 —— 打印路径回灌进同一环形缓冲。
    ///
    /// `dmesg` 每打印一段就经 `video::print` → [`capture_bytes`] 把刚打印的字节**又写回**
    /// 这个环形缓冲, 令 [`total`] 随打印持续增长。历史缺陷: 循环以**实时** `total` 为界、
    /// 且读起点从 0 起, 于是每轮读回的正是自己刚写出的字节, `read` 永不返回 0 ——
    /// `dmesg` 卡死整个 shell。
    ///
    /// 本测试复现「环形已满 + 回灌增长」的真实条件, 验证修复所用的有界排空算法:
    /// 以进入前的 `total` 快照为界、读起点对齐到环形现存最老偏移, 能在**有限步**内
    /// 终止, 且恰好打印出快照内存活的那段日志 (顺序与内容一致)。
    #[test]
    fn dmesg_print_loop_terminates_despite_echo_feedback() {
        // 先灌入 > 容量的日志, 制造环形回绕 (最老字节已被挤掉) 的真实场景。
        // 字节内容为数字循环, 便于校验顺序。
        let seeded: usize = LOG_CAP + 1000;
        for i in 0..seeded {
            let b = [(b'0' + (i % 10) as u8)];
            capture_bytes(&b);
        }

        let end = total(); // 打印前的快照: 循环界钉死在此, 不随回灌增长。
        assert!(end >= LOG_CAP as u64, "seed must overflow the ring");

        // 与修复后 shell 同构的打印循环 (+ 模拟 sys_puts 的回灌)。
        let mut off = end.saturating_sub(LOG_CAP as u64);
        let mut printed: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        let mut iters = 0usize;
        while off < end {
            iters += 1;
            assert!(
                iters <= LOG_CAP / 4096 + 2,
                "loop must converge in finite steps"
            );
            let want = ((end - off) as usize).min(buf.len());
            let n = read(off, &mut buf[..want]);
            if n == 0 {
                break;
            }
            capture_bytes(&buf[..n]); // 回灌: 打印的字节写回同一环形缓冲
            printed.extend_from_slice(&buf[..n]);
            off += n as u64;
        }

        // 恰好排空快照内存活的 LOG_CAP 字节, 顺序与播种一致 (从绝对偏移 1000 起)。
        assert_eq!(printed.len(), LOG_CAP);
        for (k, &b) in printed.iter().enumerate() {
            assert_eq!(b, b'0' + ((1000 + k) % 10) as u8, "byte {k} out of order");
        }

        // 佐证反馈确实发生: 回灌令 total 在快照之外各增长了 LOG_CAP 字节。
        assert_eq!(total(), end + LOG_CAP as u64);
    }
}
