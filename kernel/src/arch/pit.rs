//! PIT 8253 可编程定时器 — 周期性时钟中断 (IRQ0)

use x86_64::instructions::port::Port;

const PIT_CHANNEL_0: u16 = 0x40;
const PIT_CMD: u16 = 0x43;

/// PIT 基础频率 (Hz)
const PIT_BASE_FREQ: u32 = 1_193_182;

/// 目标中断频率 (Hz)。
///
/// 02b-2 优化结论: 调度/IPC 唤醒都由时钟 tick 量化, 100 Hz 时一次「阻塞→唤醒」最坏
/// 要等 10 ms; 全量回归里有上万次这种等待, 单这一项就是耗时大头。频率扫描 (同代码,
/// fs-regress 多次):
///   - 100 Hz  → 118 s (基线, 稳定)
///   - 500 Hz  → 33 s
///   - 1000 Hz → **23 s**  ← 采用 (每次等待由 10 ms 降到 1 ms)
///
/// 1000 Hz 曾以 ~18% 概率触发一条**启动期**竞态: 能力负例测试 `receiver: call echo
/// WITHOUT capability unexpectedly OK`。根因不是能力强制出错, 而是 sender (域 0) 比
/// receiver (域 1) 先起跑, 「sender 委派 `SendTo(3)`」与「receiver 启动负例」只靠时间片
/// 排序 —— tick 越密, sender 缺页阻塞→被 pager 唤醒的往返越短, 越容易在 receiver 跑到
/// 负例前完成委派 (实测失败时 `current_domain()==1` 正确、`cap::has(1, SendTo(3))==true`,
/// 即能力是**合法委派**来的)。现用一次**显式启动握手**定序 (见
/// `user/srv/src/{sender,receiver}.rs` 与 `common::RECV_HANDSHAKE_*`): sender 先发请求并
/// 阻塞, receiver 跑完负例才回 ack —— 顺序改由同步而非时间片决定, 1000 Hz 下连续多次全绿。
pub const TARGET_FREQ: u32 = 1000;

/// 初始化 PIT 为周期方波模式, 频率 [`TARGET_FREQ`] Hz
pub fn init() {
    let divider = (PIT_BASE_FREQ / TARGET_FREQ) as u16; // ≈ 11931

    unsafe {
        let mut cmd: Port<u8> = Port::new(PIT_CMD);
        let mut data: Port<u8> = Port::new(PIT_CHANNEL_0);

        // 0b0011_0110: channel 0, 先低后高字节, mode 3 (方波), 二进制
        cmd.write(0x36);
        data.write((divider & 0xFF) as u8);
        data.write((divider >> 8) as u8);
    }
}
