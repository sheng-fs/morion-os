//! 帧动画引擎
//!
//! 在 UEFI 环境中实现简单的逐帧动画系统。
//! 支持：
//!   - 加载动画 (旋转指示器)
//!   - 渐变过渡 (淡入/淡出)
//!   - 基于 UEFI 定时器事件的帧更新

use crate::gfx::framebuffer::{Color, FrameBuffer, Rect};
use uefi::table::boot::BootServices;

/// 动画帧
pub struct AnimationFrame<'a> {
    /// 帧位图数据 (BGRA 原始像素)
    pub data: &'a [u8],
    /// 帧宽度
    pub width: u32,
    /// 帧高度
    pub height: u32,
    /// 帧持续时间 (毫秒)
    pub duration_ms: u64,
}

/// 动画状态
pub struct Animation {
    /// 帧列表
    frames: &'static [AnimationFrame<'static>],
    /// 当前帧索引
    current_frame: usize,
    /// 帧内已用时间
    elapsed_ms: u64,
    /// 是否正在播放
    playing: bool,
    /// 是否循环
    looping: bool,
    /// 播放完成回调 (无状态, 仅用标志)
    finished: bool,
}

/// 动画引擎
pub struct AnimationEngine {
    animations: [Option<Animation>; 4],
}

impl AnimationEngine {
    pub fn new() -> Self {
        Self {
            animations: [const { None }; 4],
        }
    }

    /// 注册一个动画
    pub fn register(&mut self, slot: usize, anim: Animation) {
        if slot < self.animations.len() {
            self.animations[slot] = Some(anim);
        }
    }

    /// 更新所有动画 (每帧调用一次)
    ///
    /// 返回是否有任何动画处于活动状态
    pub fn update(&mut self, delta_ms: u64) -> bool {
        let mut active = false;
        for anim in self.animations.iter_mut().flatten() {
            if !anim.playing || anim.finished {
                continue;
            }
            active = true;
            anim.elapsed_ms += delta_ms;

            let frame = &anim.frames[anim.current_frame];
            if anim.elapsed_ms >= frame.duration_ms {
                anim.elapsed_ms -= frame.duration_ms;
                anim.current_frame += 1;

                if anim.current_frame >= anim.frames.len() {
                    if anim.looping {
                        anim.current_frame = 0;
                    } else {
                        anim.current_frame = anim.frames.len() - 1;
                        anim.finished = true;
                    }
                }
            }
        }
        active
    }

    /// 渲染指定槽位的动画到指定位置
    pub fn render(&self, slot: usize, fb: &mut FrameBuffer, x: i32, y: i32) -> bool {
        if let Some(anim) = &self.animations[slot] {
            if anim.frames.is_empty() {
                return false;
            }
            let frame = &anim.frames[anim.current_frame];
            render_raw_frame(fb, frame.data, frame.width, frame.height, x, y);
            return true;
        }
        false
    }

    /// 获取当前动画是否为指定状态
    pub fn is_finished(&self, slot: usize) -> bool {
        self.animations[slot]
            .as_ref()
            .map_or(true, |a| a.finished)
    }
}

/// 渲染原始 BGRA 帧数据到帧缓冲
fn render_raw_frame(
    fb: &mut FrameBuffer,
    data: &[u8],
    width: u32,
    height: u32,
    dx: i32,
    dy: i32,
) {
    for py in 0..height {
        for px in 0..width {
            let offset = ((py * width + px) * 4) as usize;
            if offset + 3 >= data.len() {
                continue;
            }
            let color = Color {
                blue: data[offset],
                green: data[offset + 1],
                red: data[offset + 2],
                alpha: data[offset + 3],
            };
            // 跳过完全透明像素
            if color.alpha == 0 {
                continue;
            }
            let tx = (dx + px as i32).max(0) as u32;
            let ty = (dy + py as i32).max(0) as u32;
            if tx < fb.width() && ty < fb.height() {
                if color.alpha == 255 {
                    fb.put_pixel(tx, ty, color);
                } else {
                    // Alpha 混合
                    if let Some(bg) = fb.get_pixel(tx, ty) {
                        fb.put_pixel(tx, ty, color.blend_over(bg));
                    } else {
                        fb.put_pixel(tx, ty, color);
                    }
                }
            }
        }
    }
}

/// 创建旋转加载动画的帧数据 (运行时生成简单的圆形加载动画)
pub fn create_loading_animation() -> Animation {
    // 8帧旋转的简易加载指示器 (12x12 像素)
    // 实际使用时从嵌入的资源文件加载，这里提供运行时生成的备用方案
    Animation {
        // 占位空切片 — 实际通过主题配置加载;
        // [..] 强制 &[AnimationFrame<'static>; 0] 转为 &[AnimationFrame<'static>]
        frames: (&[][..]),
        current_frame: 0,
        elapsed_ms: 0,
        playing: false,
        looping: true,
        finished: false,
    }
}

/// 淡入过渡辅助
pub struct FadeTransition {
    pub start_alpha: u8,
    pub end_alpha: u8,
    pub current_alpha: u8,
    pub duration_ms: u64,
    pub elapsed_ms: u64,
    pub active: bool,
}

impl FadeTransition {
    pub fn fade_in(duration_ms: u64) -> Self {
        Self {
            start_alpha: 0,
            end_alpha: 255,
            current_alpha: 0,
            duration_ms,
            elapsed_ms: 0,
            active: true,
        }
    }

    pub fn fade_out(duration_ms: u64) -> Self {
        Self {
            start_alpha: 255,
            end_alpha: 0,
            current_alpha: 255,
            duration_ms,
            elapsed_ms: 0,
            active: true,
        }
    }

    /// 更新并返回当前 alpha
    pub fn update(&mut self, delta_ms: u64) -> u8 {
        if !self.active {
            return self.end_alpha;
        }
        self.elapsed_ms += delta_ms;
        if self.elapsed_ms >= self.duration_ms {
            self.active = false;
            self.current_alpha = self.end_alpha;
        } else {
            let t = self.elapsed_ms as f64 / self.duration_ms as f64;
            // 使用 ease-in-out 缓动
            let eased = if t < 0.5 {
                2.0 * t * t
            } else {
                -1.0 + (4.0 - 2.0 * t) * t
            };
            let range = self.end_alpha as f64 - self.start_alpha as f64;
            self.current_alpha = (self.start_alpha as f64 + range * eased) as u8;
        }
        self.current_alpha
    }

    pub fn is_finished(&self) -> bool {
        !self.active
    }
}
