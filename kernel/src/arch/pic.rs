//! PIC 8259A 中断控制器
//!
//! 将 master 重映射到向量 32-39, slave 重映射到 40-47,
//! 并屏蔽除时钟 (IRQ0) 与键盘 (IRQ1) 外的所有硬件中断。

use x86_64::instructions::port::Port;

const MASTER_CMD: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_CMD: u16 = 0xA0;
const SLAVE_DATA: u16 = 0xA1;

const CMD_INIT: u8 = 0x11; // ICW1: 需要 ICW4
const CMD_EOI: u8 = 0x20;  // 结束中断命令
const MODE_8086: u8 = 0x01; // ICW4: 8086 模式

/// 重映射 PIC 并屏蔽除 IRQ0/IRQ1 外的所有中断
pub fn init() {
    unsafe {
        let mut master_cmd: Port<u8> = Port::new(MASTER_CMD);
        let mut master_data: Port<u8> = Port::new(MASTER_DATA);
        let mut slave_cmd: Port<u8> = Port::new(SLAVE_CMD);
        let mut slave_data: Port<u8> = Port::new(SLAVE_DATA);

        // ICW1
        master_cmd.write(CMD_INIT);
        slave_cmd.write(CMD_INIT);

        // ICW2: 中断向量偏移
        master_data.write(32); // master → 32..=39
        slave_data.write(40);  // slave  → 40..=47

        // ICW3: 级联关系
        master_data.write(4); // slave 接在 master 的 IRQ2
        slave_data.write(2);  // slave 的级联标识

        // ICW4: 8086 模式
        master_data.write(MODE_8086);
        slave_data.write(MODE_8086);

        // 屏蔽所有中断, 仅保留 IRQ0 (时钟) 与 IRQ1 (键盘)
        master_data.write(0xFC); // 1111_1100
        slave_data.write(0xFF);  // 1111_1111
    }
}

/// 向 master PIC 发送 EOI (End of Interrupt)
pub fn send_eoi() {
    unsafe {
        let mut master_cmd: Port<u8> = Port::new(MASTER_CMD);
        master_cmd.write(CMD_EOI);
    }
}
