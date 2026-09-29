//! UTF-8 解码 + 点阵字形查表（`cjk.bin`）—— 汉字等非 ASCII 字符的渲染 (G3a 从内核搬来)
//!
//! 数据由 [`scripts/gen-cjk-font.py`](../../../../scripts/gen-cjk-font.py) 从 GNU Unifont
//! (OFL-1.1) 生成：GB2312 全集 ∪ 仓库里出现过的非 ASCII 字符，共约 7500 字，
//! 定长 37 字节记录、按码点升序，故此处二分查找即可。**宽度随字形一起存**
//! （1 = 8x16 窄字形占 1 格，2 = 16x16 宽字形占 2 格），所以排版列数不必再维护
//! 一张 East Asian Width 表 —— 字库里有就信字库，没有（豆腐块）才按东亚宽度粗判。
//!
//! 本模块只回答"某个字的第 (row, col) 位是不是亮的"，**不碰帧缓冲** —— 落笔与回读
//! 校验都在 [`super::term`] 里做，两边各管一段。

use super::font;

/// 字库二进制（`scripts/gen-cjk-font.py` 生成，随仓库提交；构建不依赖网络/Python）。
static GLYPHS: &[u8] = include_bytes!("cjk.bin");

/// 记录长度（字节）；码点 4 + 宽度 1 + 16 行 × u16 = 37。
const RECORD: usize = 37;

/// 字形：宽度（字符格数）与 16 行位图（每行 u16 大端，bit15 为最左像素）。
pub struct Glyph {
    pub width: u32,
    pub rows: &'static [u8],
}

/// 按码点查字形（二分查找）。
pub fn glyph(cp: u32) -> Option<Glyph> {
    let count = GLYPHS.len() / RECORD;
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let off = mid * RECORD;
        let key = u32::from_be_bytes([
            GLYPHS[off],
            GLYPHS[off + 1],
            GLYPHS[off + 2],
            GLYPHS[off + 3],
        ]);
        match key.cmp(&cp) {
            core::cmp::Ordering::Equal => {
                return Some(Glyph {
                    width: GLYPHS[off + 4] as u32,
                    rows: &GLYPHS[off + 5..off + RECORD],
                })
            }
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
        }
    }
    None
}

/// 解码 UTF-8 的下一个字符：返回 (码点, 该字符占的字节数)。
///
/// 非法序列按 U+FFFD 处理并只前进 1 字节 —— 打印路径可能拿到截断/损坏的字节
/// （文本经共享页从客户端传进来，长度由客户端给），不能让一个坏字节把整行卡住。
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

/// 字符占的显示列数。
///
/// 字库里有字形就信字库（GB2312 里的 `·` 是窄的、全角 `，` 是宽的，各自都对）；
/// 没有字形（会画成豆腐块）时按东亚宽度粗判宽窄，让占位不至于错位。
pub fn width(cp: u32) -> u32 {
    if (0x20..=0x7E).contains(&cp) {
        return 1;
    }
    if cp < 0x20 || cp == 0x7F {
        return 0; // 控制字符: 不可见, 由终端按语义处理（如 '\n'）或忽略
    }
    match glyph(cp) {
        Some(g) => g.width,
        None => {
            if east_asian_wide(cp) {
                2
            } else {
                1
            }
        }
    }
}

/// 该码点在第 `row` 行、第 `col` 列（0 起，单位像素，列范围 `0..width*8`）是否为亮点。
///
/// 越界一律返回 `false`（终端按整格遍历，宽字形右侧留白自然落到这里）。
pub fn bit(cp: u32, row: u32, col: u32) -> bool {
    if row >= font::CHAR_HEIGHT || col >= width(cp).max(1) * font::CHAR_WIDTH {
        return false;
    }
    if (0x20..=0x7E).contains(&cp) {
        let g = &font::FONT[(cp - 0x20) as usize];
        return g[row as usize] & (0x80u8 >> col) != 0;
    }
    match glyph(cp) {
        Some(g) => {
            let r = row as usize * 2;
            let bits = ((g.rows[r] as u16) << 8) | g.rows[r + 1] as u16;
            bits & (0x8000u16 >> col) != 0
        }
        // 豆腐块: 空心方框, 明确表示「有这个字符, 但字库缺字形」。
        None => {
            let tw = width(cp).max(1) * font::CHAR_WIDTH;
            let th = font::CHAR_HEIGHT;
            let horiz = (row == 1 || row == th - 2) && col >= 1 && col + 2 <= tw;
            let vert = (col == 1 || col + 2 == tw) && row >= 1 && row + 2 <= th;
            horiz || vert
        }
    }
}

/// 东亚宽度粗判（仅用于「字库里没有字形」的豆腐块占位；有字形时不查这里）。
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
