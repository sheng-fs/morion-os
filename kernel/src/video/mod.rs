//! 视频输出 — 全局帧缓冲 + 文本光标 + 便捷打印接口

pub mod font;
pub mod framebuffer;

use crate::bootinfo::BootInfo;
use framebuffer::Framebuffer;

// 全局帧缓冲状态 (初始化后只读访问, 启动期单线程)
static mut FB: Framebuffer = Framebuffer::empty();
static mut CURSOR_X: u32 = 0;
static mut CURSOR_Y: u32 = 0;

const MARGIN: u32 = 16;

/// 从 Boot Info 初始化帧缓冲并清屏
pub fn init(info: &BootInfo) {
    unsafe {
        FB = Framebuffer::init(info.fb_addr, info.fb_width, info.fb_height, info.fb_stride);
        CURSOR_X = MARGIN;
        CURSOR_Y = MARGIN;
    }
    clear(0x08102A);
}

/// 帧缓冲是否可用 (panic/异常处理在打印前检查)
pub fn ready() -> bool {
    unsafe { FB.is_ready() }
}

pub fn width() -> u32 {
    unsafe { FB.width() }
}

pub fn height() -> u32 {
    unsafe { FB.height() }
}

/// 清屏
pub fn clear(color: u32) {
    unsafe { FB.clear(color) }
}

/// 将光标移动到指定位置 (字符坐标)
pub fn set_cursor(x: u32, y: u32) {
    unsafe {
        CURSOR_X = x;
        CURSOR_Y = y;
    }
}

/// 打印字符串 (支持 '\n' 换行)
pub fn print(s: &str) {
    for ch in s.bytes() {
        match ch {
            b'\n' => {
                unsafe {
                    CURSOR_X = MARGIN;
                    CURSOR_Y += font::CHAR_HEIGHT + 4;
                }
            }
            _ => {
                unsafe {
                    if CURSOR_X + font::CHAR_WIDTH >= FB.width() {
                        CURSOR_X = MARGIN;
                        CURSOR_Y += font::CHAR_HEIGHT + 4;
                    }
                    if CURSOR_Y + font::CHAR_HEIGHT >= FB.height() {
                        // 越界时回卷到顶部
                        CURSOR_Y = MARGIN;
                    }
                    font::draw_char(&mut FB, CURSOR_X, CURSOR_Y, ch, 0xFFFFFF);
                    CURSOR_X += font::CHAR_WIDTH;
                }
            }
        }
    }
}

/// 打印字符串并换行
pub fn println(s: &str) {
    print(s);
    print("\n");
}

/// 打印 u64 的 16 进制 (16 位补零)
pub fn print_hex(v: u64) {
    let hex = b"0123456789ABCDEF";
    let mut buf = [0u8; 16];
    for i in 0..16 {
        buf[i] = hex[((v >> (60 - i * 4)) & 0xF) as usize];
    }
    print(unsafe { core::str::from_utf8_unchecked(&buf) });
}

/// 打印 u64 的十进制
pub fn print_u64(v: u64) {
    let mut buf = [0u8; 20];
    let mut i = 20;
    let mut val = v;
    if val == 0 {
        print("0");
        return;
    }
    while val > 0 && i > 0 {
        i -= 1;
        buf[i] = (val % 10) as u8 + b'0';
        val /= 10;
    }
    print(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}