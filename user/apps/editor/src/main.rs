//! 纯文本编辑器 — 窗口 / 控件 / VFS 的示例骨架（第二批接线，不参与本批镜像构建）。
//!
//! - 打开 `/mfs/notes.txt`（若存在），在窗口文本区里编辑；
//! - Ctrl+S 保存（`creat` + `truncate` + 分块 `write`），Ctrl+Q / Esc 退出；
//! - 绘制与事件只用 `morion::gfx`（控件 + [`AppWindow`] 事件循环）与 `morion::vfs`；
//!   键盘经 `AppWindow` 内部的非阻塞读取，等价于 `SYS_KEY_READ`。
//!
//! 窗口更新走**脏矩形**：移动光标只重铺新旧两行 + 状态栏，编辑时重铺文本区。

#![no_std]
#![no_main]

use morion::gfx::{
    self, AppWindow, ButtonState, Event, Key, MouseKind, Point, Rect, Theme, CHAR_HEIGHT,
    CHAR_WIDTH,
};
use morion::syscall::{println, sys_alloc_page, sys_share_page, sys_virt_to_phys};
use morion::vfs;

const WIN_W: u32 = 720;
const WIN_H: u32 = 480;
const TITLE_H: u32 = 24;
const STATUS_H: u32 = 20;
const AREA_Y: u32 = TITLE_H;
const AREA_H: u32 = WIN_H - TITLE_H - STATUS_H;
/// 文本区左边距（像素）。
const PAD_X: u32 = 8;
/// 文本缓冲上限（不超过用户栈预算）。
const MAX_TEXT: usize = 8192;
/// 单次 VFS 读写的字节数（一页）。
const CHUNK: u32 = 4096;
/// 默认文件：MFS 挂载点下的笔记。
const PATH: &str = "/mfs/notes.txt";
/// MFS 服务域（与内核建域顺序一致）。
const MFS_DOMAIN: u64 = 11;

/// 本应用专用的 VFS 结果页 / 写缓冲页。
///
/// 必须是**本应用独有**的虚拟地址：`SYS_SHARE_PAGE` 把页按同一地址映射进服务域，
/// 与 app（`0x...104000`）、shell（`0x...106000`）、hello（`0x...10C000`）撞地址会让
/// 内核 `PageAlreadyMapped` panic。
const BUF_RESULT: u64 = 0x0000_0080_0011_0000;
const BUF_WRITE: u64 = 0x0000_0080_0011_1000;

#[no_mangle]
pub extern "C" fn morion_main(_domain_id: u64) {
    if !setup_buffers() {
        println("editor: VFS 缓冲页共享失败");
        return;
    }
    let mut app = match AppWindow::open(40, 40, WIN_W, WIN_H) {
        Some(a) => a,
        None => {
            println("editor: 建窗失败 (gfx_srv 不可用?)");
            return;
        }
    };
    let th = Theme::DEFAULT;
    let mut text = [0u8; MAX_TEXT];
    let mut len = load_file(&mut text);
    let mut cursor = len;
    // 打开就把视口滚到光标所在行（长文件末尾）。
    let rows = (AREA_H / CHAR_HEIGHT) as usize;
    let mut scroll = cursor_line(&text[..len], cursor)
        .0
        .saturating_sub(rows.saturating_sub(1));
    let mut modified = false;
    let mut save_failed = false;

    app.request_repaint(Rect::new(0, 0, WIN_W, WIN_H));
    loop {
        match app.wait_event() {
            Event::Repaint(_) => {
                draw(
                    &app,
                    &text[..len],
                    cursor,
                    scroll,
                    modified,
                    save_failed,
                    &th,
                );
                if !app.present() {
                    break;
                }
            }
            Event::Key(key) => {
                let (old_line, _) = cursor_line(&text[..len], cursor);
                let mut edit = false;
                let mut quit = false;
                match key {
                    Key::Char(b) => {
                        if len < MAX_TEXT {
                            text.copy_within(cursor..len, cursor + 1);
                            text[cursor] = b;
                            cursor += 1;
                            len += 1;
                            edit = true;
                        }
                    }
                    Key::Enter => {
                        if len < MAX_TEXT {
                            text.copy_within(cursor..len, cursor + 1);
                            text[cursor] = b'\n';
                            cursor += 1;
                            len += 1;
                            edit = true;
                        }
                    }
                    Key::Tab => {
                        if len < MAX_TEXT {
                            text.copy_within(cursor..len, cursor + 1);
                            text[cursor] = b' ';
                            cursor += 1;
                            len += 1;
                            edit = true;
                        }
                    }
                    Key::Backspace => {
                        if cursor > 0 {
                            text.copy_within(cursor..len, cursor - 1);
                            cursor -= 1;
                            len -= 1;
                            edit = true;
                        }
                    }
                    Key::Delete => {
                        if cursor < len {
                            text.copy_within(cursor + 1..len, cursor);
                            len -= 1;
                            edit = true;
                        }
                    }
                    Key::Left => cursor = cursor.saturating_sub(1),
                    Key::Right => cursor = (cursor + 1).min(len),
                    Key::Home => cursor = line_start(&text[..len], old_line),
                    Key::End => cursor = line_end(&text[..len], old_line),
                    Key::Save => {
                        save_failed = !save_file(&text[..len]);
                        modified = save_failed;
                    }
                    Key::Quit | Key::Escape => quit = true,
                    _ => {}
                }
                if quit {
                    break;
                }
                if edit {
                    modified = true;
                    save_failed = false;
                }
                let (new_line, _) = cursor_line(&text[..len], cursor);
                let rows = (AREA_H / CHAR_HEIGHT) as usize;
                if new_line < scroll {
                    scroll = new_line;
                } else if new_line >= scroll + rows {
                    scroll = new_line + 1 - rows;
                }
                if edit {
                    // 行可能移位：重铺整个文本区 + 状态栏。
                    app.request_repaint(area_rect().union(status_rect()));
                } else if old_line != new_line || cursor != len {
                    app.request_repaint(
                        line_band(old_line, scroll)
                            .union(line_band(new_line, scroll))
                            .union(status_rect()),
                    );
                } else {
                    app.request_repaint(line_band(new_line, scroll).union(status_rect()));
                }
            }
            Event::Mouse(m) => {
                if m.kind == MouseKind::Down && gfx::button_hit(save_rect(), Point::new(m.x, m.y)) {
                    save_failed = !save_file(&text[..len]);
                    modified = save_failed;
                    app.request_repaint(status_rect());
                }
            }
            Event::Focus(true) => app.request_repaint(title_rect()),
            Event::Focus(false) => {}
        }
    }
    app.close();
}

/// 分配并把本应用的结果页 / 写缓冲页共享给 MFS 服务（`vfs::read_into` / `write_into` 的前提）。
fn setup_buffers() -> bool {
    for va in [BUF_RESULT, BUF_WRITE] {
        if sys_virt_to_phys(va) == 0 && sys_alloc_page(va) != 1 {
            return false;
        }
        if sys_share_page(va, MFS_DOMAIN) != 1 {
            return false;
        }
    }
    true
}

/// 读入整个文件（最多 [`MAX_TEXT`] 字节）；文件不存在时返回 0（空白编辑区）。
fn load_file(buf: &mut [u8]) -> usize {
    let fd = vfs::open(PATH);
    if fd == u64::MAX {
        return 0;
    }
    let mut len = 0usize;
    while len < buf.len() {
        let want = (buf.len() - len).min(CHUNK as usize) as u32;
        let n = vfs::read_into(fd, len as u64, want, BUF_RESULT);
        if n == u64::MAX || n == 0 {
            break;
        }
        let n = (n as usize).min(buf.len() - len);
        unsafe {
            let src = BUF_RESULT as *const u8;
            core::ptr::copy_nonoverlapping(src, buf[len..].as_mut_ptr(), n);
        }
        len += n;
    }
    vfs::close(fd);
    len
}

/// 保存整个文本（`creat` → 截断 → 分块写）；成功返回 `true`。
fn save_file(data: &[u8]) -> bool {
    let fd = vfs::creat(PATH);
    if fd == u64::MAX {
        return false;
    }
    let _ = vfs::truncate(fd, 0);
    let mut off = 0usize;
    while off < data.len() {
        let n = vfs::write_into(fd, off as u64, &data[off..], BUF_WRITE);
        if n == u64::MAX || n == 0 {
            vfs::close(fd);
            return false;
        }
        off += n as usize;
    }
    vfs::close(fd) == 1
}

fn title_rect() -> Rect {
    Rect::new(0, 0, WIN_W, TITLE_H)
}

fn area_rect() -> Rect {
    Rect::new(0, AREA_Y, WIN_W, AREA_H)
}

fn status_rect() -> Rect {
    Rect::new(0, WIN_H - STATUS_H, WIN_W, STATUS_H)
}

fn save_rect() -> Rect {
    Rect::new(WIN_W - 84, 3, 76, 18)
}

/// 光标所在的**行**矩形（不可见时为空矩形）。
fn line_band(line: usize, scroll: usize) -> Rect {
    if line < scroll {
        return Rect::EMPTY;
    }
    let row = line - scroll;
    if row as u32 >= AREA_H / CHAR_HEIGHT {
        return Rect::EMPTY;
    }
    Rect::new(0, AREA_Y + row as u32 * CHAR_HEIGHT, WIN_W, CHAR_HEIGHT)
}

/// 光标所在的 `(行, 列)`（列按字节计，文本按 ASCII 处理）。
fn cursor_line(text: &[u8], cursor: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut col = 0usize;
    for &b in &text[..cursor.min(text.len())] {
        if b == b'\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// 第 `line` 行的起始字节下标。
fn line_start(text: &[u8], line: usize) -> usize {
    let mut idx = 0usize;
    for _ in 0..line {
        while idx < text.len() && text[idx] != b'\n' {
            idx += 1;
        }
        if idx < text.len() {
            idx += 1;
        }
    }
    idx.min(text.len())
}

/// 第 `line` 行的结束字节下标（不含换行符）。
fn line_end(text: &[u8], line: usize) -> usize {
    let mut idx = line_start(text, line);
    while idx < text.len() && text[idx] != b'\n' {
        idx += 1;
    }
    idx
}

/// 把整个界面重画进窗口表面（是否上屏由脏区决定，不在这里 `present`）。
fn draw(
    app: &AppWindow,
    text: &[u8],
    cursor: usize,
    scroll: usize,
    modified: bool,
    save_failed: bool,
    th: &Theme,
) {
    let s = app.surface();
    s.fill(th.window);
    // 标题栏：路径 + 保存按钮 + 修改标记。
    gfx::draw_panel(s, title_rect(), th.panel, th.dim);
    s.draw_text_limited(PAD_X, 4, WIN_W - 120, PATH, th.fg, None);
    let btn = if modified {
        ButtonState::Hover
    } else {
        ButtonState::Normal
    };
    gfx::draw_button(s, save_rect(), "SAVE", th, btn);
    if modified {
        s.draw_char(WIN_W - 100, 4, b'*', th.accent, None);
    }

    // 文本区。
    s.rect(0, AREA_Y, WIN_W, AREA_H, th.panel);
    let rows = (AREA_H / CHAR_HEIGHT) as usize;
    let max_chars = ((WIN_W - PAD_X * 2) / CHAR_WIDTH) as usize;
    let lines = text.iter().filter(|&&b| b == b'\n').count() + 1;
    for row in 0..rows {
        let li = scroll + row;
        if li >= lines {
            break;
        }
        let start = line_start(text, li);
        let end = line_end(text, li);
        let y = AREA_Y + row as u32 * CHAR_HEIGHT;
        for (i, &b) in text[start..end].iter().take(max_chars).enumerate() {
            s.draw_char(PAD_X + i as u32 * CHAR_WIDTH, y, b, th.fg, None);
        }
    }
    // 光标：只在可见行上画一条竖线。
    let (line, col) = cursor_line(text, cursor);
    if line >= scroll && line - scroll < rows {
        let x = PAD_X + (col as u32 * CHAR_WIDTH).min(WIN_W - PAD_X - 1);
        let y = AREA_Y + (line - scroll) as u32 * CHAR_HEIGHT;
        s.rect(x, y, 2, CHAR_HEIGHT, th.accent_fg);
    }

    // 状态栏。
    gfx::draw_panel(s, status_rect(), th.panel, th.dim);
    let msg = if save_failed {
        "SAVE FAILED"
    } else if modified {
        "MODIFIED"
    } else {
        "Ctrl+S save  Ctrl+Q quit"
    };
    s.draw_text(PAD_X, WIN_H - STATUS_H + 2, msg, th.dim, None);
}
