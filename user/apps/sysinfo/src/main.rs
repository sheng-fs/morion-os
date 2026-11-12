//! 只读系统信息面板。
//!
//! - `uname` / release / build 来自现有 `SYS_UNAME`；
//! - uid / gid 来自 `SYS_CRED`（[`morion::syscall::sys_cred_get`]）；
//! - 存活域数与空闲物理帧来自 `SYS_DOMAIN_COUNT` / `SYS_FRAME_FREE`；
//! - 卷信息是 **df 风格**的：MorionFS 有 `MFS_STAT`（总块 / 空闲块），
//!   其余文件服务没有容量接口，故只探测挂载点是否可打开，并明确标注「无容量 API」；
//! - `r` 刷新快照，`q` / `Esc` 退出。本应用不写任何文件。
//!
//! 没有新增 syscall —— 全部走 libmorion 已有的封装。

#![no_std]
#![no_main]

use morion::gfx::{self, AppWindow, Event, Key, Rect, Theme, CHAR_HEIGHT};
use morion::syscall::{
    domain_id, println, sys_cred_get, sys_domain_count, sys_frame_free, sys_uname,
};
use morion::vfs;

// 窗口表面必须落在 libmorion 的单表面上限内（`SURFACE_MAX_PAGES` = 256 页 = 1 MiB）:
// 700 × 380 × 4 B = 1_064_000 B (260 页) —— 超限！故取 680 × 380 = 1_033_600 B (253 页)。
const WIN_W: u32 = 680;
const WIN_H: u32 = 380;
const TITLE_H: u32 = 24;
const STATUS_H: u32 = 20;
const PAD: u32 = 8;
/// 每行文字占用的高度（字符高 + 行距）。
const LINE_H: u32 = CHAR_HEIGHT + 4;
const MAX_LINES: usize = 13;
const LINE_MAX: usize = 100;

#[no_mangle]
pub extern "C" fn morion_main(_domain_id: u64) {
    let mut app = match AppWindow::open(80, 60, WIN_W, WIN_H) {
        Some(a) => a,
        None => {
            println("sysinfo: 建窗失败 (gfx_srv 不可用?)");
            return;
        }
    };
    let th = Theme::DEFAULT;
    let mut lines = collect();

    app.request_repaint(full_rect());
    loop {
        match app.wait_event() {
            Event::Repaint(_) => {
                draw(&app, &lines, &th);
                if !app.present() {
                    break;
                }
            }
            Event::Key(key) => match key {
                Key::Char(b'r') | Key::Char(b'R') => {
                    lines = collect();
                    app.request_repaint(full_rect());
                }
                Key::Char(b'q') | Key::Char(b'Q') | Key::Quit | Key::Escape => break,
                _ => {}
            },
            Event::Mouse(_) | Event::Focus(_) => {}
        }
    }
    app.close();
}

/// 一行固定上限的文本缓冲。
#[derive(Clone, Copy)]
struct Line {
    buf: [u8; LINE_MAX],
    len: usize,
}

impl Line {
    const fn new() -> Line {
        Line {
            buf: [0; LINE_MAX],
            len: 0,
        }
    }

    fn push_str(&mut self, s: &str) -> &mut Line {
        for &b in s.as_bytes() {
            if self.len < LINE_MAX {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
        self
    }

    fn push_u64(&mut self, mut v: u64) -> &mut Line {
        let mut tmp = [0u8; 20];
        let mut i = tmp.len();
        loop {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        for &b in &tmp[i..] {
            if self.len < LINE_MAX {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
        self
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("?")
    }
}

/// 采集一次快照。
fn collect() -> [Line; MAX_LINES] {
    let mut lines = [Line::new(); MAX_LINES];

    let mut uname = [0u8; 64];
    let uname_len = sys_uname(&mut uname, 0) as usize;
    let mut release = [0u8; 32];
    let release_len = sys_uname(&mut release, 1) as usize;
    let mut build = [0u8; 32];
    let build_len = sys_uname(&mut build, 2) as usize;
    let (uid, gid) = sys_cred_get().unwrap_or((u16::MAX, u16::MAX));
    let frames = sys_frame_free();
    let free_mib = frames.saturating_mul(4096) / (1024 * 1024);
    let domains = sys_domain_count();

    lines[0].push_str("MorionOS system information");
    lines[1]
        .push_str("uname   : ")
        .push_str(text_of(&uname[..uname_len.min(uname.len())]));
    lines[2]
        .push_str("release : ")
        .push_str(text_of(&release[..release_len.min(release.len())]));
    lines[3]
        .push_str("build   : ")
        .push_str(text_of(&build[..build_len.min(build.len())]));
    lines[4]
        .push_str("domain  : ")
        .push_u64(domain_id())
        .push_str("   uid=")
        .push_u64(uid as u64)
        .push_str(" gid=")
        .push_u64(gid as u64);
    lines[5]
        .push_str("domains : ")
        .push_u64(domains)
        .push_str(" alive");
    lines[6]
        .push_str("memory  : ")
        .push_u64(free_mib)
        .push_str(" MiB free (")
        .push_u64(frames)
        .push_str(" frames of 4 KiB)");

    lines[8].push_str("volumes:");
    lines[9]
        .push_str("  /     FAT32   ")
        .push_str(if mounted("/") {
            "mounted"
        } else {
            "unavailable"
        });
    lines[10]
        .push_str("  /tmp  tmpfs   ")
        .push_str(if mounted("/tmp") {
            "mounted"
        } else {
            "unavailable"
        });

    let usage = vfs::mfs_stat();
    lines[11].push_str("  /mfs  MFS     ");
    if usage == u64::MAX {
        lines[11].push_str("unavailable (MorionFS not mounted?)");
    } else {
        let total = usage >> 32;
        let free = usage & 0xFFFF_FFFF;
        let used = total.saturating_sub(free);
        let pct = (used * 100).checked_div(total).unwrap_or(0);
        lines[11]
            .push_str("total ")
            .push_u64(total)
            .push_str("  used ")
            .push_u64(used)
            .push_str("  free ")
            .push_u64(free)
            .push_str("  (")
            .push_u64(pct)
            .push_str("% used)");
    }
    lines[12].push_str("note    : 1 MFS block = 4 KiB; other filesystems expose no capacity API");

    lines
}

/// 以只读方式探测挂载点是否可打开（打开后立刻关闭）。
fn mounted(path: &str) -> bool {
    let fd = vfs::open(path);
    if fd == u64::MAX {
        return false;
    }
    vfs::close(fd);
    true
}

fn text_of(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("?")
}

fn full_rect() -> Rect {
    Rect::new(0, 0, WIN_W, WIN_H)
}

fn title_rect() -> Rect {
    Rect::new(0, 0, WIN_W, TITLE_H)
}

fn body_rect() -> Rect {
    Rect::new(0, TITLE_H, WIN_W, WIN_H - TITLE_H - STATUS_H)
}

fn status_rect() -> Rect {
    Rect::new(0, WIN_H - STATUS_H, WIN_W, STATUS_H)
}

/// 把整个界面重画进窗口表面（上屏由脏区决定）。
fn draw(app: &AppWindow, lines: &[Line; MAX_LINES], th: &Theme) {
    let s = app.surface();
    s.fill(th.window);

    gfx::draw_panel(s, title_rect(), th.panel, th.dim);
    s.draw_text(PAD, 4, "sysinfo   (read-only)", th.fg, None);

    gfx::draw_panel(s, body_rect(), th.panel, th.dim);
    let x = PAD + 4;
    let max_w = WIN_W - x - PAD;
    for (i, line) in lines.iter().enumerate() {
        let y = TITLE_H + PAD + i as u32 * LINE_H;
        if y + CHAR_HEIGHT > WIN_H - STATUS_H {
            break;
        }
        let color = match i {
            0 => th.accent_fg,
            8 => th.accent,
            12 => th.dim,
            _ => th.fg,
        };
        s.draw_text_limited(x, y, max_w, line.as_str(), color, None);
    }

    gfx::draw_panel(s, status_rect(), th.panel, th.dim);
    s.draw_text(
        PAD,
        WIN_H - STATUS_H + 2,
        "r refresh   q / Esc quit",
        th.dim,
        None,
    );
}
