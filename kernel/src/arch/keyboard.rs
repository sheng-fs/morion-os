//! PS/2 键盘 — 读取 scancode (IRQ1)

use x86_64::instructions::port::Port;

const KEYBOARD_DATA: u16 = 0x60;

/// 读取键盘数据端口的一个字节 (scancode)
pub fn read_scancode() -> u8 {
    unsafe {
        let mut data: Port<u8> = Port::new(KEYBOARD_DATA);
        data.read()
    }
}
