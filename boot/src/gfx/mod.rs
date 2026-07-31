//! Morion Boot 图形子系统
//!
//! 基于 UEFI Graphics Output Protocol (GOP) 的高分辨率 2D 渲染引擎。
//! 核心特性：
//!   - 亚克力毛玻璃效果 (编译期预计算模糊核, 运行时纯整数运算)
//!   - 多分辨率自适应布局
//!   - 真彩色支持 (32-bit BGRx/RGBx)
//!   - 软件光标渲染
//!   - 简单的帧动画引擎

pub mod framebuffer;
pub mod renderer;
pub mod font;
pub mod animation;

pub use framebuffer::FrameBuffer;
pub use renderer::Renderer;
pub use font::BitmapFont;
pub use animation::AnimationEngine;
