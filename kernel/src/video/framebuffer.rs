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
        Self { base: 0 as *mut u8, width: 0, height: 0, stride: 0 }
    }

    /// 是否已初始化
    pub fn is_ready(&self) -> bool {
        !self.base.is_null() && self.width > 0 && self.height > 0
    }

    pub fn init(base: u64, width: u32, height: u32, stride: u32) -> Self {
        Self { base: base as *mut u8, width, height, stride }
    }

    /// 写入单个像素 (0x00RRGGBB)
    pub fn pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < self.width && y < self.height {
            let off = (y * self.stride + x) as usize * 4;
            unsafe {
                *self.base.add(off) = (color & 0xFF) as u8;
                *self.base.add(off + 1) = ((color >> 8) & 0xFF) as u8;
                *self.base.add(off + 2) = ((color >> 16) & 0xFF) as u8;
                *self.base.add(off + 3) = 0xFF;
            }
        }
    }

    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        for dy in 0..h {
            for dx in 0..w {
                self.pixel(x + dx, y + dy, color);
            }
        }
    }

    pub fn clear(&mut self, color: u32) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}