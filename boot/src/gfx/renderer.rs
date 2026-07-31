//! 整数运算 2D 渲染器 — 亚克力毛玻璃效果
//!
//! 核心思想: 在 UEFI 环境中不使用浮点运算 (FPU 状态不可靠)，
//! 所有模糊、Alpha混合等运算全部用整数定点数完成。
//!
//! 技术细节：
//!   - 高斯模糊核在编译期预计算 (build.rs → blur_kernel.rs)
//!   - 方框模糊采样 + Alpha 混合模拟毛玻璃
//!   - 圆角矩形通过逐像素检查距离实现
//!   - 所有坐标使用 i32/u32 定点

use crate::gfx::framebuffer::{Color, FrameBuffer, Rect, ScreenSize};
use core::cmp;

// 编译期生成的模糊核
include!(concat!(env!("OUT_DIR"), "/blur_kernel.rs"));

/// 2D 渲染器
pub struct Renderer<'fb> {
    fb: &'fb mut FrameBuffer,
}

impl<'fb> Renderer<'fb> {
    pub fn new(fb: &'fb mut FrameBuffer) -> Self {
        Self { fb }
    }

    pub fn screen_size(&self) -> ScreenSize {
        self.fb.resolution()
    }

    pub fn width(&self) -> u32 { self.fb.width() }
    pub fn height(&self) -> u32 { self.fb.height() }

    // ============================================================
    // 基础绘制
    // ============================================================

    /// 填充整个屏幕
    pub fn clear(&mut self, color: Color) {
        self.fb.fill(color);
    }

    /// 绘制实心矩形
    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, color: Color) {
        self.fb.fill_rect(Rect::new(x, y, w, h), color);
    }

    /// 绘制实心圆角矩形
    pub fn fill_rounded_rect(&mut self, x: i32, y: i32, w: u32, h: u32, radius: u32, color: Color) {
        let r = radius as i32;
        for dy in 0..h as i32 {
            for dx in 0..w as i32 {
                // 检查是否在圆角内
                let in_corner = |cx: i32, cy: i32| -> bool {
                    let ddx = dx - cx;
                    let ddy = dy - cy;
                    (ddx * ddx + ddy * ddy) <= r * r
                };

                let inside = if dy < r {
                    if dx < r { in_corner(r, r) }
                    else if dx >= w as i32 - r { in_corner(w as i32 - r - 1, r) }
                    else { true }
                } else if dy >= h as i32 - r {
                    if dx < r { in_corner(r, h as i32 - r - 1) }
                    else if dx >= w as i32 - r { in_corner(w as i32 - r - 1, h as i32 - r - 1) }
                    else { true }
                } else {
                    true
                };

                if inside {
                    self.fb.put_pixel((x + dx) as u32, (y + dy) as u32, color);
                }
            }
        }
    }

    /// 绘制水平线
    pub fn hline(&mut self, x: i32, y: i32, len: u32, color: Color) {
        for i in 0..len {
            self.fb.put_pixel((x + i as i32) as u32, y as u32, color);
        }
    }

    /// 绘制垂直线
    pub fn vline(&mut self, x: i32, y: i32, len: u32, color: Color) {
        for i in 0..len {
            self.fb.put_pixel(x as u32, (y + i as i32) as u32, color);
        }
    }

    // ============================================================
    // 亚克力毛玻璃效果
    // ============================================================

    /// 应用亚克力毛玻璃效果到指定矩形区域
    ///
    /// 原理：
    ///   1. 对区域内每个像素，以 BLUR_RADIUS 对背景采样
    ///   2. 用预计算的整数模糊核进行加权平均
    ///   3. 将模糊结果与给定的半透明 tint 颜色进行 Alpha 混合
    ///
    /// 参数:
    ///   - rect: 要模糊的区域
    ///   - tint: 叠加的半透明颜色 (alpha 控制强度)
    pub fn apply_acrylic(&mut self, rect: Rect, tint: Color) {
        let x_start = rect.x.max(0) as u32;
        let y_start = rect.y.max(0) as u32;
        let x_end = cmp::min(rect.x + rect.width as i32, self.fb.width() as i32).max(0) as u32;
        let y_end = cmp::min(rect.y + rect.height as i32, self.fb.height() as i32).max(0) as u32;

        // 为模糊操作创建临时缓冲区 (只缓存一行)
        let mut row_buffer: [Color; 1920] = [Color::TRANSPARENT; 1920];
        let max_width = cmp::min(x_end - x_start, 1920) as usize;

        let r_offset = BLUR_RADIUS as i32;

        for py in y_start..y_end {
            // 1. 对该行每个像素做方框模糊采样
            for (i, px) in (x_start..x_end).enumerate() {
                if i >= max_width {
                    break;
                }
                let mut r_sum: u32 = 0;
                let mut g_sum: u32 = 0;
                let mut b_sum: u32 = 0;
                let mut count: u32 = 0;

                // 采样模糊核覆盖区域
                for ky in -r_offset..=r_offset {
                    for kx in -r_offset..=r_offset {
                        let wx = (kx - (-r_offset)) as usize;
                        let wy = (ky - (-r_offset)) as usize;
                        let weight = BLUR_KERNEL[wy * KERNEL_SIZE + wx] as u32;

                        let sx = (px as i32 + kx).max(0).min(self.fb.width() as i32 - 1) as u32;
                        let sy = (py as i32 + ky).max(0).min(self.fb.height() as i32 - 1) as u32;

                        if let Some(c) = self.fb.get_pixel(sx, sy) {
                            r_sum += c.red as u32 * weight;
                            g_sum += c.green as u32 * weight;
                            b_sum += c.blue as u32 * weight;
                            count += weight;
                        }
                    }
                }

                let divisor = if count > 0 { count } else { 1 };
                let blurred = Color {
                    red:   ((r_sum / divisor) >> 12).min(255) as u8,
                    green: ((g_sum / divisor) >> 12).min(255) as u8,
                    blue:  ((b_sum / divisor) >> 12).min(255) as u8,
                    alpha: 255,
                };

                // 2. 将模糊背景与 tint 混合
                row_buffer[i] = tint.blend_over(blurred);
            }

            // 写回帧缓冲
            for (i, px) in (x_start..x_end).enumerate() {
                if i >= max_width {
                    break;
                }
                self.fb.put_pixel(px, py, row_buffer[i]);
            }
        }
    }

    /// 快速方框模糊 (比高斯核更快, 用于进度条等小元素)
    pub fn box_blur_rect(&mut self, rect: Rect, radius: i32) {
        let x_start = rect.x.max(0) as u32;
        let y_start = rect.y.max(0) as u32;
        let x_end = cmp::min(rect.x + rect.width as i32, self.fb.width() as i32).max(0) as u32;
        let y_end = cmp::min(rect.y + rect.height as i32, self.fb.height() as i32).max(0) as u32;

        for py in y_start..y_end {
            for px in x_start..x_end {
                let mut r_sum: u32 = 0;
                let mut g_sum: u32 = 0;
                let mut b_sum: u32 = 0;
                let mut cnt: u32 = 0;

                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        let sx = (px as i32 + dx).max(0).min(self.fb.width() as i32 - 1) as u32;
                        let sy = (py as i32 + dy).max(0).min(self.fb.height() as i32 - 1) as u32;
                        if let Some(c) = self.fb.get_pixel(sx, sy) {
                            r_sum += c.red as u32;
                            g_sum += c.green as u32;
                            b_sum += c.blue as u32;
                            cnt += 1;
                        }
                    }
                }

                if cnt > 0 {
                    self.fb.put_pixel(px, py, Color {
                        red:   (r_sum / cnt) as u8,
                        green: (g_sum / cnt) as u8,
                        blue:  (b_sum / cnt) as u8,
                        alpha: 255,
                    });
                }
            }
        }
    }

    // ============================================================
    // 辅助方法
    // ============================================================

    /// 居中 X 坐标
    pub fn center_x(&self, width: u32) -> i32 {
        ((self.fb.width() as i32 - width as i32) / 2).max(0)
    }

    /// 居中 Y 坐标
    pub fn center_y(&self, height: u32) -> i32 {
        ((self.fb.height() as i32 - height as i32) / 2).max(0)
    }

    /// 获取帧缓冲的不可变引用
    pub fn framebuffer(&self) -> &FrameBuffer {
        self.fb
    }

    /// 获取帧缓冲的可变引用 (用于直接像素操作)
    pub fn framebuffer_mut(&mut self) -> &mut FrameBuffer {
        self.fb
    }
}
