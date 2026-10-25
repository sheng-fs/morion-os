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

/// 环形缓冲里现存**最老**字节的绝对偏移 (`total - LOG_CAP`; 未满时为 0)。
///
/// 这是当前**仍可读到的起点**: 缓冲写满后更早的日志已被覆盖, [`read`] 会把
/// 小于它的 `start` **静默夹**到此处。调用方若不知道这个起点, 就会反复读到同一
/// 窗口 —— 故窗口型读取必须用本值当起点、`total()` 当终点 (见 shell `dmesg`)。
pub fn oldest() -> u64 {
    unsafe { oldest_of(LOG_TOTAL) }
}

/// 由累计字节数算最老存活偏移 (与 [`oldest`] 同一口径, 避免两处算法漂移)。
fn oldest_of(total: u64) -> u64 {
    total.saturating_sub(LOG_CAP as u64)
}

/// 从**绝对偏移** `start` 起读最多 `dst.len()` 字节, 返回实际字节数。
///
/// 环形里现存的最老绝对偏移 = `total - LOG_CAP`（`total > LOG_CAP` 时）；`start`
/// 早于它会被**静默夹**到最老处 —— 调用方拿不到实际起点, 故按 `start` 推进的读循环
/// 会反复读到同一窗口。要读整个存活窗口, 起点请用 [`oldest`]、终点用 [`total`]
/// (且终点须取**快照**: 打印会经 `video::print` 回灌进本缓冲, 实时 total 会一路涨)。
///
/// 绝对偏移 `s` 的字节恒在环形下标 `s % LOG_CAP` —— 缓冲从 0 顺序写入、写满从头
/// 覆盖，故写入位置 `LOG_HEAD == LOG_TOTAL % LOG_CAP`，与 `s % LOG_CAP` 一致。
pub fn read(start: u64, dst: &mut [u8]) -> usize {
    unsafe {
        let total = LOG_TOTAL;
        let cap = LOG_CAP as u64;
        let oldest = oldest_of(total);
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
    use spin::Mutex;

    /// 单测在宿主上**多线程并行**跑, 而本模块的现场是 `static mut` —— 用一把锁把
    /// 各用例串起来 (内核本身单核, 无需此锁; 锁只服务宿主单测)。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 摆一个干净的现场 (不经 `video::print`: 那条路会 `cli`/`sti` 关开中断, 是特权
    /// 指令, 在宿主单测里执行会 SIGSEGV —— 单测跑在用户态)。
    fn reset() {
        unsafe {
            LOG_BUF.fill(0);
            LOG_HEAD = 0;
            LOG_TOTAL = 0;
        }
    }

    #[test]
    fn oldest_is_zero_until_the_ring_wraps() {
        let _g = TEST_LOCK.lock();
        reset();
        capture_bytes(b"hello");
        assert_eq!(total(), 5);
        assert_eq!(oldest(), 0);
    }

    #[test]
    fn oldest_tracks_the_surviving_window() {
        let _g = TEST_LOCK.lock();
        reset();
        // 写满一圈再多写 10 字节 → 最老的 10 字节已被覆盖, 窗口只剩末尾 10 字节。
        capture_bytes(&alloc::vec![b'x'; LOG_CAP]);
        capture_bytes(b"0123456789");
        assert_eq!(total(), LOG_CAP as u64 + 10);
        assert_eq!(oldest(), 10);
    }

    #[test]
    fn read_clamps_start_to_the_oldest_byte() {
        let _g = TEST_LOCK.lock();
        reset();
        capture_bytes(&alloc::vec![b'x'; LOG_CAP]);
        capture_bytes(b"0123456789");
        // 内核把过小的 `start` **静默夹**到最老处, 调用方拿不到实际起点 —— 按自己
        // 的偏移推进会反复读到同一窗口 (shell `dmesg` 旧实现死循环 / 重复落盘的根因)。
        // 存活窗口 = `[oldest, total)` = `[10, 65546)`: 开头仍是第一圈留下的 'x',
        // 末尾才是刚写进去的 "0123456789"。
        let mut a = [0u8; 10];
        let mut b = [0u8; 10];
        let n1 = read(0, &mut a);
        let n2 = read(oldest(), &mut b);
        assert_eq!(n1, 10);
        assert_eq!(n1, n2);
        assert_eq!(&a[..n1], &b[..n2]);
        assert_eq!(&a[..n1], &[b'x'; 10]);
        let mut tail = [0u8; 10];
        let n3 = read(total() - 10, &mut tail);
        assert_eq!(n3, 10);
        assert_eq!(&tail[..n3], b"0123456789");
    }

    #[test]
    fn window_read_terminates_when_prints_feed_back_into_the_ring() {
        let _g = TEST_LOCK.lock();
        reset();
        capture_bytes(&alloc::vec![b'x'; LOG_CAP]);
        capture_bytes(b"0123456789");
        // 复刻 shell `dmesg` 的读循环: 打印经 `video::print` 回灌进同一个环形缓冲,
        // 故终点必须取**快照** —— 若用实时 total 当终点, total 一路涨, 循环永不停歇。
        let start = oldest();
        let end = total();
        assert_eq!(end - start, LOG_CAP as u64);
        let mut off = start;
        let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let mut iters = 0u32;
        while off < end {
            let want = core::cmp::min((end - off) as usize, 4096);
            let mut buf = [0u8; 4096];
            let n = read(off, &mut buf[..want]);
            if n == 0 {
                break;
            }
            capture_bytes(&buf[..n]); // 模拟 `sys_puts` → `video::print` 的回灌
            out.extend_from_slice(&buf[..n]);
            off += n as u64;
            iters += 1;
            assert!(iters < 64, "读循环未收敛");
        }
        // 整个存活窗口被**恰好**读出一次: 不重复 (夹取陷阱), 不少读, 也不错位 ——
        // 回灌的字节与被覆盖的字节同值, 故内容仍是原始日志。
        let mut expect = alloc::vec![b'x'; LOG_CAP - 10];
        expect.extend_from_slice(b"0123456789");
        assert_eq!(out.len(), LOG_CAP);
        assert_eq!(out, expect);
        assert_eq!(iters, (LOG_CAP / 4096) as u32);
    }
}
