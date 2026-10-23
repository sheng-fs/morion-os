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

    /// 重置环形缓冲状态 (klog 状态只有本模块的测试用, 无并发竞争)。
    fn reset() {
        unsafe {
            LOG_BUF = [0; LOG_CAP];
            LOG_HEAD = 0;
            LOG_TOTAL = 0;
        }
    }

    /// 日志环形缓冲的读语义 —— 用户态 `dmesg` 依赖它:
    ///
    /// 1. 一次读满 `LOG_CAP` 字节 = **现存窗口** (最近 `LOG_CAP` 字节), 连续无缺口;
    /// 2. 起点早于最老保留字节时**夹取**到最老处 —— 请求的偏移**不会**被照办。
    ///
    /// 第 2 条正是 `dmesg` 必须**一次读完**的原因: 分块读时起点永远落后于夹取后的
    /// 真实位置, 读到的还是同一块 (打印路径还会因「打印本身也进日志」而永不结束)。
    #[test]
    fn read_returns_retained_window_and_clamps_evicted_start() {
        // 未写满: 全部读出。
        reset();
        capture_bytes(b"hello morion");
        let mut buf = [0u8; 32];
        assert_eq!(read(0, &mut buf), 12);
        assert_eq!(&buf[..12], b"hello morion");
        // 起点越过末尾 → 0 字节 (已读到头)。
        assert_eq!(read(12, &mut buf), 0);
        assert_eq!(read(9999, &mut buf), 0);

        // 写满 LOG_CAP + 100 字节: 只保留最近 LOG_CAP 字节。
        reset();
        let total = LOG_CAP as u64 + 100;
        for i in 0..total {
            capture_bytes(&[(i % 251) as u8]);
        }
        let mut win = [0u8; LOG_CAP];
        assert_eq!(read(0, &mut win), LOG_CAP);
        for (i, &b) in win.iter().enumerate() {
            assert_eq!(b, ((total as usize - LOG_CAP + i) % 251) as u8);
        }
        // 已被挤掉的起点 → 夹取到最老处 (拿到窗口首字节, 而**不是**请求处的字节)。
        let evicted = total - LOG_CAP as u64 - 1;
        let mut small = [0u8; 16];
        assert_eq!(read(evicted, &mut small), 16);
        assert_eq!(small[0], ((total as usize - LOG_CAP) % 251) as u8);
        // 窗口内的起点: 必须拿到该处**连续**的字节。
        let mid = total - LOG_CAP as u64 + 10;
        assert_eq!(read(mid, &mut small), 16);
        assert_eq!(small[0], ((total as usize - LOG_CAP + 10) % 251) as u8);
    }
}
