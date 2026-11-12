//! 双栏文件列表 — 列表控件 + VFS 目录遍历的示例骨架（第二批接线，不参与本批镜像构建）。
//!
//! - 左栏从 `/`（FAT32）开始，右栏从 `/mfs` 开始；
//! - `Tab` 切换活动栏，`j` / `k` 移动选中项，`Enter` 进入目录，`Backspace` 返回上级，
//!   `r` 重新列目录，`q` / `Esc` 退出；
//! - 绘制只用 `morion::gfx`（[`gfx::draw_list`] / 按钮 / [`AppWindow`] 事件循环），
//!   目录读取只用 `morion::vfs`（`open` + `readdir` + `close`）。
//!
//! 更新走**脏矩形**：移动选中项只重铺新旧两行（换页时重铺整个列表），
//! 换目录才重铺对应栏。

#![no_std]
#![no_main]

use morion::gfx::{
    self, AppWindow, ButtonState, Event, Key, MouseKind, Point, Rect, Theme, LIST_ROW_HEIGHT,
};
use morion::syscall::{println, sys_alloc_page, sys_share_page, sys_virt_to_phys};
use morion::vfs;

// 窗口表面必须落在 libmorion 的单表面上限内（`SURFACE_MAX_PAGES` = 256 页 = 1 MiB）:
// 760 × 340 × 4 B = 1_033_600 B (253 页)，超出会 `Window::create` 返回 None。
const WIN_W: u32 = 760;
const WIN_H: u32 = 340;
const TITLE_H: u32 = 24;
const STATUS_H: u32 = 20;
const GAP: u32 = 8;
const PANE_W: u32 = (WIN_W - GAP * 3) / 2;
const PANE_H: u32 = WIN_H - TITLE_H - STATUS_H;
const HEADER_H: u32 = 22;

/// 单栏最多显示的条目数（一页结果也放得下）。
const MAX_ENTRIES: usize = 32;
/// 单条名字的最大字节数。
const NAME_MAX: usize = 64;
/// 路径最大字节数。
const PATH_MAX: usize = 96;

/// 目录树可能落到的文件服务域（与内核建域顺序一致）：FAT32 `/`、tmpfs `/tmp`、MFS `/mfs`。
const FAT32_DOMAIN: u64 = 6;
const TMPFS_DOMAIN: u64 = 10;
const MFS_DOMAIN: u64 = 11;

/// 本应用专用的 VFS 结果页。
///
/// 必须与 app（`0x...104000`）、shell（`0x...106000`）、hello（`0x...10C000`）以及
/// editor（`0x...110000`）错开：`SYS_SHARE_PAGE` 按同一虚拟地址映射进服务域，
/// 撞地址会让内核 `PageAlreadyMapped` panic。
const BUF_RESULT: u64 = 0x0000_0080_0011_2000;

#[no_mangle]
pub extern "C" fn morion_main(_domain_id: u64) {
    if !setup_buffers() {
        println("files: VFS 缓冲页共享失败");
        return;
    }
    let mut app = match AppWindow::open(60, 60, WIN_W, WIN_H) {
        Some(a) => a,
        None => {
            println("files: 建窗失败 (gfx_srv 不可用?)");
            return;
        }
    };
    let th = Theme::DEFAULT;
    let mut panes = [Pane::new("/"), Pane::new("/mfs")];
    panes[0].load();
    panes[1].load();
    let mut active = 0usize;
    let mut status = "READY";

    app.request_repaint(Rect::new(0, 0, WIN_W, WIN_H));
    loop {
        match app.wait_event() {
            Event::Repaint(_) => {
                draw(&app, &panes, active, status, &th);
                if !app.present() {
                    break;
                }
            }
            Event::Key(key) => match key {
                Key::Tab => {
                    active = 1 - active;
                    status = if active == 0 { "LEFT" } else { "RIGHT" };
                    app.request_repaint(Rect::new(0, 0, WIN_W, WIN_H));
                }
                Key::Char(b'j') | Key::Down | Key::PageDown => {
                    let old = panes[active].selected;
                    let old_top = panes[active].top;
                    panes[active].move_sel(true, list_rows());
                    let d = list_damage(active, old, old_top, &panes[active]);
                    app.request_repaint(d.union(status_rect()));
                }
                Key::Char(b'k') | Key::Up | Key::PageUp => {
                    let old = panes[active].selected;
                    let old_top = panes[active].top;
                    panes[active].move_sel(false, list_rows());
                    let d = list_damage(active, old, old_top, &panes[active]);
                    app.request_repaint(d.union(status_rect()));
                }
                Key::Enter => {
                    if panes[active].enter() {
                        status = "OPEN";
                    } else {
                        status = "NOT A DIRECTORY";
                    }
                    app.request_repaint(pane_rect(active).union(status_rect()));
                }
                Key::Backspace => {
                    panes[active].pop_path();
                    status = "UP";
                    app.request_repaint(pane_rect(active).union(status_rect()));
                }
                Key::Char(b'r') => {
                    panes[active].load();
                    status = "RELOAD";
                    app.request_repaint(pane_rect(active).union(status_rect()));
                }
                Key::Char(b'q') | Key::Quit | Key::Escape => break,
                _ => {}
            },
            Event::Mouse(m) => {
                if m.kind == MouseKind::Down {
                    let p = Point::new(m.x, m.y);
                    for (i, pane) in panes.iter_mut().enumerate() {
                        if gfx::button_hit(up_button(i), p) {
                            pane.pop_path();
                            active = i;
                            status = "UP";
                            app.request_repaint(pane_rect(i).union(status_rect()));
                        } else if let Some(idx) =
                            gfx::list_hit(list_rect(i), pane.top, pane.count, p)
                        {
                            let old = pane.selected;
                            pane.selected = idx;
                            active = i;
                            app.request_repaint(
                                row_band(i, old, pane)
                                    .union(row_band(i, idx, pane))
                                    .union(status_rect()),
                            );
                        }
                    }
                }
            }
            Event::Focus(_) => {}
        }
    }
    app.close();
}

/// 分栏。
struct Pane {
    path: [u8; PATH_MAX],
    path_len: usize,
    names: [[u8; NAME_MAX]; MAX_ENTRIES],
    name_len: [u8; MAX_ENTRIES],
    is_dir: [bool; MAX_ENTRIES],
    count: usize,
    selected: usize,
    top: usize,
    error: bool,
}

impl Pane {
    fn new(path: &str) -> Pane {
        let mut p = Pane {
            path: [0; PATH_MAX],
            path_len: 0,
            names: [[0; NAME_MAX]; MAX_ENTRIES],
            name_len: [0; MAX_ENTRIES],
            is_dir: [false; MAX_ENTRIES],
            count: 0,
            selected: 0,
            top: 0,
            error: false,
        };
        let n = path.len().min(PATH_MAX);
        p.path[..n].copy_from_slice(&path.as_bytes()[..n]);
        p.path_len = n;
        p
    }

    fn path_str(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.path[..self.path_len]) }
    }

    fn name(&self, i: usize) -> &str {
        let n = self.name_len[i] as usize;
        unsafe { core::str::from_utf8_unchecked(&self.names[i][..n]) }
    }

    /// 重新读取当前目录（失败置 `error`，保留空列表）。
    fn load(&mut self) {
        self.count = 0;
        self.selected = 0;
        self.top = 0;
        self.error = false;
        let fd = vfs::open(self.path_str());
        if fd == u64::MAX {
            self.error = true;
            return;
        }
        let bytes = vfs::readdir_into(fd, BUF_RESULT);
        vfs::close(fd);
        if bytes == u64::MAX {
            self.error = true;
            return;
        }
        let ent = core::mem::size_of::<vfs::DirEntry>();
        let total = (bytes as usize) / ent;
        let take = total.min(MAX_ENTRIES);
        for i in 0..take {
            let e =
                unsafe { core::ptr::read_unaligned((BUF_RESULT as *const vfs::DirEntry).add(i)) };
            let n = {
                let dst = &mut self.names[i];
                copy_name(&e, dst)
            };
            self.name_len[i] = n as u8;
            self.is_dir[i] = e.is_dir == 1;
        }
        self.count = take;
    }

    /// 上下移动选中项，并让选中行保持在可视区。
    fn move_sel(&mut self, down: bool, rows: usize) {
        if self.count == 0 {
            return;
        }
        if down {
            if self.selected + 1 < self.count {
                self.selected += 1;
            }
        } else if self.selected > 0 {
            self.selected -= 1;
        }
        if self.selected < self.top {
            self.top = self.selected;
        }
        if rows > 0 && self.selected >= self.top + rows {
            self.top = self.selected + 1 - rows;
        }
    }

    /// 进入选中的目录；不是目录 / 空列表返回 `false`。
    fn enter(&mut self) -> bool {
        if self.selected >= self.count || !self.is_dir[self.selected] {
            return false;
        }
        let n = self.name_len[self.selected] as usize;
        let mut name = [0u8; NAME_MAX];
        name[..n].copy_from_slice(&self.names[self.selected][..n]);
        self.push_path(&name[..n]);
        self.load();
        true
    }

    fn push_path(&mut self, name: &[u8]) {
        let mut n = self.path_len;
        if (n == 0 || self.path[n - 1] != b'/') && n < PATH_MAX {
            self.path[n] = b'/';
            n += 1;
        }
        for &b in name {
            if n >= PATH_MAX {
                break;
            }
            self.path[n] = b;
            n += 1;
        }
        self.path_len = n;
    }

    fn pop_path(&mut self) {
        if self.path_len <= 1 {
            return;
        }
        let mut i = self.path_len - 1;
        while i > 0 && self.path[i] != b'/' {
            i -= 1;
        }
        self.path_len = if i == 0 { 1 } else { i };
        self.load();
    }
}

/// 把 `DirEntry` 的名字（长名优先，否则 8.3）拷进 `dst`，返回字节数；非 ASCII 换 `?`。
fn copy_name(e: &vfs::DirEntry, dst: &mut [u8]) -> usize {
    if e.long_len > 0 {
        let n = (e.long_len as usize).min(dst.len());
        for (i, &b) in e.long[..n].iter().enumerate() {
            dst[i] = sanitize(b);
        }
        return n;
    }
    let mut n = 0usize;
    for &b in &e.name[..8] {
        if b == b' ' || n >= dst.len() {
            break;
        }
        dst[n] = sanitize(b);
        n += 1;
    }
    let ext_len = e.name[8..11].iter().take_while(|&&b| b != b' ').count();
    if ext_len > 0 && n < dst.len() {
        dst[n] = b'.';
        n += 1;
    }
    for &b in &e.name[8..8 + ext_len] {
        if n >= dst.len() {
            break;
        }
        dst[n] = sanitize(b);
        n += 1;
    }
    n
}

fn sanitize(b: u8) -> u8 {
    if (0x20..=0x7E).contains(&b) {
        b
    } else {
        b'?'
    }
}

/// 分配结果页，并共享给可能被浏览的三个文件服务。
fn setup_buffers() -> bool {
    if sys_virt_to_phys(BUF_RESULT) == 0 && sys_alloc_page(BUF_RESULT) != 1 {
        return false;
    }
    for domain in [FAT32_DOMAIN, TMPFS_DOMAIN, MFS_DOMAIN] {
        if sys_share_page(BUF_RESULT, domain) != 1 {
            return false;
        }
    }
    true
}

fn title_rect() -> Rect {
    Rect::new(0, 0, WIN_W, TITLE_H)
}

fn status_rect() -> Rect {
    Rect::new(0, WIN_H - STATUS_H, WIN_W, STATUS_H)
}

fn pane_rect(i: usize) -> Rect {
    Rect::new(GAP + i as u32 * (PANE_W + GAP), TITLE_H, PANE_W, PANE_H)
}

fn list_rect(i: usize) -> Rect {
    let p = pane_rect(i);
    Rect::new(
        p.x + 1,
        p.y + HEADER_H,
        p.w.saturating_sub(2),
        p.h.saturating_sub(HEADER_H + 1),
    )
}

fn up_button(i: usize) -> Rect {
    let p = pane_rect(i);
    Rect::new(p.x + p.w - 50, p.y + 2, 46, HEADER_H - 4)
}

fn list_rows() -> usize {
    (list_rect(0).h / LIST_ROW_HEIGHT) as usize
}

/// 某一行的窗口内矩形（不可见时为空）。
fn row_band(pane: usize, idx: usize, p: &Pane) -> Rect {
    if idx < p.top {
        return Rect::EMPTY;
    }
    let row = idx - p.top;
    if row as u32 >= list_rect(pane).h / LIST_ROW_HEIGHT {
        return Rect::EMPTY;
    }
    let lr = list_rect(pane);
    Rect::new(
        lr.x,
        lr.y + row as u32 * LIST_ROW_HEIGHT,
        lr.w,
        LIST_ROW_HEIGHT,
    )
}

/// 选中项变化时的脏区：没换页只重铺两行，换页则整块列表都可能移位。
fn list_damage(pane: usize, old_sel: usize, old_top: usize, p: &Pane) -> Rect {
    if p.top != old_top {
        return list_rect(pane);
    }
    row_band(pane, old_sel, p).union(row_band(pane, p.selected, p))
}

/// 把整个界面重画进窗口表面（是否上屏由脏区决定）。
fn draw(app: &AppWindow, panes: &[Pane; 2], active: usize, status: &str, th: &Theme) {
    let s = app.surface();
    s.fill(th.window);
    gfx::draw_panel(s, title_rect(), th.panel, th.dim);
    s.draw_text_limited(
        GAP,
        4,
        WIN_W - GAP * 2,
        "files   Tab switch   j/k move   Enter open   BS up   r reload   q quit",
        th.fg,
        None,
    );

    for i in 0..2 {
        let p = pane_rect(i);
        gfx::draw_panel(
            s,
            p,
            th.window,
            if i == active { th.accent } else { th.dim },
        );
        s.rect(p.x + 1, p.y + 1, p.w.saturating_sub(2), HEADER_H, th.panel);
        s.draw_text_limited(
            p.x + 6,
            p.y + 3,
            p.w.saturating_sub(64),
            panes[i].path_str(),
            th.fg,
            None,
        );
        gfx::draw_button(s, up_button(i), "UP", th, ButtonState::Normal);

        let mut refs = [""; MAX_ENTRIES];
        for (j, slot) in refs.iter_mut().enumerate().take(panes[i].count) {
            *slot = panes[i].name(j);
        }
        gfx::draw_list(
            s,
            list_rect(i),
            &refs[..panes[i].count],
            panes[i].top,
            Some(panes[i].selected),
            th,
        );
        if panes[i].error {
            s.draw_text(p.x + 6, p.y + HEADER_H + 6, "(read error)", th.dim, None);
        }
    }

    gfx::draw_panel(s, status_rect(), th.panel, th.dim);
    s.draw_text(GAP, WIN_H - STATUS_H + 2, status, th.dim, None);
}
