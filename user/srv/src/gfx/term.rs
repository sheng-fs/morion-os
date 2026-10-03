//! 服务内文本终端：按**显示列**排版，带光标 / 自动换行 / 滚动 (G3a)
//!
//! 屏幕切成 `cols = 宽/8` × `rows = 高/16` 个字符格。一个字形的宽度由字库给出
//! （ASCII 1 格、汉字/全角 2 格），所以排版走的是显示列而不是字节数 —— 与内核终端
//! 的口径一致，不会因为多字节字符而错位。
//!
//! G5 起终端渲染进**控制台窗口的后备表面**（[`Fb`] 视图，普通内存），由合成器按 z 序上屏。
//! **每次落笔仍逐像素写后回读**（写进去的颜色再读回来必须相等）：一来"字真的画出来了"在
//! 没有显示器的环境里也能断言，二来映射一旦失效会立刻在回复值里暴露，不会静静地画到空气里。
//! 落笔同时累积**脏矩形**（[`Term::take_damage`]），合成器据此只重铺脏区。
//!
//! 控制字符：`\n` 换行、`\r` 归零列、`\t` 到下一个 8 列制表位、`\b` 退格（左移一列并擦掉
//! 该格）—— G4 的行编辑器靠 `\b` 实现退格回显。

use super::font::{CHAR_HEIGHT, CHAR_WIDTH};
use super::glyphs;
use super::{Fb, Rect};

pub struct Term {
    fb: Fb,
    cols: u32,
    rows: u32,
    col: u32,
    row: u32,
    fg: u32,
    bg: u32,
    /// 上一次 [`Term::write`] / [`Term::clear`] 触碰的**目标内**像素矩形（脏区）。
    dirty: Rect,
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
            dirty: Rect::new(0, 0, 0, 0),
        }
    }

    /// 取走上一次落笔触碰的**目标内**像素矩形（并清零）；无改动返回空矩形（`w = h = 0`）。
    ///
    /// 合成器据此只重铺**脏区**（而非整屏），避免在未缓存 MMIO 上逐像素重绘整屏。
    pub fn take_damage(&mut self) -> Rect {
        core::mem::replace(&mut self.dirty, Rect::new(0, 0, 0, 0))
    }

    /// 把一块目标内像素并入脏区。
    fn mark(&mut self, x: u32, y: u32, w: u32, h: u32) {
        self.dirty = self.dirty.union(Rect::new(x, y, w, h));
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
        self.mark(0, 0, self.fb.width, self.fb.height);
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
        self.mark(x0, y0, w * CHAR_WIDTH, CHAR_HEIGHT);
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
    /// 走的是**目标缓冲**（G5 起是控制台窗口的后备表面，普通内存）的逐像素搬运，滚动后把
    /// 整个文本区标为脏区，交由合成器重铺上屏。
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
        self.mark(0, 0, self.fb.width, h);
    }
}
