//! 视频输出 — 全局帧缓冲 + 终端 (历史区 + 固定输入行 + 光标)
//!
//! 屏幕布局 (自顶向下):
//!   - 顶部 `MARGIN` 起为「历史区」, 显示已提交的行, 可通过 ↑/↓ 回滚查看。
//!   - 底部固定一行「输入行」, 用于当前正在编辑/打印的行, 带可见光标。

pub mod font;
pub mod framebuffer;

use crate::bootinfo::BootInfo;
use framebuffer::Framebuffer;
use spin::Mutex;

// 全局帧缓冲状态 (初始化后只读访问, 启动期单线程)
static mut FB: Framebuffer = Framebuffer::empty();
static mut CURSOR_X: u32 = 0;
static mut CURSOR_Y: u32 = 0;

const MARGIN: u32 = 16;
/// 行高 (字符高 + 行间距)
const LINE_HEIGHT: u32 = font::CHAR_HEIGHT + 4;
/// 背景色 (与清屏颜色一致)
const BACKGROUND: u32 = 0x08102A;
/// 前景色 (字符 / 光标)
const FG: u32 = 0xFFFFFF;

// ---- 行历史环形缓冲 (用于回滚查看顶部输出) ----
const HISTORY_LINES: usize = 512;
/// 每行最大字节数 (超过截断)
const LINE_BYTES: usize = 200;
static mut HISTORY: [[u8; LINE_BYTES]; HISTORY_LINES] = [[0; LINE_BYTES]; HISTORY_LINES];
/// 每行实际长度
static mut HISTORY_LEN: [usize; HISTORY_LINES] = [0; HISTORY_LINES];
/// 已存储行数 (≤ HISTORY_LINES)
static mut HISTORY_COUNT: usize = 0;
/// 最老行在环形缓冲中的下标
static mut HISTORY_START: usize = 0;

/// 当前正在编辑的输入行
static mut CUR_LINE: [u8; LINE_BYTES] = [0; LINE_BYTES];
/// 当前行长度
static mut CUR_LEN: usize = 0;
/// 光标位置 (输入行内字符索引, 0..=CUR_LEN)
static mut CUR_POS: usize = 0;

/// 回滚偏移: 0 = 跟随底部(live), N > 0 = 向上回滚了 N 行
static mut SCROLL_OFFSET: usize = 0;

/// 光标所在行 (相对底部输入行向上偏移): 0 = 输入行, N > 0 = 上移到历史区第 N 行
static mut CUR_ROW: usize = 0;

/// 光标在历史区行的列位置 (CUR_ROW > 0 时使用, 0..=该行长度)
static mut CUR_COL: usize = 0;

/// 从 Boot Info 初始化帧缓冲并清屏
pub fn init(info: &BootInfo) {
    unsafe {
        FB = Framebuffer::init(info.fb_addr, info.fb_width, info.fb_height, info.fb_stride);
        CURSOR_X = MARGIN;
        CURSOR_Y = MARGIN;
    }
    clear(BACKGROUND);
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

// ---------------------------------------------------------------------------
// 布局计算
// ---------------------------------------------------------------------------

/// 文本区可容纳的行数 (含输入行)
fn visible_lines() -> usize {
    unsafe { ((FB.height() - MARGIN) / LINE_HEIGHT) as usize }
}

/// 历史区可显示的行数 (给底部输入行留一行)
fn hist_visible() -> usize {
    visible_lines().saturating_sub(1)
}

/// 输入行顶部的 Y 坐标
fn input_y() -> u32 {
    MARGIN + (hist_visible() as u32) * LINE_HEIGHT
}

/// 每行可显示的字符数 (列数)
fn max_cols() -> usize {
    unsafe { ((FB.width() - MARGIN) / font::CHAR_WIDTH) as usize }
}

// ---------------------------------------------------------------------------
// 行历史
// ---------------------------------------------------------------------------

/// 把一段字节追加到行历史环形缓冲
fn history_push(bytes: &[u8]) {
    unsafe {
        let idx = (HISTORY_START + HISTORY_COUNT) % HISTORY_LINES;
        if HISTORY_COUNT < HISTORY_LINES {
            HISTORY_COUNT += 1;
        } else {
            HISTORY_START = (HISTORY_START + 1) % HISTORY_LINES;
        }
        let n = if bytes.len() > LINE_BYTES { LINE_BYTES } else { bytes.len() };
        for i in 0..n {
            HISTORY[idx][i] = bytes[i];
        }
        HISTORY_LEN[idx] = n;
    }
}

/// 提交当前输入行到历史, 并清空输入行 / 光标
fn commit_line() {
    unsafe {
        history_push(&CUR_LINE[..CUR_LEN]);
        CUR_LEN = 0;
        CUR_POS = 0;
    }
}

/// 最大可回滚行数
fn max_scroll() -> usize {
    let total = unsafe { HISTORY_COUNT };
    let visible = hist_visible();
    if total > visible { total - visible } else { 0 }
}

/// 光标可上移到的最大行数 (不触发滚动); 0 = 底部输入行。
/// 等于当前可视区内实际存在的历史行数。
fn cur_row_max() -> usize {
    let visible = hist_visible();
    let total = unsafe { HISTORY_COUNT };
    let start = if total > visible {
        total - visible - unsafe { SCROLL_OFFSET }
    } else {
        0
    };
    let drawn = if total > start { total - start } else { 0 };
    drawn.min(visible)
}

/// 把光标拉回输入行并回到 live 视图 (回车提交时使用)。
fn return_to_input() {
    unsafe {
        CUR_ROW = 0;
        SCROLL_OFFSET = 0;
    }
}

/// 光标所在历史行的环形缓冲下标; CUR_ROW == 0 (输入行) 或越界时返回 None。
fn cur_hist_ridx() -> Option<usize> {
    let cur_row = unsafe { CUR_ROW };
    if cur_row == 0 {
        return None;
    }
    let visible = hist_visible();
    let total = unsafe { HISTORY_COUNT };
    let start = if total > visible {
        total - visible - unsafe { SCROLL_OFFSET }
    } else {
        0
    };
    let row = visible - cur_row;
    let hidx = start + row;
    if hidx < total {
        Some((unsafe { HISTORY_START } + hidx) % HISTORY_LINES)
    } else {
        None
    }
}

/// 光标所在历史行 (CUR_ROW > 0) 的长度; CUR_ROW == 0 时返回 0。
fn cur_hist_len() -> usize {
    match cur_hist_ridx() {
        Some(ridx) => unsafe { HISTORY_LEN[ridx] },
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// 重绘
// ---------------------------------------------------------------------------

/// 从历史 + 输入行重绘整个文本区, 并在光标位置画下划线。
fn redraw() {
    unsafe {
        FB.fill_rect(0, MARGIN, FB.width(), FB.height() - MARGIN, BACKGROUND);

        let visible = hist_visible();
        let total = HISTORY_COUNT;
        let start = if total > visible { total - visible - SCROLL_OFFSET } else { 0 };

        let mut y = MARGIN;
        for i in start..total {
            if y >= input_y() {
                break;
            }
            let idx = (HISTORY_START + i) % HISTORY_LINES;
            let len = HISTORY_LEN[idx];
            let mut x = MARGIN;
            for b in 0..len {
                let ch = HISTORY[idx][b];
                if ch == 0 {
                    break;
                }
                font::draw_char(&mut FB, x, y, ch, FG);
                x += font::CHAR_WIDTH;
                if x + font::CHAR_WIDTH > FB.width() {
                    break;
                }
            }
            y += LINE_HEIGHT;
        }

        // 输入行 (固定底部)
        let iy = input_y();
        let mut x = MARGIN;
        for b in 0..CUR_LEN {
            font::draw_char(&mut FB, x, iy, CUR_LINE[b], FG);
            x += font::CHAR_WIDTH;
            if x + font::CHAR_WIDTH > FB.width() {
                break;
            }
        }

        // 光标位置: CUR_ROW = 0 在输入行 (列 = CUR_POS), >0 在历史区 (列 = CUR_COL)
        let (mut cx, cy) = if CUR_ROW == 0 {
            let cx = MARGIN + (CUR_POS as u32) * font::CHAR_WIDTH;
            (cx, iy + font::CHAR_HEIGHT)
        } else {
            let cy = iy - (CUR_ROW as u32) * LINE_HEIGHT + font::CHAR_HEIGHT;
            (MARGIN + (CUR_COL as u32) * font::CHAR_WIDTH, cy)
        };
        if cx + font::CHAR_WIDTH > FB.width() {
            cx = FB.width() - font::CHAR_WIDTH;
        }
        for dx in 0..font::CHAR_WIDTH {
            FB.pixel(cx + dx, cy, FG);
            FB.pixel(cx + dx, cy + 1, FG);
        }
    }
}

/// 键盘 ↑: 光标上移一行; 光标已在可视区顶部时再触发向上滚动
pub fn scroll_view_up() {
    unsafe {
        if CUR_ROW < cur_row_max() {
            CUR_ROW += 1;
        } else if SCROLL_OFFSET < max_scroll() {
            SCROLL_OFFSET += 1;
        }
        // 上移后把列位置收敛到新行长度内 (仅在历史区)
        if CUR_ROW > 0 {
            let len = cur_hist_len();
            if CUR_COL > len {
                CUR_COL = len;
            }
        }
    }
    redraw();
}

/// 键盘 ↓: 光标下移一行; 光标已回到输入行时再触发向下滚动 (恢复 live)
pub fn scroll_view_down() {
    unsafe {
        if CUR_ROW > 0 {
            CUR_ROW -= 1;
        } else if SCROLL_OFFSET > 0 {
            SCROLL_OFFSET -= 1;
        }
        // 下移后把列位置收敛到新行长度内 (仅在历史区)
        if CUR_ROW > 0 {
            let len = cur_hist_len();
            if CUR_COL > len {
                CUR_COL = len;
            }
        }
    }
    redraw();
}

// ---------------------------------------------------------------------------
// 输出
// ---------------------------------------------------------------------------

/// 输出互斥锁: 串行化各域的打印, 防止并发下字符交错 (多核防御)。
static PRINT_LOCK: Mutex<()> = Mutex::new(());

/// 串口 16550 (COM1, 0x3F8) 单字符输出 — 用于 QEMU `-serial stdio` 抓日志。
/// 不做硬件初始化，依赖 QEMU 默认已准备好 COM1。
///
/// 注意: 串口是 I/O 端口, 必须用 x86 IN/OUT 指令访问. 绝对不能对 0x3F8 做
/// 内存 load/store (那是访问"虚拟/物理地址 0x3F8"的 DRAM, 不是串口寄存器).
#[inline]
fn serial_put(ch: u8) {
    unsafe {
        // 轮询 LSR@0x3FD bit5 = THR 空
        let mut lsr: u8;
        for _ in 0..1_000_000u32 {
            core::arch::asm!(
                "in al, dx",
                in("dx") 0x3FDu16,
                out("al") lsr,
                options(nomem, nostack, preserves_flags),
            );
            if lsr & 0x20 != 0 {
                break;
            }
        }
        core::arch::asm!(
            "out dx, al",
            in("dx") 0x3F8u16,
            in("al") ch,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// 打印字符串 (支持 '\n' 换行; 日志追加到输入行末尾)
pub fn print(s: &str) {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let guard = PRINT_LOCK.lock();

    for ch in s.bytes() {
        // 同步写串口: 在加锁成功 & 写屏幕前先打串口，保证屏幕打印成功的行
        // 一定能在串口里看到 (triple fault 后串口缓冲区还会被 QEMU 刷出)。
        serial_put(ch);
        match ch {
            b'\n' => commit_line(),
            _ => unsafe {
                if CUR_LEN >= max_cols() {
                    commit_line();
                }
                if CUR_LEN < LINE_BYTES {
                    CUR_LINE[CUR_LEN] = ch;
                    CUR_LEN += 1;
                    CUR_POS = CUR_LEN;
                }
            },
        }
    }
    redraw();

    // 先释放锁再开中断, 避免「开中断后被抢占、别的核/任务自旋等锁」造成的死锁。
    drop(guard);
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
}

/// 打印字符串并换行
pub fn println(s: &str) {
    print(s);
    print("\n");
}

// ---------------------------------------------------------------------------
// 终端编辑 (键盘输入)
// ---------------------------------------------------------------------------

/// 在光标处插入一个字符 (回车提交当前行)。
///
/// 光标在历史区 (CUR_ROW > 0) 时, 字符直接插入到该历史行光标处, 不跳回输入行。
pub fn term_put(c: u8) {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();

    unsafe {
        match c {
            b'\n' => {
                return_to_input();
                commit_line();
            }
            _ => {
                if CUR_ROW > 0 {
                    insert_hist_char(c);
                } else {
                    if CUR_LEN >= max_cols() {
                        commit_line();
                    }
                    if CUR_LEN < LINE_BYTES {
                        for i in (CUR_POS..CUR_LEN).rev() {
                            CUR_LINE[i + 1] = CUR_LINE[i];
                        }
                        CUR_LINE[CUR_POS] = c;
                        CUR_LEN += 1;
                        CUR_POS += 1;
                    }
                }
            }
        }
    }
    redraw();

    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
}

/// 在光标所在历史行的 CUR_COL 处插入一个字符, 光标右移。
fn insert_hist_char(c: u8) {
    let ridx = match cur_hist_ridx() {
        Some(r) => r,
        None => return,
    };
    unsafe {
        let len = HISTORY_LEN[ridx];
        let mut col = CUR_COL;
        if col > len {
            col = len;
        }
        if len < LINE_BYTES {
            for i in (col..len).rev() {
                HISTORY[ridx][i + 1] = HISTORY[ridx][i];
            }
            HISTORY[ridx][col] = c;
            HISTORY_LEN[ridx] = len + 1;
            CUR_COL = col + 1;
        }
    }
}

/// 退格: 删除光标前一个字符。
///
/// 光标在历史区时删除该历史行光标前字符, 不跳回输入行。
pub fn term_backspace() {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();

    unsafe {
        if CUR_ROW > 0 {
            backspace_hist_char();
        } else if CUR_POS > 0 {
            for i in CUR_POS..CUR_LEN {
                CUR_LINE[i - 1] = CUR_LINE[i];
            }
            CUR_LEN -= 1;
            CUR_POS -= 1;
        }
    }
    redraw();

    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
}

/// 删除光标所在历史行 CUR_COL 前的一个字符, 光标左移。
fn backspace_hist_char() {
    let ridx = match cur_hist_ridx() {
        Some(r) => r,
        None => return,
    };
    unsafe {
        let len = HISTORY_LEN[ridx];
        let mut col = CUR_COL;
        if col > len {
            col = len;
        }
        if col > 0 {
            for i in col..len {
                HISTORY[ridx][i - 1] = HISTORY[ridx][i];
            }
            HISTORY_LEN[ridx] = len - 1;
            CUR_COL = col - 1;
        }
    }
}

/// 光标左移
pub fn term_left() {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();

    unsafe {
        if CUR_ROW == 0 {
            if CUR_POS > 0 {
                CUR_POS -= 1;
            }
        } else if CUR_COL > 0 {
            CUR_COL -= 1;
        }
    }
    redraw();

    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
}

/// 光标右移
pub fn term_right() {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();

    unsafe {
        if CUR_ROW == 0 {
            if CUR_POS < CUR_LEN {
                CUR_POS += 1;
            }
        } else if CUR_COL < cur_hist_len() {
            CUR_COL += 1;
        }
    }
    redraw();

    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
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