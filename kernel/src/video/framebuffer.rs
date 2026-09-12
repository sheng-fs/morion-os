//! 线性帧缓冲 (GOP) — 直接写入 BGRA 像素
//!
//! 颜色约定与引导器/kernel_test 保持一致: color = 0x00RRGGBB。

pub struct Framebuffer {
    base: *mut u8,
    width: u32,
    height: u32,
    stride: u32, // 行跨度, 单位像素
}

impl Framebuffer {
    /// 未初始化的空缓冲 (用于 static 初始值)
    pub const fn empty() -> Self {
        Self { base: core::ptr::null_mut::<u8>(), width: 0, height: 0, stride: 0 }
    }

    /// 是否已初始化
    pub fn is_ready(&self) -> bool {
        !self.base.is_null() && self.width > 0 && self.height > 0
    }

    pub fn init(base: u64, width: u32, height: u32, stride: u32) -> Self {
        Self { base: base as *mut u8, width, height, stride }
    }

    /// 写入单个像素 (0x00RRGGBB)。
    ///
    /// 按 32 位写入: 行跨度以像素为单位且帧缓冲基址 4 字节对齐, 故 `(y*stride+x)*4`
    /// 天然 4 字节对齐。相比逐字节写 + 每字节边界检查, 相机码流减少约 4 倍,
    /// 对未缓存的 MMIO 帧缓冲提升明显。字节序 (BGRA) 即小端 u32 = `0xFF<<24|RGB`。
    pub fn pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < self.width && y < self.height {
            let off = y as usize * self.stride as usize + x as usize;
            unsafe {
                *(self.base as *mut u32).add(off) = 0xFF00_0000 | (color & 0x00FF_FFFF);
            }
        }
    }

    /// 填充矩形 (逐行一次性写满, 按 32 位写)。
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        if x >= self.width || y >= self.height || w == 0 || h == 0 {
            return;
        }
        let w = w.min(self.width - x) as usize;
        let h = h.min(self.height - y);
        let val = 0xFF00_0000 | (color & 0x00FF_FFFF);
        let stride = self.stride as usize;
        unsafe {
            let base = self.base as *mut u32;
            for dy in 0..h as usize {
                let start = (y as usize + dy) * stride + x as usize;
                let row = base.add(start);
                for dx in 0..w {
                    *row.add(dx) = val;
                }
            }
        }
    }

    pub fn clear(&mut self, color: u32) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// 向上滚动: 把 [top + line_height, height) 上移到 [top, height - line_height),
    /// 并清空底部 [height - line_height, height) 为背景色。
    pub fn scroll_up(&mut self, top: u32, line_height: u32, background: u32) {
        if self.height <= top + line_height {
            return;
        }
        let bpp = 4;
        let stride = self.stride as usize;
        let top = top as usize;
        let line = line_height as usize;
        let height = self.height as usize;

        unsafe {
            let src = self.base.add((top + line) * stride * bpp);
            let dst = self.base.add(top * stride * bpp);
            core::ptr::copy(src, dst, (height - top - line) * stride * bpp);
        }

        for y in (self.height - line_height)..self.height {
            for x in 0..self.width {
                self.pixel(x, y, background);
            }
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}