//! PS/2 键盘 (i8042 控制器) — 初始化 + 读取 scancode (IRQ1)
//!
//! 引导固件 (OVMF) 退出 Boot Services 后, 键盘控制器可能处于「键盘中断被
//! 屏蔽 / 键盘被禁用」的状态, 导致 IRQ1 永不触发。因此内核需自行初始化
//! i8042: 使能键盘中断、使能键盘、并开启 scancode set2→set1 翻译, 使
//! 读到的 scancode 与用户态驱动解码表 (set 1) 一致。

use x86_64::instructions::port::{Port, PortReadOnly, PortWriteOnly};

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64; // 读: 状态寄存器
const COMMAND: u16 = 0x64; // 写: 命令寄存器

/// 状态寄存器位: bit0 输出缓冲满, bit1 输入缓冲满。
const STAT_OUTPUT_FULL: u8 = 0x01;
const STAT_INPUT_FULL: u8 = 0x02;

/// 等待输入缓冲为空 (可写入命令/数据)。
unsafe fn wait_input_empty(status: &mut PortReadOnly<u8>) {
    while status.read() & STAT_INPUT_FULL != 0 {
        core::hint::spin_loop();
    }
}

/// 等待输出缓冲非空 (有数据可读)。
unsafe fn wait_output_full(status: &mut PortReadOnly<u8>) {
    while status.read() & STAT_OUTPUT_FULL == 0 {
        core::hint::spin_loop();
    }
}

/// 初始化 i8042 控制器, 确保键盘中断使能、键盘启用、scancode 翻译为 set 1。
pub fn init() {
    unsafe {
        let mut status: PortReadOnly<u8> = PortReadOnly::new(STATUS);
        let mut command: PortWriteOnly<u8> = PortWriteOnly::new(COMMAND);
        let mut data: Port<u8> = Port::new(DATA);

        // 1. 禁用键盘, 避免初始化期间产生 IRQ。
        wait_input_empty(&mut status);
        command.write(0xAD);

        // 2. 清空输出缓冲, 丢弃固件遗留数据。
        while status.read() & STAT_OUTPUT_FULL != 0 {
            let _ = data.read();
        }

        // 3. 读取命令字节 (0x20)。
        wait_input_empty(&mut status);
        command.write(0x20);
        wait_output_full(&mut status);
        let mut cfg = data.read();

        // 4. bit0=键盘中断使能, bit4=键盘启用(清0), bit6=set2→set1 翻译。
        cfg |= 0x01;
        cfg &= !0x10;
        cfg |= 0x40;

        // 5. 写回命令字节 (0x60)。
        wait_input_empty(&mut status);
        command.write(0x60);
        wait_input_empty(&mut status);
        data.write(cfg);

        // 6. 重新使能键盘 (0xAE)。
        wait_input_empty(&mut status);
        command.write(0xAE);
    }
}

/// 读取键盘数据端口的一个字节 (scancode)
pub fn read_scancode() -> u8 {
    unsafe {
        let mut data: Port<u8> = Port::new(DATA);
        data.read()
    }
}
