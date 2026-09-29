//! UTF-8 解码 + 最小 ASCII 文本绘制 (G3c)
//!
//! 汉字/全角点阵 (`cjk.bin`, ≈276 KB) 与东亚宽度排版**已搬到用户态的 `gfx_srv`**
//! (见 `user/srv/src/gfx/`)：屏幕上的汉字现在由用户态画，内核不再携带字库 ——
//! 内核镜像因此瘦掉约 276 KB。`scripts/gen-cjk-font.py` 的产物也只进用户态那份。
//!
//! 内核这边只保留「gfx 接管前那几秒 + panic 屏」够用的最小能力：
//! - ASCII (`0x20..=0x7E`) 走 [`super::font`] 的 8x16 字模；
//! - 非 ASCII 一律画**空心豆腐块**，宽度按东亚宽度粗判（汉字类 2 格），
//!   于是列表 / 排版列数仍然对得上，只是看不出是哪几个字；
//! - UTF-8 边界推进 (`decode` / `prev_index` / `next_index`) 保持原样 ——
//!   退格与光标左右移因此不会把多字节字符切成半个。
//!
//! **COM1 串口输出全程不受影响**：`print` 直接把 UTF-8 字节写串口，不经过本模块。

use super::font;
use super::framebuffer::Framebuffer;

/// 解码 UTF-8 的下一个字符：返回 (码点, 该字符占的字节数)。
///
/// 非法序列按 U+FFFD 处理并只前进 1 字节 —— 打印路径可能拿到截断/损坏的字节
/// （`SYS_PUTS` 只信任指针 + 长度），不能让一个坏字节把整行卡住。
pub fn decode(bytes: &[u8], i: usize) -> (u32, usize) {
    let b0 = bytes[i];
    let (need, mut cp) = if b0 < 0x80 {
        return (b0 as u32, 1);
    } else if b0 & 0xE0 == 0xC0 {
        (1usize, (b0 & 0x1F) as u32)
    } else if b0 & 0xF0 == 0xE0 {
        (2, (b0 & 0x0F) as u32)
    } else if b0 & 0xF8 == 0xF0 {
        (3, (b0 & 0x07) as u32)
    } else {
        return (0xFFFD, 1);
    };
    if i + need >= bytes.len() {
        // 后续字节不足（截断的序列）
        return (0xFFFD, 1);
    }
    for k in 1..=need {
        let b = bytes[i + k];
        if b & 0xC0 != 0x80 {
            return (0xFFFD, 1);
        }
        cp = (cp << 6) | (b & 0x3F) as u32;
    }
    // 过长编码 / 代理区 / 超范围一律判非法
    let min = match need {
        1 => 0x80,
        2 => 0x800,
        _ => 0x1_0000,
    };
    if cp < min || (0xD800..=0xDFFF).contains(&cp) || cp > 0x10_FFFF {
        return (0xFFFD, 1);
    }
    (cp, need + 1)
}

/// 该字符的**下一个字符起始下标**（`bytes` 必须确实是 UTF-8）。
pub fn next_index(bytes: &[u8], i: usize) -> usize {
    let (_cp, n) = decode(bytes, i);
    i + n
}

/// 该位置**前一个字符**的起始下标（退格 / 光标左移用，保证不切进多字节字符中间）。
pub fn prev_index(bytes: &[u8], i: usize) -> usize {
    let mut j = i.saturating_sub(1);
    // 回退到「不是续字节 (10xxxxxx)」的那个字节 —— 即上一个字符的首字节。
    while j > 0 && bytes[j] & 0xC0 == 0x80 {
        j -= 1;
    }
    j
}

/// 字符占的显示列数。
///
/// 内核已无字库，非 ASCII 只能按**东亚宽度**粗判宽窄（汉字类 2 格、其余 1 格）——
/// 与用户态 `gfx_srv` 的口径一致，接管前后列数不会突变。
pub fn width(cp: u32) -> u32 {
    if (0x20..=0x7E).contains(&cp) {
        return 1;
    }
    if cp < 0x20 || cp == 0x7F {
        return 0; // 控制字符: 不可见, 由终端按语义处理（如 '\n'）或忽略
    }
    if east_asian_wide(cp) {
        2
    } else {
        1
    }
}

/// 一段 UTF-8 字节的显示列数。
pub fn str_width(bytes: &[u8]) -> u32 {
    let mut w = 0;
    let mut i = 0;
    while i < bytes.len() {
        let (cp, n) = decode(bytes, i);
        w += width(cp);
        i += n;
    }
    w
}

/// 画一个码点：ASCII 走 8x16 字模，非 ASCII 画**空心豆腐块**（内核已不带字库）。
pub fn draw(fb: &mut Framebuffer, x: u32, y: u32, cp: u32, color: u32) {
    if (0x20..=0x7E).contains(&cp) {
        font::draw_char(fb, x, y, cp as u8, color);
        return;
    }
    let w = width(cp).max(1) * font::CHAR_WIDTH;
    let h = font::CHAR_HEIGHT;
    for col in 1..w.saturating_sub(1) {
        fb.pixel(x + col, y + 1, color);
        fb.pixel(x + col, y + h - 2, color);
    }
    for row in 1..h.saturating_sub(1) {
        fb.pixel(x + 1, y + row, color);
        fb.pixel(x + w - 2, y + row, color);
    }
}

/// 东亚宽度粗判（只影响非 ASCII 占几格；字库不在内核里了）。
fn east_asian_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 宽度口径: ASCII 1 格、控制字符 0 格、汉字/全角标点 2 格、窄符号 1 格。
    /// 这些值与用户态 `gfx_srv` 的排版列数一致 —— 接管前后不能突变。
    #[test]
    fn width_matches_user_space_columns() {
        assert_eq!(width(b'A' as u32), 1);
        assert_eq!(width(b'\n' as u32), 0);
        assert_eq!(width('中' as u32), 2);
        assert_eq!(width('，' as u32), 2); // 全角标点 U+FF0C
        assert_eq!(width('·' as u32), 1); // 窄间隔点 U+00B7
    }

    /// 三字节 UTF-8 解码 + 混排列数: "A中B" 共 1 + 2 + 1 = 4 列。
    #[test]
    fn str_width_counts_display_columns() {
        let s = "A中B".as_bytes();
        let (cp, n) = decode(s, 1);
        assert_eq!((cp, n), ('中' as u32, 3));
        assert_eq!(str_width(s), 4);
    }

    /// 退格 / 左移按字符边界回退, 不会切进多字节字符中间。
    #[test]
    fn prev_index_stops_at_char_boundary() {
        let s = "A中".as_bytes();
        assert_eq!(prev_index(s, s.len()), 1);
        assert_eq!(next_index(s, 1), s.len());
    }

    /// 截断的多字节序列按 U+FFFD 处理且只前进 1 字节 (不能让坏字节卡住整行)。
    #[test]
    fn truncated_sequence_advances_one_byte() {
        assert_eq!(decode(&[0xE4, 0xB8], 0), (0xFFFD, 1));
    }
}
