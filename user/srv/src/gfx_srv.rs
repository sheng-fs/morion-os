//! 域 15 — gfx_srv (图形服务, G1 / G2)
//!
//! 内核把**帧缓冲**交给用户态: 本服务持 `Capability::Fb`, 先用 `SYS_FB_INFO` 取几何、
//! `SYS_FB_MAP` 把整块帧缓冲映射进自己的地址空间, 再用 `SYS_FB_TAKEOVER` 宣告接管显示 ——
//! 之后内核终端不再写帧缓冲 (它的输出只保留 COM1), 屏幕由本服务负责。
//!
//! G1 起屏 (取几何 → 映射 → 画测试图案 → 回读校验 → 接管), G2 起在这里服务**绘图请求**:
//! 客户端 ([`morion::gfx`]) 把一块表面同址共享过来, 发 `SYS_CALL`; 本服务把表面拷到帧缓冲
//! (blit) 并**回读校验**, 校验通过才回 1 —— 于是"画上去了"这件事在无显示器时也可断言。
//!
//! 文本渲染 / 输入行服务化见 [docs/roadmap-gfx.md](../../docs/roadmap-gfx.md) 的 G3 / G4。

use crate::common::Message;
use morion::gfx::{GfxReq, GFX_OP_BLIT, GFX_OP_FILL, GFX_OP_PING, GFX_OP_RECT, GFX_TAG};
use morion::syscall::*;

/// 帧缓冲在本域的映射基址: `USER_SPACE_BASE + 1 GiB`。
///
/// 放这么高是因为程序镜像、共享缓冲、用户栈都挤在 `USER_SPACE_BASE` 起的头几 MiB 内,
/// 而用户空间有 512 GiB —— 挪到 1 GiB 处远离它们, 帧缓冲再大也撞不上。
const FB_VADDR: u64 = 0x0000_0080_4000_0000;

/// 背景色 (深蓝黑)。
const BG: u32 = 0x10_18_28;
/// 前景色块 (琥珀)。
const FG: u32 = 0xE0_A0_30;

/// 域 15 入口: 取几何 → 映射 → 画图 + 回读校验 → 接管 → 重画, 然后常驻。
pub fn run() {
    println("gfx: starting (framebuffer takeover)");

    let mut info = FbInfo::default();
    if sys_fb_info(&mut info) != 1 {
        println("gfx: SYS_FB_INFO FAILED (missing Capability::Fb?)");
        return;
    }
    // 目前只按 32bpp 线性帧缓冲处理 (与内核 `Framebuffer` 的写入口径一致)。
    if info.addr == 0 || info.width == 0 || info.height == 0 || info.bpp != 32 {
        print("gfx: unsupported framebuffer bpp=");
        print_u64(info.bpp as u64);
        println("");
        return;
    }
    print("gfx: framebuffer ");
    print_u64(info.width as u64);
    print("x");
    print_u64(info.height as u64);
    print(" stride=");
    print_u64(info.stride as u64);
    print(" phys=0x");
    print_hex(info.addr);
    println("");

    if sys_fb_map(FB_VADDR) != 1 {
        println("gfx: SYS_FB_MAP FAILED");
        return;
    }
    let fb = Fb {
        base: FB_VADDR,
        width: info.width,
        height: info.height,
        stride: info.stride,
    };

    // 先画一遍并**回读校验**: 证明映射确实可写、几何算得对 (回读值必须等于写入值)。
    paint(&fb);
    if !verify(&fb) {
        println("gfx: framebuffer readback FAILED");
        return;
    }

    // 校验通过才宣告接管 —— 此刻起内核终端不再写屏, 屏幕只由本服务负责。
    if sys_fb_takeover() != 1 {
        println("gfx: SYS_FB_TAKEOVER FAILED");
        return;
    }

    // 接管后再画一次: 这一次的内容不会被内核日志覆盖。
    paint(&fb);
    println("gfx: framebuffer takeover OK (kernel console detached)");

    // G2: 服务绘图请求。注意本服务的请求循环在**接管之后**才启动, 故任何客户端的第一条
    // 请求都必然在"屏幕已归用户态"之后被处理 —— 客户端无需自己等待接管完成。
    println("gfx: ready for draw requests");
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        let reply = handle(&fb, msg.tag, msg.payload.as_ptr());
        sys_reply(reply);
    }
}

/// 处理一条绘图请求, 返回回复值 (`1` = 成功, 其它 = 失败)。
fn handle(fb: &Fb, tag: u64, payload: *const u8) -> u64 {
    if tag != GFX_TAG {
        return u64::MAX;
    }
    let req: GfxReq = unsafe { core::ptr::read_unaligned(payload as *const GfxReq) };
    // 坐标为 u64, 一律先夹到 u32 可表示的范围 (屏幕坐标本就很小), 免得截断成怪值。
    if req.x > u32::MAX as u64
        || req.y > u32::MAX as u64
        || req.w > u32::MAX as u64
        || req.h > u32::MAX as u64
    {
        return 0;
    }
    match req.op {
        GFX_OP_FILL => {
            fb.fill(req.color as u32);
            1
        }
        GFX_OP_RECT => {
            fb.rect(
                req.x as u32,
                req.y as u32,
                req.w as u32,
                req.h as u32,
                req.color as u32,
            );
            1
        }
        GFX_OP_BLIT => blit_to_screen(fb, &req),
        GFX_OP_PING => 1,
        _ => 0,
    }
}

/// 把客户端表面拷到屏幕并**回读校验**; 成功返回 1。
///
/// 表面由客户端 `SYS_SHARE_PAGE` **同址**共享过来, 故这里可直接按 `req.buf` 读。读之前先
/// 确认首尾像素**落在本域已映射的页里** (`SYS_VIRT_TO_PHYS` 反映的是调用方 = 本域的映射) ——
/// 否则一个没共享过来的地址会让服务在读像素时缺页。
fn blit_to_screen(fb: &Fb, req: &GfxReq) -> u64 {
    let (sw, sh, sstride) = (req.w, req.h, req.stride);
    if req.buf == 0 || sw == 0 || sh == 0 || sstride == 0 {
        return 0;
    }
    let src = req.buf;
    let last = src + (sh - 1) * sstride * 4 + (sw - 1) * 4;
    if sys_virt_to_phys(src) == 0 || sys_virt_to_phys(last) == 0 {
        return 0;
    }

    let mut drawn = 0u64;
    for sy in 0..sh {
        let dy = req.y + sy;
        if dy >= fb.height as u64 {
            break;
        }
        for sx in 0..sw {
            let dx = req.x + sx;
            if dx >= fb.width as u64 {
                break;
            }
            let off = (sy * sstride + sx) * 4;
            let color = unsafe { *((src + off) as *const u32) } & 0x00FF_FFFF;
            fb.pixel(dx as u32, dy as u32, color);
            drawn += 1;
        }
    }
    if drawn == 0 {
        return 0;
    }
    verify_blit(fb, req, src)
}

/// 回读校验: 抽 5 个点, 屏幕上的像素必须等于表面里的像素 (证明确实"画上去了")。
fn verify_blit(fb: &Fb, req: &GfxReq, src: u64) -> u64 {
    let (sw, sh, sstride) = (req.w, req.h, req.stride);
    let samples = [
        (0, 0),
        (sw - 1, 0),
        (0, sh - 1),
        (sw - 1, sh - 1),
        (sw / 2, sh / 2),
    ];
    for (sx, sy) in samples {
        let dx = req.x + sx;
        let dy = req.y + sy;
        if dx >= fb.width as u64 || dy >= fb.height as u64 {
            continue; // 被屏幕裁剪掉的采样点跳过
        }
        let off = (sy * sstride + sx) * 4;
        let want = unsafe { *((src + off) as *const u32) } & 0x00FF_FFFF;
        if fb.read(dx as u32, dy as u32) != want {
            return 0;
        }
    }
    1
}

/// 帧缓冲视角 (线性 BGRA, 每像素 4 字节, `stride` 单位像素)。
struct Fb {
    base: u64,
    width: u32,
    height: u32,
    stride: u32,
}

impl Fb {
    /// 写一个像素 (`color` 为 `0x00RRGGBB`; 高字节固定 `0xFF` 不透明)。
    fn pixel(&self, x: u32, y: u32, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let off = (y as u64 * self.stride as u64 + x as u64) * 4;
        unsafe {
            *((self.base + off) as *mut u32) = 0xFF00_0000 | (color & 0x00FF_FFFF);
        }
    }

    /// 读一个像素 (只取低 24 位 RGB)。
    fn read(&self, x: u32, y: u32) -> u32 {
        let off = (y as u64 * self.stride as u64 + x as u64) * 4;
        unsafe { *((self.base + off) as *const u32) & 0x00FF_FFFF }
    }

    fn fill(&self, color: u32) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.pixel(x, y, color);
            }
        }
    }

    fn rect(&self, x0: u32, y0: u32, w: u32, h: u32, color: u32) {
        for y in y0..y0.saturating_add(h) {
            for x in x0..x0.saturating_add(w) {
                self.pixel(x, y, color);
            }
        }
    }
}

/// 画测试图案: 全屏底色 + 居中色块。
fn paint(fb: &Fb) {
    fb.fill(BG);
    let w = fb.width / 3;
    let h = fb.height / 3;
    fb.rect((fb.width - w) / 2, (fb.height - h) / 2, w, h, FG);
}

/// 回读校验: 四角仍是背景色、正中是前景色块。
fn verify(fb: &Fb) -> bool {
    let (w, h) = (fb.width, fb.height);
    fb.read(0, 0) == BG
        && fb.read(w - 1, 0) == BG
        && fb.read(0, h - 1) == BG
        && fb.read(w - 1, h - 1) == BG
        && fb.read(w / 2, h / 2) == FG
}
