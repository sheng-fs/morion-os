use morion::syscall::*;

/// scancode set 1 基础键 → (无 shift, 有 shift) 字节；`0` 表示非字符键。
/// 索引即 scancode (0..0x60)。修饰键 (shift/ctrl/alt/caps) 与扩展键不在此列。
const KEYMAP: [(u8, u8); 0x60] = {
    let mut m = [(0u8, 0u8); 0x60];
    m[0x02] = (b'1', b'!');
    m[0x03] = (b'2', b'@');
    m[0x04] = (b'3', b'#');
    m[0x05] = (b'4', b'$');
    m[0x06] = (b'5', b'%');
    m[0x07] = (b'6', b'^');
    m[0x08] = (b'7', b'&');
    m[0x09] = (b'8', b'*');
    m[0x0A] = (b'9', b'(');
    m[0x0B] = (b'0', b')');
    m[0x0C] = (b'-', b'_');
    m[0x0D] = (b'=', b'+');
    m[0x0F] = (b'\t', b'\t');
    m[0x10] = (b'q', b'Q');
    m[0x11] = (b'w', b'W');
    m[0x12] = (b'e', b'E');
    m[0x13] = (b'r', b'R');
    m[0x14] = (b't', b'T');
    m[0x15] = (b'y', b'Y');
    m[0x16] = (b'u', b'U');
    m[0x17] = (b'i', b'I');
    m[0x18] = (b'o', b'O');
    m[0x19] = (b'p', b'P');
    m[0x1A] = (b'[', b'{');
    m[0x1B] = (b']', b'}');
    m[0x1E] = (b'a', b'A');
    m[0x1F] = (b's', b'S');
    m[0x20] = (b'd', b'D');
    m[0x21] = (b'f', b'F');
    m[0x22] = (b'g', b'G');
    m[0x23] = (b'h', b'H');
    m[0x24] = (b'j', b'J');
    m[0x25] = (b'k', b'K');
    m[0x26] = (b'l', b'L');
    m[0x27] = (b';', b':');
    m[0x28] = (b'\'', b'"');
    m[0x29] = (b'`', b'~');
    m[0x2B] = (b'\\', b'|');
    m[0x2C] = (b'z', b'Z');
    m[0x2D] = (b'x', b'X');
    m[0x2E] = (b'c', b'C');
    m[0x2F] = (b'v', b'V');
    m[0x30] = (b'b', b'B');
    m[0x31] = (b'n', b'N');
    m[0x32] = (b'm', b'M');
    m[0x33] = (b',', b'<');
    m[0x34] = (b'.', b'>');
    m[0x35] = (b'/', b'?');
    m[0x39] = (b' ', b' ');
    m
};

/// 查询 scancode 对应的字符字节 (按 shift 状态)；非字符键返回 `None`。
fn key_char(sc: u8, shift: bool) -> Option<u8> {
    let i = sc as usize;
    if i >= KEYMAP.len() {
        return None;
    }
    let (base, shifted) = KEYMAP[i];
    let c = if shift { shifted } else { base };
    if c == 0 {
        None
    } else {
        Some(c)
    }
}

/// 域 4 — 用户态键盘驱动: 注册接收 IRQ1, 循环接收 scancode, 解码成字节推进内核键队列。
///
/// **G4 起内核不解释按键**: 这里只做 scancode → 字节的翻译 —— 可打印字符按 shift 状态取
/// 对应字符, 退格发 `0x08`, (小/数字键盘的) 回车发 `'\n'`; 方向键等非字符键直接丢弃
/// (行编辑 / 行历史在 `gfx_srv` 的屏幕控制台)。
pub fn run() {
    if sys_register_irq(1) != 1 {
        println("kbd: register irq1 FAILED");
        return;
    }

    let mut ext = false;
    let mut shift = false;
    loop {
        let sc = sys_recv() as u8;

        // E0 扩展前缀: 标记后续字节为扩展键码。
        if sc == 0xE0 {
            ext = true;
            continue;
        }
        // 释放码 (bit7 置位): 只处理按下码; shift 释放时清除状态。
        if sc & 0x80 != 0 {
            let base = sc & 0x7F;
            if base == 0x2A || base == 0x36 {
                shift = false;
            }
            ext = false;
            continue;
        }

        // 扩展按下码: 只认数字键盘回车, 其余 (方向键等) 丢弃。
        if ext {
            ext = false;
            if sc == 0x1C {
                sys_key_push(b'\n'); // 数字键盘 Enter
            }
            continue;
        }

        // 普通按下码。
        match sc {
            0x2A | 0x36 => shift = true, // 左右 shift 按下
            0x0E => {
                sys_key_push(0x08); // 退格
            }
            0x1C => {
                sys_key_push(b'\n'); // 回车
            }
            _ => {
                if let Some(c) = key_char(sc, shift) {
                    sys_key_push(c);
                }
            }
        }
    }
}
