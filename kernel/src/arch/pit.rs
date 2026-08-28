//! PIT 8253 可编程定时器 — 周期性时钟中断 (IRQ0)

use x86_64::instructions::port::Port;

const PIT_CHANNEL_0: u16 = 0x40;
const PIT_CMD: u16 = 0x43;

/// PIT 基础频率 (Hz)
const PIT_BASE_FREQ: u32 = 1_193_182;

/// 目标中断频率 (Hz)
const TARGET_FREQ: u32 = 100;

/// 初始化 PIT 为周期方波模式, 频率约 100 Hz
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
