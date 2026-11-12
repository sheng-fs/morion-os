//! 四则运算计算器 —— 窗口 / 按钮 / 键盘的示例应用。
//!
//! - 按钮布局 4×4: `7 8 9 /`、`4 5 6 *`、`1 2 3 -`、`C 0 = +`；
//! - 键盘数字与 `+ - * /` 直接可用，`Enter` 等于 `=`，`C` 清空，`Esc` / `Ctrl+Q` 退出；
//! - 显示区右对齐；除零或溢出显示 `ERROR`；
//! - 运算逻辑在 [`calc`] 里（纯状态机 + 单元测试），本文件只负责绘制与事件分发。

#![no_std]
#![no_main]

mod calc;

use calc::{Calc, Op, DISPLAY_MAX};
use morion::gfx::{
    self, AppWindow, ButtonState, Event, Key, MouseKind, Point, Rect, Theme, CHAR_HEIGHT,
};

// 窗口表面必须落在 libmorion 的单表面上限内（`SURFACE_MAX_PAGES` = 256 页 = 1 MiB）:
// 340 × 400 × 4 B = 544_000 B (133 页)。
const WIN_W: u32 = 340;
const WIN_H: u32 = 400;
const TITLE_H: u32 = 24;
const STATUS_H: u32 = 20;
const PAD: u32 = 8;
const DISPLAY_H: u32 = 64;
const DISPLAY_Y: u32 = TITLE_H;
const GRID_Y: u32 = DISPLAY_Y + DISPLAY_H + PAD;
const GRID_H: u32 = WIN_H - GRID_Y - STATUS_H - PAD;
const COLS: usize = 4;
const ROWS: usize = 4;
const GAP: u32 = 6;
const BTN_W: u32 = (WIN_W - PAD * 2 - GAP * (COLS as u32 - 1)) / COLS as u32;
const BTN_H: u32 = (GRID_H - GAP * (ROWS as u32 - 1)) / ROWS as u32;

/// 错误态显示色。
const ERROR_FG: u32 = 0xE0_60_60;

/// 按钮标签（行优先）。
const LABELS: [[&str; COLS]; ROWS] = [
    ["7", "8", "9", "/"],
    ["4", "5", "6", "*"],
    ["1", "2", "3", "-"],
    ["C", "0", "=", "+"],
];

#[no_mangle]
pub extern "C" fn morion_main(_domain_id: u64) {
    let mut app = match AppWindow::open(40, 40, WIN_W, WIN_H) {
        Some(a) => a,
        None => {
            morion::syscall::println("calc: 建窗失败 (gfx_srv 不可用?)");
            return;
        }
    };
    let th = Theme::DEFAULT;
    let mut calc = Calc::new();

    app.request_repaint(full_rect());
    loop {
        match app.wait_event() {
            Event::Repaint(_) => {
                draw(&app, &calc, &th);
                if !app.present() {
                    break;
                }
            }
            Event::Key(key) => {
                if !handle_key(&mut calc, key) {
                    break;
                }
                app.request_repaint(full_rect());
            }
            Event::Mouse(m) => {
                if m.kind == MouseKind::Down {
                    let p = Point::new(m.x, m.y);
                    for (row, labels) in LABELS.iter().enumerate() {
                        for (col, label) in labels.iter().enumerate() {
                            if gfx::button_hit(btn_rect(row, col), p) {
                                press(&mut calc, label);
                                app.request_repaint(full_rect());
                            }
                        }
                    }
                }
            }
            Event::Focus(_) => {}
        }
    }
    app.close();
}

/// 处理一个按键；返回 `false` 表示退出。
fn handle_key(calc: &mut Calc, key: Key) -> bool {
    match key {
        Key::Char(c) => match c {
            b'0'..=b'9' => calc.digit(c - b'0'),
            b'+' => calc.operator(Op::Add),
            b'-' => calc.operator(Op::Sub),
            b'*' => calc.operator(Op::Mul),
            b'/' => calc.operator(Op::Div),
            b'=' => calc.equals(),
            b'c' | b'C' => calc.clear(),
            b'q' | b'Q' => return false,
            _ => {}
        },
        Key::Enter => calc.equals(),
        Key::Escape | Key::Quit => return false,
        _ => {}
    }
    true
}

/// 按下某个按钮标签。
fn press(calc: &mut Calc, label: &str) {
    match label.as_bytes().first().copied() {
        Some(b'0'..=b'9') => calc.digit(label.as_bytes()[0] - b'0'),
        Some(b'+') => calc.operator(Op::Add),
        Some(b'-') => calc.operator(Op::Sub),
        Some(b'*') => calc.operator(Op::Mul),
        Some(b'/') => calc.operator(Op::Div),
        Some(b'=') => calc.equals(),
        Some(b'C') => calc.clear(),
        _ => {}
    }
}

fn full_rect() -> Rect {
    Rect::new(0, 0, WIN_W, WIN_H)
}

fn title_rect() -> Rect {
    Rect::new(0, 0, WIN_W, TITLE_H)
}

fn display_rect() -> Rect {
    Rect::new(PAD, DISPLAY_Y + 8, WIN_W - PAD * 2, DISPLAY_H - 8)
}

fn status_rect() -> Rect {
    Rect::new(0, WIN_H - STATUS_H, WIN_W, STATUS_H)
}

/// 第 `row` 行第 `col` 列按钮的窗口内矩形。
fn btn_rect(row: usize, col: usize) -> Rect {
    Rect::new(
        PAD + col as u32 * (BTN_W + GAP),
        GRID_Y + row as u32 * (BTN_H + GAP),
        BTN_W,
        BTN_H,
    )
}

/// 把整个界面重画进窗口表面（上屏由脏区决定）。
fn draw(app: &AppWindow, calc: &Calc, th: &Theme) {
    let s = app.surface();
    s.fill(th.window);

    gfx::draw_panel(s, title_rect(), th.panel, th.dim);
    s.draw_text(PAD, 4, "calc", th.fg, None);

    // 显示区：右对齐。
    gfx::draw_panel(s, display_rect(), th.panel, th.accent);
    let mut buf = [0u8; DISPLAY_MAX];
    let text = calc.display(&mut buf);
    let max_w = display_rect().w.saturating_sub(PAD * 2);
    let tw = gfx::text_width(text).min(max_w);
    let x = display_rect().x + display_rect().w.saturating_sub(PAD) - tw;
    let y = display_rect().y + (display_rect().h - CHAR_HEIGHT) / 2;
    let fg = if calc.is_error() {
        ERROR_FG
    } else {
        th.accent_fg
    };
    s.draw_text_limited(x, y, max_w, text, fg, Some(th.panel));

    for (row, labels) in LABELS.iter().enumerate() {
        for (col, label) in labels.iter().enumerate() {
            gfx::draw_button(s, btn_rect(row, col), label, th, ButtonState::Normal);
        }
    }

    gfx::draw_panel(s, status_rect(), th.panel, th.dim);
    s.draw_text_limited(
        PAD,
        WIN_H - STATUS_H + 2,
        WIN_W - PAD * 2,
        "0-9 +-*/   Enter =   C   Esc quit",
        th.dim,
        None,
    );
}
