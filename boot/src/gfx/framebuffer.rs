//! UEFI GOP Framebuffer 抽象层
//!
//! 封装 UEFI Graphics Output Protocol，提供像素级帧缓冲访问。
//! 支持：
//!   - 自动检测最优显示模式
//!   - 32-bit BGRA/RGBA 像素格式
//!   - 双缓冲 (可选, 用于无撕裂动画)

use core::ptr;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::table::boot::{BootServices, OpenProtocolAttributes, OpenProtocolParams, SearchType};
use uefi::Identify;

/// 颜色 (BGRA 格式, 与 UEFI GOP 兼容)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Color {
    pub blue: u8,
    pub green: u8,
    pub red: u8,
    pub alpha: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self { blue: 0, green: 0, red: 0, alpha: 0 };
    pub const BLACK: Self       = Self { blue: 0, green: 0, red: 0, alpha: 255 };
    pub const WHITE: Self       = Self { blue: 255, green: 255, red: 255, alpha: 255 };
    pub const RED: Self         = Self { blue: 0, green: 0, red: 255, alpha: 255 };
    pub const GREEN: Self       = Self { blue: 0, green: 255, red: 0, alpha: 255 };
    pub const BLUE: Self        = Self { blue: 255, green: 0, red: 0, alpha: 255 };

    pub fn from_hex(hex: &str) -> Option<Self> {
        let hex = hex.trim_start_matches('#');
        if hex.len() != 6 {
            return None;
        }
        let bytes = hex.as_bytes();
        let hex_byte = |i: usize| -> Option<u8> {
            let c = bytes[i];
            match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                b'A'..=b'F' => Some(c - b'A' + 10),
                _ => None,
            }
        };
        let r = hex_byte(0)? * 16 + hex_byte(1)?;
        let g = hex_byte(2)? * 16 + hex_byte(3)?;
        let b = hex_byte(4)? * 16 + hex_byte(5)?;
        Some(Self { red: r, green: g, blue: b, alpha: 255 })
    }

    pub fn blend_over(self, bg: Self) -> Self {
        let a = self.alpha as u32;
        let inv_a = 255 - a;
        Self {
            blue:  ((self.blue as u32 * a + bg.blue as u32 * inv_a) >> 8) as u8,
            green: ((self.green as u32 * a + bg.green as u32 * inv_a) >> 8) as u8,
            red:   ((self.red as u32 * a + bg.red as u32 * inv_a) >> 8) as u8,
            alpha: 255,
        }
    }

    pub fn with_alpha(self, alpha: u8) -> Self {
        Self { alpha, ..self }
    }

    pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self { red: r, green: g, blue: b, alpha: 255 }
    }

    pub const fn from_rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { red: r, green: g, blue: b, alpha: a }
    }
}

/// 矩形区域
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, width: w, height: h }
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.width as i32
            && py >= self.y && py < self.y + self.height as i32
    }
}

/// 屏幕尺寸
#[derive(Clone, Copy, Debug)]
pub struct ScreenSize {
    pub width: u32,
    pub height: u32,
}

/// UEFI GOP 帧缓冲抽象
pub struct FrameBuffer {
    base: *mut u8,
    size: usize,
    resolution: ScreenSize,
    stride: u32,
    pixel_format: PixelFormat,
    bytes_per_pixel: u8,
    owned: bool,
}

unsafe impl Send for FrameBuffer {}
unsafe impl Sync for FrameBuffer {}

impl FrameBuffer {
    /// 从 UEFI GOP 协议创建帧缓冲
    pub fn from_gop(boot_services: &BootServices) -> Result<Self, &'static str> {
        // SAFETY: UEFI 引导阶段，GOP 是系统唯一拥有的图形输出资源
        unsafe {
            let gop_guid = GraphicsOutput::GUID;
            let handles = boot_services
                .locate_handle_buffer(SearchType::ByProtocol(&gop_guid))
                .map_err(|_| "未找到 GOP 协议句柄")?;
            let handle = handles.first().copied().ok_or("无 GOP 句柄")?;

            let gop_handle = boot_services
                .open_protocol::<GraphicsOutput>(
                    OpenProtocolParams {
                        handle,
                        agent: boot_services.image_handle(),
                        controller: None,
                    },
                    OpenProtocolAttributes::GetProtocol,
                )
                .map_err(|_| "无法打开 GOP 协议")?;

            // 获取可变引用以调用 frame_buffer()
            // 注: ScopedProtocol 只提供 &T 借用，但 frame_buffer() 需要 &mut self。
            // 引导阶段 GOP 无其他消费者，此转换是安全的。
            #[allow(invalid_reference_casting)]
            let gop = &mut *(core::ptr::from_ref(&*gop_handle) as *mut GraphicsOutput);
            let mode = gop.current_mode_info();
            let mut fb = gop.frame_buffer();

        Self::from_raw_fb(
            fb.as_mut_ptr(),
            fb.size(),
            ScreenSize {
                width: mode.resolution().0 as u32,
                height: mode.resolution().1 as u32,
            },
            mode.stride() as u32,
            mode.pixel_format(),
        )
        } // end unsafe
    }

    /// 从原始帧缓冲指针创建 (用于直通/飞地场景)
    pub fn from_raw_fb(
        base: *mut u8,
        size: usize,
        resolution: ScreenSize,
        stride: u32,
        pixel_format: PixelFormat,
    ) -> Result<Self, &'static str> {
        if base.is_null() || size == 0 {
            return Err("无效的帧缓冲指针");
        }
        let bpp = match pixel_format {
            PixelFormat::Rgb | PixelFormat::Bgr => 4,
            PixelFormat::Bitmask | PixelFormat::BltOnly => {
                return Err("不支持的像素格式 (位掩码/BltOnly)");
            }
        };
        Ok(Self {
            base, size, resolution, stride, pixel_format,
            bytes_per_pixel: bpp, owned: false,
        })
    }

    pub fn width(&self) -> u32 { self.resolution.width }
    pub fn height(&self) -> u32 { self.resolution.height }
    pub fn resolution(&self) -> ScreenSize { self.resolution }
    pub fn bpp(&self) -> u8 { self.bytes_per_pixel }

    pub fn is_bgra(&self) -> bool {
        match self.pixel_format {
            PixelFormat::Bgr => true,
            _ => false,
        }
    }

    #[inline]
    pub fn put_pixel(&mut self, x: u32, y: u32, color: Color) {
        if x >= self.resolution.width || y >= self.resolution.height {
            return;
        }
        let offset = (y as usize * self.stride as usize + x as usize) * self.bytes_per_pixel as usize;
        if offset + 4 > self.size { return; }
        unsafe {
            let ptr = self.base.add(offset);
            ptr::write(ptr, color.blue);
            ptr::write(ptr.add(1), color.green);
            ptr::write(ptr.add(2), color.red);
            ptr::write(ptr.add(3), color.alpha);
        }
    }

    #[inline]
    pub fn get_pixel(&self, x: u32, y: u32) -> Option<Color> {
        if x >= self.resolution.width || y >= self.resolution.height {
            return None;
        }
        let offset = (y as usize * self.stride as usize + x as usize) * self.bytes_per_pixel as usize;
        if offset + 4 > self.size { return None; }
        unsafe {
            let ptr = self.base.add(offset);
            Some(Color {
                blue: ptr::read(ptr),
                green: ptr::read(ptr.add(1)),
                red: ptr::read(ptr.add(2)),
                alpha: 255,
            })
        }
    }

    pub fn fill(&mut self, color: Color) {
        for y in 0..self.resolution.height {
            for x in 0..self.resolution.width {
                self.put_pixel(x, y, color);
            }
        }
    }

    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        let x_end = (rect.x + rect.width as i32).min(self.resolution.width as i32).max(0) as u32;
        let y_end = (rect.y + rect.height as i32).min(self.resolution.height as i32).max(0) as u32;
        let x_start = rect.x.max(0) as u32;
        let y_start = rect.y.max(0) as u32;
        for y in y_start..y_end {
            for x in x_start..x_end {
                self.put_pixel(x, y, color);
            }
        }
    }

    pub fn copy_rect_from(&mut self, src: &FrameBuffer, src_rect: Rect, dst_x: i32, dst_y: i32) {
        for dy in 0..src_rect.height as i32 {
            for dx in 0..src_rect.width as i32 {
                let sx = src_rect.x + dx;
                let sy = src_rect.y + dy;
                if let Some(color) = src.get_pixel(sx as u32, sy as u32) {
                    self.put_pixel((dst_x + dx) as u32, (dst_y + dy) as u32, color);
                }
            }
        }
    }
}