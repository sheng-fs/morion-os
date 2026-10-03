//! `gfx_srv` 的渲染内核 —— 帧缓冲视角 + 点阵字库 + 文本终端 (G3a)
//!
//! 这套东西原先在内核 `video/` 里（`framebuffer.rs` / `font.rs` / `unicode.rs` / 终端）。
//! G3a 把**文本渲染**搬到用户态：字库（ASCII 8x16 + 汉字 16x16）与排版状态都归本服务，
//! 内核那边只保留 panic 用的最小 ASCII 输出。
//!
//! 帧缓冲由内核经 `Capability::Fb` 交付，故这里没有"内存管理"，只有"往映射里写像素"。

pub mod font;
pub mod glyphs;
pub mod term;

/// 一个像素矩形（屏幕 / 表面坐标；`w` 或 `h` 为 0 表示空矩形）。
///
/// 合成器用它做**屏幕边界裁剪**与**窗口边界裁剪**，终端用它累积"脏矩形"（damage）。
#[derive(Clone, Copy)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// 建一个矩形。
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    /// 交集；不相交返回空矩形（`w = h = 0`）。
    pub fn intersect(self, o: Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.x.saturating_add(self.w).min(o.x.saturating_add(o.w));
        let y1 = self.y.saturating_add(self.h).min(o.y.saturating_add(o.h));
        if x1 <= x0 || y1 <= y0 {
            Rect::new(0, 0, 0, 0)
        } else {
            Rect::new(x0, y0, x1 - x0, y1 - y0)
        }
    }

    /// 并集（外接矩形）；空矩形是并集的单位元。
    pub fn union(self, o: Rect) -> Rect {
        if self.w == 0 || self.h == 0 {
            return o;
        }
        if o.w == 0 || o.h == 0 {
            return self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = self.x.saturating_add(self.w).max(o.x.saturating_add(o.w));
        let y1 = self.y.saturating_add(self.h).max(o.y.saturating_add(o.h));
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

/// 帧缓冲视角（线性 BGRA，每像素 4 字节，`stride` 单位像素）。
///
/// 只是个**视图**（几何 + 基址），可以随意复制 —— 终端与绘图原语各持一份，指向同一块映射。
#[derive(Clone, Copy)]
pub struct Fb {
    pub base: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl Fb {
    /// 写一个像素（`color` 为 `0x00RRGGBB`；高字节固定 `0xFF` 不透明）。
    pub fn pixel(&self, x: u32, y: u32, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let off = (y as u64 * self.stride as u64 + x as u64) * 4;
        unsafe {
            *((self.base + off) as *mut u32) = 0xFF00_0000 | (color & 0x00FF_FFFF);
        }
    }

    /// 读一个像素（只取低 24 位 RGB）。
    pub fn read(&self, x: u32, y: u32) -> u32 {
        let off = (y as u64 * self.stride as u64 + x as u64) * 4;
        unsafe { *((self.base + off) as *const u32) & 0x00FF_FFFF }
    }

    /// 整屏铺色。
    pub fn fill(&self, color: u32) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.pixel(x, y, color);
            }
        }
    }

    /// 画一个纯色矩形（自动裁剪到屏幕内）。
    pub fn rect(&self, x0: u32, y0: u32, w: u32, h: u32, color: u32) {
        for y in y0..y0.saturating_add(h) {
            for x in x0..x0.saturating_add(w) {
                self.pixel(x, y, color);
            }
        }
    }
}
