//! Morion Boot — 最小化 GOP 测试
//!
//! 不依赖内核，只测试 UEFI GOP 帧缓冲能否正常渲染亚克力引导菜单。

#![no_std]
#![no_main]
#![allow(invalid_reference_casting)]

use uefi::prelude::*;
use uefi::proto::console::gop::GraphicsOutput;
use uefi::table::boot::{BootServices, OpenProtocolAttributes, OpenProtocolParams, SearchType};
use uefi::Identify;

// uefi crate 的 alloc feature 需要 global_allocator
#[global_allocator]
static ALLOCATOR: uefi::allocator::Allocator = uefi::allocator::Allocator;

/// 颜色 (BGRA)
#[derive(Clone, Copy)]
struct Color {
    b: u8, g: u8, r: u8, a: u8,
}

/// 帧缓冲
struct Fb {
    base: *mut u8,
    size: usize,
    width: u32,
    height: u32,
    stride: u32,
}

impl Fb {
    fn from_gop(bs: &BootServices) -> Result<Self, Status> {
        // SAFETY: UEFI 引导阶段，GOP 由本应用独占，所有 UEFI 协议调用安全
        unsafe {
            let gop_guid = GraphicsOutput::GUID;
            let handles = bs
                .locate_handle_buffer(SearchType::ByProtocol(&gop_guid))
                .map_err(|_| Status::UNSUPPORTED)?;
            let handle = handles.first().copied().ok_or(Status::UNSUPPORTED)?;

            let gop_handle = bs
                .open_protocol::<GraphicsOutput>(
                    OpenProtocolParams { handle, agent: bs.image_handle(), controller: None },
                    OpenProtocolAttributes::GetProtocol,
                )
                .map_err(|_| Status::UNSUPPORTED)?;

            // 只读: 获取分辨率信息
            let gop = &*gop_handle;
            let mode = gop.current_mode_info();
            let (w, h) = mode.resolution();

            // frame_buffer() 需要 &mut — 引导阶段我们是 GOP 唯一持有者
            let gop_mut = &mut *(gop as *const GraphicsOutput as *mut GraphicsOutput);
            let mut fb_buffer = gop_mut.frame_buffer();
            let fb_ptr = fb_buffer.as_mut_ptr();
            let fb_size = fb_buffer.size();

            let result = Ok(Fb {
                base: fb_ptr,
                size: fb_size,
                width: w as u32,
                height: h as u32,
                stride: mode.stride() as u32,
            });
            result
        } // end unsafe
    }

    fn put_pixel(&mut self, x: u32, y: u32, c: Color) {
        if x >= self.width || y >= self.height { return; }
        let off = (y as usize * self.stride as usize + x as usize) * 4;
        if off + 4 > self.size { return; }
        unsafe {
            *self.base.add(off)     = c.b;
            *self.base.add(off + 1) = c.g;
            *self.base.add(off + 2) = c.r;
            *self.base.add(off + 3) = c.a;
        }
    }

    fn fill(&mut self, c: Color) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, c);
            }
        }
    }

    fn fill_rect(&mut self, rx: u32, ry: u32, rw: u32, rh: u32, c: Color) {
        for y in ry..(ry + rh).min(self.height) {
            for x in rx..(rx + rw).min(self.width) {
                self.put_pixel(x, y, c);
            }
        }
    }
}

/// 8x16 位图数字 (0-9)
const DIGIT_8X16: [[u8; 16]; 10] = [
    [0x00,0x3C,0x66,0x6E,0x76,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // 0
    [0x00,0x18,0x38,0x78,0x18,0x18,0x18,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // 1
    [0x00,0x3C,0x66,0x06,0x0C,0x18,0x30,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // 2
    [0x00,0x3C,0x66,0x06,0x1C,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // 3
    [0x00,0x0C,0x1C,0x3C,0x6C,0x7E,0x0C,0x0C,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // 4
    [0x00,0x7E,0x60,0x7C,0x06,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // 5
    [0x00,0x3C,0x60,0x7C,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // 6
    [0x00,0x7E,0x06,0x0C,0x18,0x30,0x30,0x30,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // 7
    [0x00,0x3C,0x66,0x66,0x3C,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // 8
    [0x00,0x3C,0x66,0x66,0x3E,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // 9
];

fn draw_char(fb: &mut Fb, ch: u8, cx: u32, cy: u32, fg: Color) {
    let glyph = if (b'0'..=b'9').contains(&ch) {
        DIGIT_8X16[(ch - b'0') as usize]
    } else {
        [0u8; 16]
    };
    for row in 0..16u32 {
        let bits = glyph[row as usize];
        for col in 0..8u32 {
            if bits & (0x80 >> col) != 0 {
                fb.put_pixel(cx + col, cy + row, fg);
            }
        }
    }
}

fn draw_text(fb: &mut Fb, text: &str, cx: u32, cy: u32, fg: Color) {
    for (i, ch) in text.bytes().enumerate() {
        draw_char(fb, ch, cx + i as u32 * 8, cy, fg);
    }
}

#[cfg(target_os = "uefi")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop { core::hint::spin_loop(); }
}

/// UEFI 入口点 — 纯 GOP 测试
#[entry]
fn efi_main(_image: uefi::Handle, st: SystemTable<Boot>) -> Status {
    let bs = st.boot_services();

    // 1. 初始化 GOP 帧缓冲
    let mut fb = match Fb::from_gop(bs) {
        Ok(fb) => fb,
        Err(e) => return e,
    };

    // 2. 清屏 (深色背景)
    fb.fill(Color { r: 0x0A, g: 0x0C, b: 0x14, a: 255 });

    // 3. 绘制亚克力风格菜单面板
    let panel_w = 640u32;
    let panel_h = 420u32;
    let px = (fb.width - panel_w) / 2;
    let py = (fb.height - panel_h) / 2;

    // 半透明面板
    fb.fill_rect(px, py, panel_w, panel_h,
        Color { r: 0x15, g: 0x1A, b: 0x2A, a: 220 });

    // 标题栏
    fb.fill_rect(px, py, panel_w, 48,
        Color { r: 0x20, g: 0x28, b: 0x3E, a: 255 });

    // 标题文字
    draw_text(&mut fb, "Morion OS Bootloader", px + 24, py + 14,
        Color { r: 0xFF, g: 0xFF, b: 0xFF, a: 255 });

    // 版本
    draw_text(&mut fb, "v0.1.0 | Acrylic Theme", px + 24, py + 36,
        Color { r: 0x88, g: 0x8A, b: 0x9A, a: 255 });

    // 菜单项背景
    let item_y = py + 64;
    for i in 0u32..4 {
        let iy = item_y + i * 52;
        // 项背景
        let bg = if i == 0 {
            Color { r: 0x3A, g: 0x6A, b: 0xFF, a: 180 }  // 选中高亮
        } else {
            Color { r: 0x18, g: 0x1F, b: 0x30, a: 120 }
        };
        fb.fill_rect(px + 16, iy, panel_w - 32, 44, bg);

        let label = match i {
            0 => "[*] Morion OS (current generation)",
            1 => "[ ] Morion OS gen 2 (rollback)",
            2 => "[R] Rescue mode",
            _ => "[F] UEFI Firmware Settings",
        };
        draw_text(&mut fb, label, px + 32, iy + 14,
            Color { r: 0xFF, g: 0xFF, b: 0xFF, a: 255 });
    }

    // 底栏
    let bottom_y = py + panel_h - 36;
    fb.fill_rect(px, bottom_y, panel_w, 36,
        Color { r: 0x12, g: 0x18, b: 0x24, a: 255 });
    draw_text(&mut fb, "ENTER=boot  ESC=reboot  F2=security  F10=shutdown",
        px + 16, bottom_y + 10,
        Color { r: 0x99, g: 0x9B, b: 0xAA, a: 255 });

    // 4. 死循环 (等待用户观察画面)
    loop {
        core::hint::spin_loop();
    }
}
