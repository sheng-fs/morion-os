//! Morion Boot — 声明式微引导加载器
//!
//! 基于 UEFI GOP 渲染亚克力风格引导菜单。
//! 支持: 声明式主题 (theme.toml) | 键盘导航 | 鼠标指针 | 多代引导条目

#![no_std]
#![no_main]
#![allow(invalid_reference_casting)]

use core::panic::PanicInfo;
use uefi::prelude::*;
use uefi::proto::console::gop::GraphicsOutput;
use uefi::table::boot::{OpenProtocolAttributes, OpenProtocolParams, SearchType};
use uefi::Identify;
use morion_boot::boot::loader::{Elf64Header, Elf64ProgramHeader};

#[global_allocator]
static ALLOCATOR: uefi::allocator::Allocator = uefi::allocator::Allocator;

// ============================================================
//  基本类型
// ============================================================

#[derive(Clone, Copy)]
struct Color { b: u8, g: u8, r: u8, a: u8 }

impl Color {
    const fn rgb(r: u8, g: u8, b: u8) -> Self { Self { r, g, b, a: 255 } }
    const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self { Self { r, g, b, a } }
    fn from_hex(hex: &str) -> Option<Self> {
        let h = hex.trim_start_matches('#');
        if h.len() != 6 { return None; }
        let b = h.as_bytes();
        let v = |i: usize| -> Option<u8> {
            match b[i] { c@b'0'..=b'9' => Some(c - b'0'), c@b'a'..=b'f' => Some(c - b'a'+10), c@b'A'..=b'F' => Some(c - b'A'+10), _ => None }
        };
        Some(Self { r: v(0)?*16+v(1)?, g: v(2)?*16+v(3)?, b: v(4)?*16+v(5)?, a: 255 })
    }
}

#[derive(Clone, Copy)]
struct Rect { x: i32, y: i32, w: u32, h: u32 }

impl Rect {
    const fn new(x: i32, y: i32, w: u32, h: u32) -> Self { Self { x, y, w, h } }
    fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w as i32 && py >= self.y && py < self.y + self.h as i32
    }
}

struct Fb { base: *mut u8, size: usize, w: u32, h: u32, stride: u32 }

impl Fb {
    fn from_gop(bs: &BootServices) -> Result<Self, Status> {
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
                ).map_err(|_| Status::UNSUPPORTED)?;

            let gop = &*gop_handle;
            let mode = gop.current_mode_info();
            let (w, h) = mode.resolution();
            let gop_mut = &mut *(gop as *const GraphicsOutput as *mut GraphicsOutput);
            let mut fbb = gop_mut.frame_buffer();
            Ok(Fb { base: fbb.as_mut_ptr(), size: fbb.size(), w: w as u32, h: h as u32, stride: mode.stride() as u32 })
        }
    }

    fn pixel(&mut self, x: i32, y: i32, c: Color) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 { return; }
        let off = (y as usize * self.stride as usize + x as usize) * 4;
        if off + 4 > self.size { return; }
        unsafe { *self.base.add(off)=c.b; *self.base.add(off+1)=c.g; *self.base.add(off+2)=c.r; *self.base.add(off+3)=c.a; }
    }

    fn get_pixel(&self, x: i32, y: i32) -> Color {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 { return Color::rgb(0, 0, 0); }
        let off = (y as usize * self.stride as usize + x as usize) * 4;
        if off + 4 > self.size { return Color::rgb(0, 0, 0); }
        unsafe { Color { r: *self.base.add(off+2), g: *self.base.add(off+1), b: *self.base.add(off), a: *self.base.add(off+3) } }
    }

    // 半透明填充: 按 alpha 将前景色混合到已有背景像素 (真正的 alpha blending)。
    fn fill_rect(&mut self, r: Rect, c: Color) {
        let a = c.a as u32;
        for y in r.y.max(0)..((r.y+r.h as i32).min(self.h as i32)) {
            for x in r.x.max(0)..((r.x+r.w as i32).min(self.w as i32)) {
                let bg = self.get_pixel(x, y);
                let rr = (c.r as u32 * a + bg.r as u32 * (255 - a)) / 255;
                let gg = (c.g as u32 * a + bg.g as u32 * (255 - a)) / 255;
                let bb = (c.b as u32 * a + bg.b as u32 * (255 - a)) / 255;
                self.pixel(x, y, Color::rgb(rr as u8, gg as u8, bb as u8));
            }
        }
    }

    // 半透明圆角填充 (alpha blending)。
    fn fill_rounded(&mut self, r: Rect, radius: u32, c: Color) {
        let r2 = (radius * radius) as i32;
        let a = c.a as u32;
        for y in r.y.max(0)..((r.y+r.h as i32).min(self.h as i32)) {
            for x in r.x.max(0)..((r.x+r.w as i32).min(self.w as i32)) {
                let cx = r.x + radius as i32;
                let cy = r.y + radius as i32;
                let dx = if x < cx { cx - x - 1 } else if x >= r.x + r.w as i32 - radius as i32 { x - (r.x + r.w as i32 - radius as i32) } else { 0 };
                let dy = if y < cy { cy - y - 1 } else if y >= r.y + r.h as i32 - radius as i32 { y - (r.y + r.h as i32 - radius as i32) } else { 0 };
                if dx*dx + dy*dy <= r2 {
                    let bg = self.get_pixel(x, y);
                    let rr = (c.r as u32 * a + bg.r as u32 * (255 - a)) / 255;
                    let gg = (c.g as u32 * a + bg.g as u32 * (255 - a)) / 255;
                    let bb = (c.b as u32 * a + bg.b as u32 * (255 - a)) / 255;
                    self.pixel(x, y, Color::rgb(rr as u8, gg as u8, bb as u8));
                }
            }
        }
    }

    fn vline(&mut self, x: i32, y0: i32, y1: i32, c: Color) {
        for y in y0..=y1 { self.pixel(x, y, c); }
    }

    fn hline(&mut self, y: i32, x0: i32, x1: i32, c: Color) {
        for x in x0..=x1 { self.pixel(x, y, c); }
    }
}

// ============================================================
//  8x16 位图字体
// ============================================================

const FONT8X16: [[u8; 16]; 95] = [
[0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],// sp
[0x00,0x00,0x18,0x3C,0x3C,0x3C,0x18,0x18,0x18,0x00,0x18,0x18,0x00,0x00,0x00,0x00],// !
[0x00,0x66,0x66,0x66,0x24,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],// "
[0x00,0x00,0x00,0x6C,0x6C,0xFE,0x6C,0x6C,0x6C,0xFE,0x6C,0x6C,0x00,0x00,0x00,0x00],// #
[0x18,0x18,0x7C,0xC6,0xC2,0xC0,0x7C,0x06,0x86,0xC6,0x7C,0x18,0x18,0x00,0x00,0x00],// $
[0x00,0x00,0x00,0x00,0xC2,0xC6,0x0C,0x18,0x30,0x60,0xC6,0x86,0x00,0x00,0x00,0x00],// %
[0x00,0x00,0x38,0x6C,0x6C,0x38,0x76,0xDC,0xCC,0xCC,0xCC,0x76,0x00,0x00,0x00,0x00],// &
[0x00,0x30,0x30,0x30,0x60,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],// '
[0x00,0x00,0x0C,0x18,0x30,0x30,0x30,0x30,0x30,0x30,0x18,0x0C,0x00,0x00,0x00,0x00],// (
[0x00,0x00,0x30,0x18,0x0C,0x0C,0x0C,0x0C,0x0C,0x0C,0x18,0x30,0x00,0x00,0x00,0x00],// )
[0x00,0x00,0x00,0x00,0x00,0x66,0x3C,0xFF,0x3C,0x66,0x00,0x00,0x00,0x00,0x00,0x00],// *
[0x00,0x00,0x00,0x00,0x00,0x18,0x18,0x7E,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00],// +
[0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x18,0x18,0x18,0x30,0x00,0x00,0x00],// ,
[0x00,0x00,0x00,0x00,0x00,0x00,0x00,0xFE,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],// -
[0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x18,0x18,0x00,0x00,0x00,0x00],// .
[0x00,0x00,0x00,0x00,0x02,0x06,0x0C,0x18,0x30,0x60,0xC0,0x80,0x00,0x00,0x00,0x00],// /
[0x00,0x00,0x7C,0xC6,0xCE,0xDE,0xF6,0xE6,0xC6,0xC6,0x7C,0x00,0x00,0x00,0x00,0x00],// 0
[0x00,0x00,0x18,0x38,0x78,0x18,0x18,0x18,0x18,0x18,0x7E,0x00,0x00,0x00,0x00,0x00],// 1
[0x00,0x00,0x7C,0xC6,0x06,0x0C,0x18,0x30,0x60,0xC0,0xFE,0x00,0x00,0x00,0x00,0x00],// 2
[0x00,0x00,0x7C,0xC6,0x06,0x06,0x3C,0x06,0x06,0xC6,0x7C,0x00,0x00,0x00,0x00,0x00],// 3
[0x00,0x00,0x0C,0x1C,0x3C,0x6C,0xCC,0xFE,0x0C,0x0C,0x1E,0x00,0x00,0x00,0x00,0x00],// 4
[0x00,0x00,0xFE,0xC0,0xC0,0xFC,0x06,0x06,0x06,0xC6,0x7C,0x00,0x00,0x00,0x00,0x00],// 5
[0x00,0x00,0x38,0x60,0xC0,0xC0,0xFC,0xC6,0xC6,0xC6,0x7C,0x00,0x00,0x00,0x00,0x00],// 6
[0x00,0x00,0xFE,0xC6,0x06,0x06,0x0C,0x18,0x30,0x30,0x30,0x00,0x00,0x00,0x00,0x00],// 7
[0x00,0x00,0x7C,0xC6,0xC6,0xC6,0x7C,0xC6,0xC6,0xC6,0x7C,0x00,0x00,0x00,0x00,0x00],// 8
[0x00,0x00,0x7C,0xC6,0xC6,0xC6,0x7E,0x06,0x06,0x0C,0x78,0x00,0x00,0x00,0x00,0x00],// 9
[0x00,0x00,0x00,0x00,0x18,0x18,0x00,0x00,0x00,0x18,0x18,0x00,0x00,0x00,0x00,0x00],// :
[0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x04,0x0E,0x1E,0x7C,0x7C,0x38,0x00],// ;
[0x00,0x00,0x00,0x06,0x0C,0x18,0x30,0x60,0x30,0x18,0x0C,0x06,0x00,0x00,0x00,0x00],// <
[0x00,0x00,0x00,0x00,0x00,0x00,0xFE,0x00,0x00,0xFE,0x00,0x00,0x00,0x00,0x00,0x00],// =
[0x00,0x00,0x00,0x60,0x30,0x18,0x0C,0x06,0x0C,0x18,0x30,0x60,0x00,0x00,0x00,0x00],// >
[0x00,0x00,0x7C,0xC6,0xC6,0x0C,0x18,0x18,0x18,0x00,0x18,0x18,0x00,0x00,0x00,0x00],// ?
[0x00,0x00,0x00,0x7C,0xC6,0xC6,0xDE,0xDE,0xDE,0xDC,0xC0,0x7C,0x00,0x00,0x00,0x00],// @
[0x00,0x00,0x10,0x38,0x6C,0xC6,0xC6,0xFE,0xC6,0xC6,0xC6,0x00,0x00,0x00,0x00,0x00],// A
[0x00,0x00,0xFC,0x66,0x66,0x66,0x7C,0x66,0x66,0x66,0xFC,0x00,0x00,0x00,0x00,0x00],// B
[0x00,0x00,0x3C,0x66,0xC2,0xC0,0xC0,0xC0,0xC2,0x66,0x3C,0x00,0x00,0x00,0x00,0x00],// C
[0x00,0x00,0xF8,0x6C,0x66,0x66,0x66,0x66,0x66,0x6C,0xF8,0x00,0x00,0x00,0x00,0x00],// D
[0x00,0x00,0xFE,0x66,0x62,0x68,0x78,0x68,0x62,0x66,0xFE,0x00,0x00,0x00,0x00,0x00],// E
[0x00,0x00,0xFE,0x66,0x62,0x68,0x78,0x68,0x60,0x60,0xF0,0x00,0x00,0x00,0x00,0x00],// F
[0x00,0x00,0x3C,0x66,0xC2,0xC0,0xC0,0xDE,0xC6,0x66,0x3A,0x00,0x00,0x00,0x00,0x00],// G
[0x00,0x00,0xC6,0xC6,0xC6,0xC6,0xFE,0xC6,0xC6,0xC6,0xC6,0x00,0x00,0x00,0x00,0x00],// H
[0x00,0x00,0x3C,0x18,0x18,0x18,0x18,0x18,0x18,0x18,0x3C,0x00,0x00,0x00,0x00,0x00],// I
[0x00,0x00,0x1E,0x0C,0x0C,0x0C,0x0C,0x0C,0xCC,0xCC,0x78,0x00,0x00,0x00,0x00,0x00],// J
[0x00,0x00,0xE6,0x66,0x6C,0x6C,0x78,0x6C,0x6C,0x66,0xE6,0x00,0x00,0x00,0x00,0x00],// K
[0x00,0x00,0xF0,0x60,0x60,0x60,0x60,0x60,0x62,0x66,0xFE,0x00,0x00,0x00,0x00,0x00],// L
[0x00,0x00,0xC6,0xEE,0xFE,0xFE,0xD6,0xC6,0xC6,0xC6,0xC6,0x00,0x00,0x00,0x00,0x00],// M
[0x00,0x00,0xC6,0xE6,0xF6,0xFE,0xDE,0xCE,0xC6,0xC6,0xC6,0x00,0x00,0x00,0x00,0x00],// N
[0x00,0x00,0x38,0x6C,0xC6,0xC6,0xC6,0xC6,0xC6,0x6C,0x38,0x00,0x00,0x00,0x00,0x00],// O
[0x00,0x00,0xFC,0x66,0x66,0x66,0x7C,0x60,0x60,0x60,0xF0,0x00,0x00,0x00,0x00,0x00],// P
[0x00,0x00,0x7C,0xC6,0xC6,0xC6,0xC6,0xD6,0xDE,0x7C,0x0C,0x0E,0x00,0x00,0x00,0x00],// Q
[0x00,0x00,0xFC,0x66,0x66,0x66,0x7C,0x6C,0x66,0x66,0xE6,0x00,0x00,0x00,0x00,0x00],// R
[0x00,0x00,0x7C,0xC6,0xC6,0x60,0x38,0x0C,0xC6,0xC6,0x7C,0x00,0x00,0x00,0x00,0x00],// S
[0x00,0x00,0x7E,0x7E,0x5A,0x18,0x18,0x18,0x18,0x18,0x3C,0x00,0x00,0x00,0x00,0x00],// T
[0x00,0x00,0xC6,0xC6,0xC6,0xC6,0xC6,0xC6,0xC6,0xC6,0x7C,0x00,0x00,0x00,0x00,0x00],// U
[0x00,0x00,0xC6,0xC6,0xC6,0xC6,0xC6,0xC6,0x6C,0x38,0x10,0x00,0x00,0x00,0x00,0x00],// V
[0x00,0x00,0xC6,0xC6,0xC6,0xC6,0xD6,0xD6,0xFE,0x6C,0x6C,0x00,0x00,0x00,0x00,0x00],// W
[0x00,0x00,0xC6,0xC6,0x6C,0x6C,0x38,0x6C,0x6C,0xC6,0xC6,0x00,0x00,0x00,0x00,0x00],// X
[0x00,0x00,0x66,0x66,0x66,0x66,0x3C,0x18,0x18,0x18,0x3C,0x00,0x00,0x00,0x00,0x00],// Y
[0x00,0x00,0xFE,0xC6,0x86,0x0C,0x18,0x30,0x62,0xC6,0xFE,0x00,0x00,0x00,0x00,0x00],// Z
[0x00,0x00,0x3C,0x30,0x30,0x30,0x30,0x30,0x30,0x30,0x3C,0x00,0x00,0x00,0x00,0x00],// [
[0x00,0x00,0x00,0x80,0xC0,0xE0,0x70,0x38,0x1C,0x0E,0x06,0x02,0x00,0x00,0x00,0x00],// backslash
[0x00,0x00,0x3C,0x0C,0x0C,0x0C,0x0C,0x0C,0x0C,0x0C,0x3C,0x00,0x00,0x00,0x00,0x00],// ]
[0x10,0x38,0x6C,0xC6,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],// ^
[0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0xFF,0x00,0x00],// _
[0x30,0x30,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],// `
[0x00,0x00,0x00,0x00,0x00,0x78,0x0C,0x7C,0xCC,0xCC,0x76,0x00,0x00,0x00,0x00,0x00],// a
[0x00,0x00,0xE0,0x60,0x60,0x78,0x6C,0x66,0x66,0x66,0xDC,0x00,0x00,0x00,0x00,0x00],// b
[0x00,0x00,0x00,0x00,0x00,0x7C,0xC6,0xC0,0xC0,0xC6,0x7C,0x00,0x00,0x00,0x00,0x00],// c
[0x00,0x00,0x1C,0x0C,0x0C,0x3C,0x6C,0xCC,0xCC,0xCC,0x76,0x00,0x00,0x00,0x00,0x00],// d
[0x00,0x00,0x00,0x00,0x00,0x7C,0xC6,0xFE,0xC0,0xC6,0x7C,0x00,0x00,0x00,0x00,0x00],// e
[0x00,0x00,0x1C,0x36,0x32,0x30,0x78,0x30,0x30,0x30,0x78,0x00,0x00,0x00,0x00,0x00],// f
[0x00,0x00,0x00,0x00,0x00,0x76,0xCC,0xCC,0xCC,0x7C,0x0C,0xCC,0x78,0x00,0x00,0x00],// g
[0x00,0x00,0xE0,0x60,0x60,0x6C,0x76,0x66,0x66,0x66,0xE6,0x00,0x00,0x00,0x00,0x00],// h
[0x00,0x00,0x18,0x18,0x00,0x38,0x18,0x18,0x18,0x18,0x3C,0x00,0x00,0x00,0x00,0x00],// i
[0x00,0x00,0x06,0x06,0x00,0x0E,0x06,0x06,0x06,0x06,0x66,0x66,0x3C,0x00,0x00,0x00],// j
[0x00,0x00,0xE0,0x60,0x60,0x66,0x6C,0x78,0x6C,0x66,0xE6,0x00,0x00,0x00,0x00,0x00],// k
[0x00,0x00,0x38,0x18,0x18,0x18,0x18,0x18,0x18,0x18,0x3C,0x00,0x00,0x00,0x00,0x00],// l
[0x00,0x00,0x00,0x00,0x00,0xEC,0xFE,0xD6,0xD6,0xD6,0xC6,0x00,0x00,0x00,0x00,0x00],// m
[0x00,0x00,0x00,0x00,0x00,0xDC,0x66,0x66,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00],// n
[0x00,0x00,0x00,0x00,0x00,0x7C,0xC6,0xC6,0xC6,0xC6,0x7C,0x00,0x00,0x00,0x00,0x00],// o
[0x00,0x00,0x00,0x00,0x00,0xDC,0x66,0x66,0x66,0x7C,0x60,0x60,0xF0,0x00,0x00,0x00],// p
[0x00,0x00,0x00,0x00,0x00,0x76,0xCC,0xCC,0xCC,0x7C,0x0C,0x0C,0x1E,0x00,0x00,0x00],// q
[0x00,0x00,0x00,0x00,0x00,0xDC,0x76,0x66,0x60,0x60,0xF0,0x00,0x00,0x00,0x00,0x00],// r
[0x00,0x00,0x00,0x00,0x00,0x7C,0xC6,0x70,0x1C,0xC6,0x7C,0x00,0x00,0x00,0x00,0x00],// s
[0x00,0x00,0x10,0x30,0x30,0xFC,0x30,0x30,0x30,0x36,0x1C,0x00,0x00,0x00,0x00,0x00],// t
[0x00,0x00,0x00,0x00,0x00,0xCC,0xCC,0xCC,0xCC,0xCC,0x76,0x00,0x00,0x00,0x00,0x00],// u
[0x00,0x00,0x00,0x00,0x00,0x66,0x66,0x66,0x66,0x3C,0x18,0x00,0x00,0x00,0x00,0x00],// v
[0x00,0x00,0x00,0x00,0x00,0xC6,0xC6,0xD6,0xD6,0xFE,0x6C,0x00,0x00,0x00,0x00,0x00],// w
[0x00,0x00,0x00,0x00,0x00,0xC6,0x6C,0x38,0x38,0x6C,0xC6,0x00,0x00,0x00,0x00,0x00],// x
[0x00,0x00,0x00,0x00,0x00,0xC6,0xC6,0xC6,0xC6,0x7E,0x06,0x0C,0xF8,0x00,0x00,0x00],// y
[0x00,0x00,0x00,0x00,0x00,0xFE,0xCC,0x18,0x30,0x66,0xFE,0x00,0x00,0x00,0x00,0x00],// z
[0x00,0x00,0x0E,0x18,0x18,0x18,0x70,0x18,0x18,0x18,0x0E,0x00,0x00,0x00,0x00,0x00],// {
[0x00,0x00,0x18,0x18,0x18,0x18,0x00,0x18,0x18,0x18,0x18,0x18,0x00,0x00,0x00,0x00],// |
[0x00,0x00,0x70,0x18,0x18,0x18,0x0E,0x18,0x18,0x18,0x70,0x00,0x00,0x00,0x00,0x00],// }
[0x00,0x00,0x76,0xDC,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],// ~
];

fn draw_char(fb: &mut Fb, ch: u8, x: i32, y: i32, c: Color) {
    if ch < 0x20 || ch > 0x7E { return; }
    let glyph = &FONT8X16[(ch - 0x20) as usize];
    for row in 0..16i32 {
        let bits = glyph[row as usize];
        for col in 0..8i32 {
            if bits & (0x80 >> col) != 0 {
                fb.pixel(x + col, y + row, c);
            }
        }
    }
}

fn draw_text(fb: &mut Fb, s: &str, x: i32, y: i32, c: Color) {
    for (i, ch) in s.bytes().enumerate() {
        draw_char(fb, ch, x + i as i32 * 8, y, c);
    }
}

fn text_width(s: &str) -> i32 { s.len() as i32 * 8 }

// ============================================================
//  BMP 图片加载器 (无外部依赖)
// ============================================================

struct BmpImage {
    w: u32, h: u32,
    data: &'static [u8],
    pixel_offset: usize,
    bpp: u16,
}

impl BmpImage {
    fn parse(raw: &'static [u8]) -> Option<Self> {
        if raw.len() < 54 { return None; }
        // .get() 写法规避 rust-analyzer 在 no_std UEFI 目标下无法解析 Index trait 的问题
        if raw.get(0).copied() != Some(b'B') || raw.get(1).copied() != Some(b'M') { return None; }
        let b = |off: usize| -> u8 { raw.get(off).copied().unwrap_or(0) };
        let read_u32 = |off: usize| u32::from_le_bytes([b(off), b(off+1), b(off+2), b(off+3)]);
        let read_u16 = |off: usize| u16::from_le_bytes([b(off), b(off+1)]);
        let pixel_offset = read_u32(10) as usize;
        let w = read_u32(18);
        let h = read_u32(22);
        let bpp = read_u16(28);
        if pixel_offset >= raw.len() || w == 0 || h == 0 { return None; }
        Some(Self { w, h, data: raw, pixel_offset, bpp })
    }

    fn pixel(&self, x: u32, y: u32) -> Color {
        if x >= self.w || y >= self.h { return Color::rgb(0, 0, 0); }
        let row = (self.h - 1 - y) as usize; // BMP 自底向上
        let bpp_bytes = (self.bpp as usize + 7) / 8;
        let row_size = ((self.w as usize * bpp_bytes + 3) / 4) * 4;
        let off = self.pixel_offset + row * row_size + x as usize * bpp_bytes;
        if off + bpp_bytes > self.data.len() { return Color::rgb(0, 0, 0); }
        let b = |i: usize| -> u8 { self.data.get(i).copied().unwrap_or(0) };
        match self.bpp {
            24 => Color { r: b(off+2), g: b(off+1), b: b(off), a: 255 },
            32 => Color { r: b(off+2), g: b(off+1), b: b(off), a: b(off+3) },
            _  => Color { r: 255, g: 0, b: 255, a: 255 }, // 不支持格式 → 紫色标记
        }
    }
}

fn draw_bmp(fb: &mut Fb, bmp: &BmpImage, x: i32, y: i32) {
    for by in 0..bmp.h {
        for bx in 0..bmp.w {
            let c = bmp.pixel(bx, by);
            if c.a > 0 {
                fb.pixel(x + bx as i32, y + by as i32, c);
            }
        }
    }
}

// 等比 cover 缩放铺满全屏: 保持宽高比, 放大到覆盖整个画面, 多余部分居中裁剪。
fn draw_bmp_cover(fb: &mut Fb, bmp: &BmpImage) {
    if bmp.w == 0 || bmp.h == 0 { return; }
    let sx = fb.w as f32 / bmp.w as f32;
    let sy = fb.h as f32 / bmp.h as f32;
    let scale = if sx > sy { sx } else { sy };
    let ox = ((fb.w as f32 - bmp.w as f32 * scale) * 0.5) as i32;
    let oy = ((fb.h as f32 - bmp.h as f32 * scale) * 0.5) as i32;

    for dy in 0..fb.h as i32 {
        for dx in 0..fb.w as i32 {
            let tx = ((dx - ox) as f32 / scale) as i32;
            let ty = ((dy - oy) as f32 / scale) as i32;
            if tx < 0 || ty < 0 || tx >= bmp.w as i32 || ty >= bmp.h as i32 { continue; }
            let c = bmp.pixel(tx as u32, ty as u32);
            if c.a > 0 {
                fb.pixel(dx, dy, c);
            }
        }
    }
}

// ============================================================
//  主题配置 (从 theme.toml 解析)
// ============================================================

struct ThemeConfig {
    menu_x: i32, menu_y: i32,
    menu_w: u32, menu_h: u32,
    menu_radius: u32,
    menu_alpha: f32,
    item_height: u32,
    spacing: u32,
    text_color: Color,
    hl_color: Color,
    hl_alpha: f32,
    hl_text_color: Color,
    timeout_enable: bool,
    timeout_seconds: u32,
    splash_duration: u64,
    animation_enabled: bool,
    blur_strength: u8,
    logo_margin_top: u32,
}

impl ThemeConfig {
    fn load() -> Self {
        let raw = include_str!("../loader/theme.toml");
        let mut cfg = Self::default();

        let mut section: &str = "";
        for line in raw.lines() {
            let trimmed = line.trim();
            // 跳过空行和注释
            if trimmed.is_empty() || trimmed.starts_with('#') { continue; }
            // 检测 section header [section] 或 [section.sub]
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                section = &trimmed[1..trimmed.len()-1];
                continue;
            }
            // 解析 key = value
            if let Some(eq) = trimmed.find('=') {
                let key = trimmed[..eq].trim().trim_matches('"');
                let val = trimmed[eq+1..].trim().trim_matches('"');
                cfg.apply(section, key, val);
            }
        }
        cfg
    }

    fn apply(&mut self, section: &str, key: &str, val: &str) {
        match (section, key) {
            ("render", "animation_enabled") => self.animation_enabled = val == "true",
            ("render", "acrylic_blur_strength") => self.blur_strength = parse_u8(val),
            ("menu", "width") => self.menu_w = parse_u32(val),
            ("menu", "height") => self.menu_h = parse_u32(val),
            ("menu", "radius") => self.menu_radius = parse_u32(val),
            ("menu", "alpha") => self.menu_alpha = parse_f32(val),
            ("menu", "item_height") => self.item_height = parse_u32(val),
            ("menu", "spacing") => self.spacing = parse_u32(val),
            ("menu", "text_color") => self.text_color = Color::from_hex(val).unwrap_or(self.text_color),
            ("menu.highlight", "color") => self.hl_color = Color::from_hex(val).unwrap_or(self.hl_color),
            ("menu.highlight", "alpha") => self.hl_alpha = parse_f32(val),
            ("menu.highlight", "text_color") => self.hl_text_color = Color::from_hex(val).unwrap_or(self.hl_text_color),
            ("timeout", "enable") => self.timeout_enable = val == "true",
            ("timeout", "default") => self.timeout_seconds = parse_u32(val),
            ("splash", "duration_ms") => self.splash_duration = parse_u32(val) as u64,
            ("logo", "margin_top") => self.logo_margin_top = parse_u32(val),
            _ => {}
        }
    }

    fn default() -> Self {
        Self {
            menu_w: 800, menu_h: 540, menu_radius: 16,
            menu_alpha: 0.85, item_height: 60, spacing: 8,
            text_color: Color::rgb(255,255,255),
            hl_color: Color::rgb(0x3A,0x6A,0xFF), hl_alpha: 0.9,
            hl_text_color: Color::rgb(255,255,255),
            timeout_enable: true, timeout_seconds: 5,
            splash_duration: 1200, animation_enabled: true, blur_strength: 3,
            menu_x: 0, menu_y: 0,
            logo_margin_top: 32,
        }
    }
}

fn parse_u32(s: &str) -> u32 { parse_u64(s) as u32 }
fn parse_u8(s: &str) -> u8 { parse_u64(s) as u8 }
fn parse_f32(s: &str) -> f32 {
    let mut result: f32 = 0.0;
    let mut decimal = false;
    let mut divisor: f32 = 1.0;
    for b in s.bytes() {
        match b {
            b'0'..=b'9' => {
                let d = (b - b'0') as f32;
                if decimal { divisor *= 10.0; result += d / divisor; }
                else { result = result * 10.0 + d; }
            }
            b'.' => { decimal = true; }
            _ => {}
        }
    }
    result
}
fn parse_u64(s: &str) -> u64 { s.parse::<u64>().unwrap_or(0) }

// ============================================================
//  菜单条目
// ============================================================

struct BootEntry<'a> {
    icon: &'a str,
    title: &'a str,
    info: &'a str,
}

// ============================================================
//  嵌入资源 (编译期打包)
// ============================================================
//
//  所有图片以 32-bit BGRA BMP 格式嵌入 (BI_RGB 无压缩)，
//  由 build 阶段从同名 PNG 自动生成。

// --- 桌面背景 (splash 背景铺满全屏) ---
static BG_BMP: &[u8] = include_bytes!("../loader/resources/splash/background.bmp");

// --- 系统 Logo (菜单右侧) ---
static SYSTEM_LOGO_BMP: &[u8] = include_bytes!("../loader/resources/logo/system.bmp");

// --- 滚动箭头 ---
static ARROW_UP_BMP:   &[u8] = include_bytes!("../loader/resources/icons/ui/arrow_up.bmp");
static ARROW_DOWN_BMP: &[u8] = include_bytes!("../loader/resources/icons/ui/arrow_down.bmp");

// --- 选中指示图标 ---
static SELECTED_BMP: &[u8] = include_bytes!("../loader/resources/icons/ui/selected.bmp");

// --- 对话框背景 ---
static DIALOG_BG_BMP: &[u8] = include_bytes!("../loader/resources/icons/dialog/bg.bmp");

// --- 加载动画帧 (6 帧 128x128) ---
static LOADING_FRAMES: [&[u8]; 6] = [
    include_bytes!("../loader/resources/animation/loading/frame_00.bmp"),
    include_bytes!("../loader/resources/animation/loading/frame_01.bmp"),
    include_bytes!("../loader/resources/animation/loading/frame_02.bmp"),
    include_bytes!("../loader/resources/animation/loading/frame_03.bmp"),
    include_bytes!("../loader/resources/animation/loading/frame_04.bmp"),
    include_bytes!("../loader/resources/animation/loading/frame_05.bmp"),
];

// 嵌入测试内核 ELF (编译时打包)
// Makefile 的 kernel 目标会把编译好的测试内核 ELF 拷贝到
// boot/loader/morion-kernel.elf, 再由这里 include_bytes! 嵌入。
static KERNEL_ELF: &[u8] = include_bytes!("../loader/morion-kernel.elf");


// Boot Info 结构 — 传递给内核
#[repr(C)]
struct BootInfo {
    magic: u32,
    version: u32,
    fb_addr: u64,
    fb_width: u32,
    fb_height: u32,
    fb_stride: u32,
    fb_bpp: u32,
}

fn boot_kernel(fb: &mut Fb) -> ! {
    // ELF 校验 — 如果不是真正的 ELF (缺少魔数 / 长度不足)，
    // 显示提示后循环休眠，不要破坏内存。
    let is_valid_elf = KERNEL_ELF.len() >= 64
        && KERNEL_ELF.get(0) == Some(&0x7F)
        && KERNEL_ELF.get(1) == Some(&b'E')
        && KERNEL_ELF.get(2) == Some(&b'L')
        && KERNEL_ELF.get(3) == Some(&b'F')
        && KERNEL_ELF.get(4) == Some(&2); // 64-bit class

    if !is_valid_elf {
        let msg = "Kernel image invalid — rebuild the kernel, then rebuild bootloader";
        draw_text(fb, msg, 40, 40, Color::rgb(0xFF, 0xCC, 0x66));
        let tip = "(expected a valid x86_64 ELF at boot/loader/morion-kernel.elf)";
        draw_text(fb, tip, 40, 60, Color::rgb(0xAA, 0xAA, 0xAA));
        loop { unsafe { core::arch::asm!("hlt", options(nomem, nostack)); } }
    }

    // 设置 Boot Info 到 0x7000
    let boot_info = BootInfo {
        magic: 0x4D4F5249, // "MORI"
        version: 1,
        fb_addr: fb.base as u64,
        fb_width: fb.w,
        fb_height: fb.h,
        fb_stride: fb.stride,
        fb_bpp: 32,
    };
    unsafe {
        let ptr = 0x7000 as *mut BootInfo;
        core::ptr::write(ptr, boot_info);
    }

    // 解析 ELF 并加载段
    let header = unsafe { &*(KERNEL_ELF.as_ptr() as *const Elf64Header) };
    let entry_point = header.entry;

    // 遍历程序头，加载段
    let phoff = header.phoff as usize;
    let phentsize = header.phentsize as usize;
    let phnum = header.phnum as usize;

    for i in 0..phnum {
        let ph_offset = phoff + i * phentsize;
        if ph_offset + 80 > KERNEL_ELF.len() { break; }
        let ph = unsafe { &*(KERNEL_ELF.as_ptr().add(ph_offset) as *const Elf64ProgramHeader) };

        if ph.ptype == 1 && ph.memsz > 0 {
            // PT_LOAD — 复制到物理地址
            let load_addr = ph.paddr;
            let file_size = ph.filesz as usize;
            let mem_size = ph.memsz as usize;

            if file_size > 0 {
                let file_start = ph.offset as usize;
                let file_end = file_start + file_size;
                if file_end <= KERNEL_ELF.len() {
                    unsafe {
                        let dst = core::slice::from_raw_parts_mut(load_addr as *mut u8, file_size);
                        dst.copy_from_slice(&KERNEL_ELF[file_start..file_end]);
                    }
                }
            }
            // 零填充 BSS
            if mem_size > file_size {
                unsafe {
                    core::ptr::write_bytes((load_addr + file_size as u64) as *mut u8, 0, mem_size - file_size);
                }
            }
        }
    }

    // 跳转到内核
    let kernel_entry: extern "C" fn() -> ! = unsafe { core::mem::transmute(entry_point) };
    kernel_entry();
}

// 顶栏: 左侧系统名称文字, 右侧系统 logo。
fn render_header(fb: &mut Fb, _theme: &ThemeConfig) {
    // 顶部淡色遮罩, 提升文字/logo 在亮背景上的可读性 (区间自适应高度)。
    let bar_h = 200u32.min(fb.h / 3);
    fb.fill_rect(Rect::new(0, 0, fb.w, bar_h), Color::rgba(0x05, 0x08, 0x12, 80));

    // 左侧: 系统名称 + 副标题
    draw_text(fb, "Morion OS", 48, 40, Color::rgb(0xFF, 0xFF, 0xFF));
    draw_text(fb, "Immutable Micro-OS Bootloader", 48, 64, Color::rgb(0x9A, 0xA6, 0xB8));

    // 菜单正上方中央: 系统 Logo (水平居中, 略靠上)
    if let Some(logo) = BmpImage::parse(SYSTEM_LOGO_BMP) {
        let lx = (fb.w as i32 - logo.w as i32) / 2;
        let ly = 12;
        draw_bmp(fb, &logo, lx, ly);
    }
}

// 整屏渲染: 背景 (cover) -> 顶栏 -> 菜单。每次选择变化都从背景重画,
// 避免半透明叠加 (double-blend) 导致面板越画越暗。
fn render_screen(fb: &mut Fb, theme: &ThemeConfig, entries: &[BootEntry], sel: usize) {
    if let Some(bg) = BmpImage::parse(BG_BMP) {
        draw_bmp_cover(fb, &bg);
    }
    render_header(fb, theme);
    render_menu(fb, theme, entries, sel);
}

fn render_menu(fb: &mut Fb, theme: &ThemeConfig, entries: &[BootEntry], sel: usize) {
    let tw = theme.menu_w;
    let th = theme.menu_h;
    let tx = theme.menu_x;
    let ty = theme.menu_y;

    // 亚克力毛玻璃效果: 菜单区域上层暗色铺底
    let overlay_a = (theme.menu_alpha * 0.3 * 255.0) as u8;
    fb.fill_rect(Rect::new(tx - 8, ty - 8, tw + 16, th + 16),
        Color::rgba(0x08, 0x10, 0x1E, overlay_a));

    // 菜单面板 — 半透明圆角
    let panel_c = Color::rgba(0x10, 0x15, 0x22, (theme.menu_alpha * 255.0) as u8);
    fb.fill_rounded(Rect::new(tx, ty, tw, th), theme.menu_radius, panel_c);

    // 标题
    let title = "Morion OS  Boot Manager";
    let title_w = text_width(title);
    draw_text(fb, title, tx + (tw as i32 - title_w)/2, ty + 18, theme.text_color);

    // 分割线
    let line_y = ty + 44;
    fb.hline(line_y, tx + 24, tx + tw as i32 - 24, Color::rgba(0x3A, 0x3A, 0x4A, 128));

    // 底部信息
    draw_text(fb, "v0.1.0  |  Acrylic Theme  |  Secure Boot", tx + 24, ty + th as i32 - 22,
        Color::rgb(0x88, 0x8A, 0x9A));
    let footer = "[ENTER]=Boot  [UP/DOWN]=Navigate  [ESC]=Reboot  [F10]=Shutdown";
    draw_text(fb, footer, tx + (tw as i32 - text_width(footer))/2, ty + th as i32 - 44,
        Color::rgb(0x77, 0x79, 0x8A));

    // 如果条目数量超出菜单显示区域, 绘制上下滚动箭头
    let visible_count = (th - 100) / (theme.item_height + theme.spacing);
    if entries.len() > visible_count as usize {
        if let Some(arrow_up) = BmpImage::parse(ARROW_UP_BMP) {
            let ax = tx + tw as i32 - arrow_up.w as i32 - 16;
            let ay = line_y + 8;
            draw_bmp(fb, &arrow_up, ax, ay);
        }
        if let Some(arrow_down) = BmpImage::parse(ARROW_DOWN_BMP) {
            let ax = tx + tw as i32 - arrow_down.w as i32 - 16;
            let ay = ty + th as i32 - arrow_down.h as i32 - 48;
            draw_bmp(fb, &arrow_down, ax, ay);
        }
    }

    // 渲染每个菜单项
    let item_start_y = ty + 58;
    for i in 0..entries.len() {
        let iy = item_start_y + i as i32 * (theme.item_height as i32 + theme.spacing as i32);
        let ir = Rect::new(tx + 16, iy, tw - 32, theme.item_height);

        if i == sel {
            let hl_c = Color::rgba(theme.hl_color.r, theme.hl_color.g, theme.hl_color.b, (theme.hl_alpha * 255.0) as u8);
            fb.fill_rounded(ir, 8, hl_c);
            // 选中项：在高亮左边绘制 selected 图标 (蓝色对勾/方形 32x32)
            if let Some(sel_bmp) = BmpImage::parse(SELECTED_BMP) {
                let ix = ir.x - 4;
                let iy_img = iy + (theme.item_height as i32 - sel_bmp.h as i32) / 2;
                draw_bmp(fb, &sel_bmp, ix, iy_img);
            }
        } else {
            // 非选中项：用面板底色覆盖（清除可能的高亮残留）
            fb.fill_rounded(ir, 8, panel_c);
        }

        let entry = &entries[i];
        let tc = if i == sel { theme.hl_text_color } else { theme.text_color };
        draw_text(fb, entry.icon, ir.x + 40, iy + (theme.item_height as i32 - 16)/2 + 2, tc);
        draw_text(fb, entry.title, ir.x + 72, iy + (theme.item_height as i32 - 16)/2 + 2, tc);
        let info_w = text_width(entry.info);
        draw_text(fb, entry.info, ir.x + ir.w as i32 - info_w - 16, iy + (theme.item_height as i32 - 16)/2 + 2,
            Color::rgb(0x88, 0x88, 0x88));
    }
}

// ============================================================
//  UEFI 入口点
// ============================================================

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! { loop { core::hint::spin_loop(); } }

#[entry]
fn efi_main(_image: uefi::Handle, mut st: SystemTable<Boot>) -> Status {
    // 先获取 stdin (需要 &mut st)，获取后立即转为裸指针释放借用
    let stdin_ptr: *mut uefi::proto::console::text::Input = st.stdin() as *mut _;
    let stdin = unsafe { &mut *stdin_ptr };
    // 然后再获取 boot_services (只需要 &st)
    let bs = st.boot_services();

    // 1. 初始化 GOP
    let mut fb = match Fb::from_gop(bs) { Ok(f) => f, Err(_) => return Status::UNSUPPORTED };

    // 2. 加载主题
    let mut theme = ThemeConfig::load();
    theme.menu_x = (fb.w - theme.menu_w) as i32 / 2;
    theme.menu_y = (fb.h - theme.menu_h) as i32 / 2;

    // 3. 引导条目
    let entries = [
        BootEntry { icon: "[*]", title: "Morion OS  (current generation)", info: "Gen 42" },
        BootEntry { icon: "[ ]", title: "Morion OS  gen 41  (rollback)",    info: "Gen 41" },
        BootEntry { icon: "[R]", title: "Rescue Mode",                       info: "recovery" },
        BootEntry { icon: "[F]", title: "UEFI Firmware Settings",             info: "setup" },
    ];

    let mut sel: usize = 0;

    // 4. 主循环
    // 初始整屏渲染 (背景 + 顶栏 + 菜单)
    render_screen(&mut fb, &theme, &entries[..], sel);

    loop {
        let prev_sel = sel;

        // 键盘
        if let Ok(Some(key)) = stdin.read_key() {
            use uefi::proto::console::text::{Key, ScanCode};
            match key {
                Key::Special(ScanCode::UP) => { sel = sel.saturating_sub(1); }
                Key::Special(ScanCode::DOWN) => { sel = (sel + 1).min(entries.len() - 1); }
                Key::Special(ScanCode::HOME) => { sel = 0; }
                Key::Special(ScanCode::END) => { sel = entries.len() - 1; }
                Key::Printable(c) if c == uefi::Char16::try_from('\r').unwrap() => {
                    if sel == 0 {
                        draw_text(&mut fb, "Loading kernel...", theme.menu_x + 24, theme.menu_y + theme.menu_h as i32 - 64, Color::rgb(0, 255, 0));
                        boot_kernel(&mut fb);
                    } else {
                        draw_text(&mut fb, "Not implemented", theme.menu_x + 24, theme.menu_y + theme.menu_h as i32 - 64, Color::rgb(0, 255, 0));
                        for _ in 0..50_000_000 { core::hint::spin_loop(); }
                    }
                }
                Key::Special(ScanCode::ESCAPE) => { return Status::SUCCESS; }
                Key::Special(ScanCode::FUNCTION_10) => { return Status::SUCCESS; }
                _ => {}
            }
        }

        // 选择变化时整屏重绘 (背景 + 顶栏 + 菜单)
        if sel != prev_sel {
            render_screen(&mut fb, &theme, &entries[..], sel);
        }

        // 延时减少 CPU 占用
        for _ in 0..500_000 { core::hint::spin_loop(); }
    }
}
