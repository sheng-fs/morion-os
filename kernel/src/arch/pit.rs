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
///   - 500 Hz  → **33 s**, 16/16 次全绿  ← 采用
///   - 1000 Hz → 23 s, 但 ~18% 概率触发内核潜藏竞态 (能力负例测试失败) → 不用
///
/// 500 Hz 在「提速 3.6×」与「不触发竞态」之间取平衡 (每次等待由 10 ms 降到 2 ms)。
pub const TARGET_FREQ: u32 = 500;

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
