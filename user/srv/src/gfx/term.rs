//! 服务内文本终端：按**显示列**排版，带光标 / 自动换行 / 滚动 (G3a)
//!
//! 屏幕切成 `cols = 宽/8` × `rows = 高/16` 个字符格。一个字形的宽度由字库给出
//! （ASCII 1 格、汉字/全角 2 格），所以排版走的是显示列而不是字节数 —— 与内核终端
//! 的口径一致，不会因为多字节字符而错位。
//!
//! **每次落笔都逐像素写后回读**（写进去的颜色再读回来必须相等）：一来"字真的画到帧缓冲上
//! 了"在没有显示器的环境里也能断言，二来帧缓冲映射一旦失效会立刻在回复值里暴露，不会静静
//! 地画到空气里。
//!
//! 控制字符：`\n` 换行、`\r` 归零列、`\t` 到下一个 8 列制表位、`\b` 退格（左移一列并擦掉
//! 该格）—— G4 的行编辑器靠 `\b` 实现退格回显。

use super::font::{CHAR_HEIGHT, CHAR_WIDTH};
use super::glyphs;
use super::Fb;

pub struct Term {
    fb: Fb,
    cols: u32,
    rows: u32,
    col: u32,
    row: u32,
    fg: u32,
    bg: u32,
}

impl Term {
    /// 以帧缓冲几何建一个终端（光标归零，但**不清屏** —— 清屏要显式 [`Term::clear`]）。
    pub fn new(fb: Fb, fg: u32, bg: u32) -> Term {
        let cols = fb.width / CHAR_WIDTH;
        let rows = fb.height / CHAR_HEIGHT;
        Term {
            fb,
            cols,
            rows,
            col: 0,
            row: 0,
            fg,
            bg,
        }
    }

    /// 光标位置 `(行, 列)`。
    pub fn cursor(&self) -> (u32, u32) {
        (self.row, self.col)
    }

    /// 定位光标；越界返回 `false` 且不动（调用方多半是自测，越界就该报错而不是被悄悄夹住）。
    pub fn move_to(&mut self, col: u32, row: u32) -> bool {
        if col >= self.cols || row >= self.rows {
            return false;
        }
        self.col = col;
        self.row = row;
        true
    }

    /// 清屏（铺背景色）并把光标归零。
    pub fn clear(&mut self) {
        self.fb.fill(self.bg);
        self.col = 0;
        self.row = 0;
    }

    /// 按 UTF-8 写一段文本；任何一次逐像素回读不一致都返回 `false`。
    pub fn write(&mut self, bytes: &[u8]) -> bool {
        let mut i = 0;
        while i < bytes.len() {
            let (cp, n) = glyphs::decode(bytes, i);
            i += n;
            match cp {
                0x0A => self.newline(), // '\n'
                0x0D => self.col = 0,   // '\r'
                0x08 => {
                    // '\b' 退格: 光标左移一列, 并把那一格涂成背景色 (即擦掉一个字符格)。
                    // 宽字形 (汉字) 占 2 格, 调用方按列数重复发即可 —— G4 的行编辑器
                    // (`morion::console`) 的输入目前全是 ASCII, 一次一键即一列。
                    if self.col > 0 {
                        self.col -= 1;
                        if !self.write_cp(0x20) {
                            return false;
                        }
                    }
                }
                0x09 => {
                    // '\t' → 下一个 8 列制表位
                    let next = (self.col / 8 + 1) * 8;
                    while self.col < next {
                        if !self.write_cp(0x20) {
                            return false;
                        }
                    }
                }
                _ if cp < 0x20 || cp == 0x7F => {} // 其它控制字符忽略
                _ => {
                    if !self.write_cp(cp) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// 落一个字符：整格逐像素算出应显示的颜色，写入并立即回读校验。
    fn write_cp(&mut self, cp: u32) -> bool {
        let w = glyphs::width(cp).max(1);
        if self.col + w > self.cols {
            self.newline();
        }
        let x0 = self.col * CHAR_WIDTH;
        let y0 = self.row * CHAR_HEIGHT;
        for row in 0..CHAR_HEIGHT {
            for col in 0..w * CHAR_WIDTH {
                let want = if glyphs::bit(cp, row, col) {
                    self.fg
                } else {
                    self.bg
                };
                self.fb.pixel(x0 + col, y0 + row, want);
                if self.fb.read(x0 + col, y0 + row) != want {
                    return false;
                }
            }
        }
        self.col += w;
        true
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row += 1;
        if self.row >= self.rows {
            self.scroll();
            self.row = self.rows - 1;
        }
    }

    /// 整屏上滚一行（把第 1 行起的内容往上挪一行，最后一行刷背景）。
    ///
    /// 注意这是**逐像素**搬运，帧缓冲是非缓存映射，所以一次滚动要动上百万像素 ——
    /// 只在写到最后一行时才发生。
    fn scroll(&mut self) {
        let h = self.rows * CHAR_HEIGHT;
        for y in 0..h.saturating_sub(CHAR_HEIGHT) {
            for x in 0..self.fb.width {
                let c = self.fb.read(x, y + CHAR_HEIGHT);
                self.fb.pixel(x, y, c);
            }
        }
        let top = h.saturating_sub(CHAR_HEIGHT);
        for y in top..h {
            for x in 0..self.fb.width {
                self.fb.pixel(x, y, self.bg);
            }
        }
    }
}
