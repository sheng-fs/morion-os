//! 视频输出 — 全局帧缓冲 + 终端 (历史区 + 固定输入行 + 光标)
//!
//! 屏幕布局 (自顶向下):
//!   - 顶部 `MARGIN` 起为「历史区」, 显示已提交的行, 可通过 ↑/↓ 回滚查看。
//!   - 底部固定一行「输入行」, 用于当前正在编辑/打印的行, 带可见光标。

pub mod bg;
pub mod font;
pub mod framebuffer;
pub mod logo;

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

/// 本轮用户输入的起点 (CUR_LINE 下标): 其前是 shell 提示符等已打印文本, 不计入输入。
static mut INPUT_BASE: usize = 0;
/// 本轮是否已有用户按键 (决定 INPUT_BASE 何时锁定, 以及回车提交哪一段)。
static mut INPUT_ACTIVE: bool = false;

/// 回滚偏移: 0 = 跟随底部(live), N > 0 = 向上回滚了 N 行
static mut SCROLL_OFFSET: usize = 0;

/// 光标所在行 (相对底部输入行向上偏移): 0 = 输入行, N > 0 = 上移到历史区第 N 行
static mut CUR_ROW: usize = 0;

/// 光标在历史区行的列位置 (CUR_ROW > 0 时使用, 0..=该行长度)
static mut CUR_COL: usize = 0;

// ---- 输入行队列 (键盘 Enter 提交的行, 供 SYS_READLINE 取走) ----
// 键盘域经 SYS_TERM_PUT 编辑 CUR_LINE, 回车时把整行压入此队列并唤醒等待者;
// 用户态 SYS_READLINE 阻塞取走一行。队列满时丢弃最旧行 (交互输入不阻塞内核)。
const INPUT_QUEUE_LINES: usize = 4;
static mut INPUT_QUEUE: [[u8; LINE_BYTES]; INPUT_QUEUE_LINES] = [[0; LINE_BYTES]; INPUT_QUEUE_LINES];
static mut INPUT_QUEUE_LEN: [usize; INPUT_QUEUE_LINES] = [0; INPUT_QUEUE_LINES];
/// 最老行在队列中的下标。
static mut INPUT_HEAD: usize = 0;
/// 下一写入位置在队列中的下标。
static mut INPUT_TAIL: usize = 0;
/// 当前队列中的行数 (<= INPUT_QUEUE_LINES)。
static mut INPUT_COUNT: usize = 0;

// ---------------------------------------------------------------------------
// COM1 串口输出 (headless 调试用)
// ---------------------------------------------------------------------------
// 内核当前只把日志写到 GOP 帧缓冲, 无显示器时无法观察启动进度。此处在 COM1
// (0x3F8) 镜像一份输出, 供 QEMU `-serial file:` 捕获启动日志, 定位崩溃点。

const COM1: u16 = 0x3F8;

/// 初始化 COM1 串口 (115200 8N1)。
fn serial_init() {
    use x86_64::instructions::port::Port;
    unsafe {
        let mut data: Port<u8> = Port::new(COM1);
        let mut ier: Port<u8> = Port::new(COM1 + 1);
        let mut fcr: Port<u8> = Port::new(COM1 + 2);
        let mut lcr: Port<u8> = Port::new(COM1 + 3);
        let mut mcr: Port<u8> = Port::new(COM1 + 4);

        ier.write(0x00); // 禁用中断
        lcr.write(0x80); // 使能 DLAB (设置分频)
        data.write(0x03); // 分频低字节 (115200)
        ier.write(0x00); // 分频高字节 0
        lcr.write(0x03); // 8N1
        fcr.write(0xC7); // 使能并清空 FIFO
        mcr.write(0x0B); // DTR | RTS | OUT2
    }
}

/// 向 COM1 写一个字节 (忙等发送缓冲空)。
fn serial_putc(c: u8) {
    use x86_64::instructions::port::Port;
    unsafe {
        let mut status: Port<u8> = Port::new(COM1 + 5);
        while status.read() & 0x20 == 0 {}
        let mut data: Port<u8> = Port::new(COM1);
        data.write(c);
    }
}

/// 向 COM1 写一段字符串 (`\n` 扩展为 `\r\n`)。
fn serial_write(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' {
            serial_putc(b'\r');
        }
        serial_putc(b);
    }
}

/// 从 Boot Info 初始化帧缓冲并清屏
pub fn init(info: &BootInfo) {
    serial_init();
    unsafe {
        FB = Framebuffer::init(info.fb_addr, info.fb_width, info.fb_height, info.fb_stride);
        CURSOR_X = MARGIN;
        CURSOR_Y = MARGIN;
    }
    bg_fill_all();
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

/// 用背景渐变填充一块矩形 (替代纯色填充)。
///
/// 背景是竖直渐变, 逐行取色一次性填满整行, 开销与纯色 `fill_rect` 同量级。
fn bg_fill_rect(x: u32, y: u32, w: u32, h: u32) {
    unsafe {
        let screen_h = FB.height();
        for dy in 0..h {
            let color = bg::color_for_row(y + dy, screen_h);
            FB.fill_rect(x, y + dy, w, 1, color);
        }
    }
}

/// 用背景渐变铺满整屏。
fn bg_fill_all() {
    unsafe {
        let (w, h) = (FB.width(), FB.height());
        bg_fill_rect(0, 0, w, h);
    }
}

/// 清屏
pub fn clear(color: u32) {
    unsafe { FB.clear(color) }
}

/// 清屏并复位终端状态 (历史 / 输入行 / 光标 / 回滚), 供 `SYS_CLEAR` 使用。
pub fn clear_screen() {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    unsafe {
        HISTORY_COUNT = 0;
        HISTORY_START = 0;
        SCROLL_OFFSET = 0;
        CUR_LINE = [0; LINE_BYTES];
        CUR_LEN = 0;
        CUR_POS = 0;
        CUR_ROW = 0;
        CUR_COL = 0;
        INPUT_BASE = 0;
        INPUT_ACTIVE = false;
    }
    bg_fill_all();
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
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
        HISTORY[idx][..n].copy_from_slice(&bytes[..n]);
        HISTORY_LEN[idx] = n;
    }
}

/// 提交当前输入行到历史, 并清空输入行 / 光标
fn commit_line() {
    unsafe {
        history_push(&CUR_LINE[..CUR_LEN]);
        CUR_LEN = 0;
        CUR_POS = 0;
        // 提交后本轮输入结束: 下一次按键重新锁定输入起点。
        INPUT_BASE = 0;
        INPUT_ACTIVE = false;
    }
}

/// 把一行 (键盘回车提交的输入) 压入输入行队列; 队列满时丢弃最旧行。
fn input_queue_push(bytes: &[u8]) {
    unsafe {
        let idx = INPUT_TAIL;
        let n = if bytes.len() > LINE_BYTES { LINE_BYTES } else { bytes.len() };
        INPUT_QUEUE[idx][..n].copy_from_slice(&bytes[..n]);
        INPUT_QUEUE_LEN[idx] = n;
        INPUT_TAIL = (INPUT_TAIL + 1) % INPUT_QUEUE_LINES;
        if INPUT_COUNT < INPUT_QUEUE_LINES {
            INPUT_COUNT += 1;
        } else {
            INPUT_HEAD = (INPUT_HEAD + 1) % INPUT_QUEUE_LINES;
        }
    }
}

/// 从输入行队列取走一行, 拷入 `out` (最多 `max` 字节, 不含换行)。
/// 返回该行长度; 队列为空返回 `None`。
///
/// # Safety
/// `out` 必须指向本域内至少 `max` 字节的可写缓冲。
pub unsafe fn input_read(out: *mut u8, max: usize) -> Option<usize> {
    unsafe {
        if INPUT_COUNT == 0 {
            return None;
        }
        let idx = INPUT_HEAD;
        let len = INPUT_QUEUE_LEN[idx];
        let n = if len > max { max } else { len };
        if n > 0 {
            core::ptr::copy_nonoverlapping(INPUT_QUEUE[idx].as_ptr(), out, n);
        }
        INPUT_HEAD = (INPUT_HEAD + 1) % INPUT_QUEUE_LINES;
        INPUT_COUNT -= 1;
        Some(n)
    }
}

/// 最大可回滚行数
fn max_scroll() -> usize {
    let total = unsafe { HISTORY_COUNT };
    let visible = hist_visible();
    total.saturating_sub(visible)
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
    let drawn = total.saturating_sub(start);
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
        // 用背景渐变擦除内容区 (而非纯色), 以便背景图在每次重绘后保持。
        bg_fill_rect(0, MARGIN, FB.width(), FB.height() - MARGIN);
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
            for &ch in HISTORY[idx][..len].iter() {
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
    }
    draw_input_line();
}

/// 只重绘底部输入行 (清空该行 + 画字符与光标)。
///
/// 打字/退格/左右移动光标时历史区不变, 无需整屏重绘 —— 整屏 `redraw()` 会重填
/// 整个文本区背景并重画所有历史行, 在未缓存的 MMIO 帧缓冲上代价很高 (逐键卡顿)。
/// 仅在光标始终位于输入行 (`CUR_ROW == 0` 且未发生换行提交) 时使用。
fn redraw_input_line() {
    let iy = input_y();
    bg_fill_rect(0, iy, unsafe { FB.width() }, LINE_HEIGHT);
    draw_input_line();
}

/// 画输入行内容与光标下划线 (调用方负责先擦除该行区域)。
fn draw_input_line() {
    let iy = input_y();
    unsafe {
        let mut x = MARGIN;
        for &ch in CUR_LINE[..CUR_LEN].iter() {
            font::draw_char(&mut FB, x, iy, ch, FG);
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
        } else {
            SCROLL_OFFSET = SCROLL_OFFSET.saturating_sub(1);
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

/// 把一个字符追加到当前输入行 (行满则先换行提交); 返回是否发生了提交。
fn append_char(ch: u8) -> bool {
    unsafe {
        if CUR_LEN >= max_cols() {
            commit_line();
            if CUR_LEN < LINE_BYTES {
                CUR_LINE[CUR_LEN] = ch;
                CUR_LEN += 1;
                CUR_POS = CUR_LEN;
            }
            return true;
        }
        if CUR_LEN < LINE_BYTES {
            CUR_LINE[CUR_LEN] = ch;
            CUR_LEN += 1;
            CUR_POS = CUR_LEN;
        }
        false
    }
}

/// 追加一段文本 (`\n` 提交当前行); 返回是否发生过换行提交。
fn append_text(s: &str) -> bool {
    let mut committed = false;
    for ch in s.bytes() {
        if ch == b'\n' {
            commit_line();
            committed = true;
        } else if append_char(ch) {
            committed = true;
        }
    }
    committed
}

/// 打印字符串 (支持 '\n' 换行; 日志追加到输入行末尾)
pub fn print(s: &str) {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let guard = PRINT_LOCK.lock();

    // 镜像到 COM1 串口, 供 headless 调试捕获。
    serial_write(s);

    let committed = append_text(s);
    // 历史区变化或光标不在输入行时才需要整屏重绘, 否则只重画输入行。
    if committed || unsafe { CUR_ROW } != 0 {
        redraw();
    } else {
        redraw_input_line();
    }

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

/// 打印启动 LOGO: 整块水平居中, 一次性追加 + 单次重绘。
///
/// 注意必须按**整块**居中 (常量左缩进): 逐行各自居中的话, 每行长度不同会把图形
/// 横向错切 (这是此前 LOGO「形状不对」的原因)。原先逐字符/逐行调用
/// `print()` 还会触发上百次整屏重绘 (启动明显卡顿), 这里改为批量追加后只重绘一次。
pub fn print_logo() {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let guard = PRINT_LOCK.lock();

    let cols = max_cols();
    let indent = if cols > logo::WIDTH { (cols - logo::WIDTH) / 2 } else { 0 };

    for line in logo::LOGO {
        for _ in 0..indent {
            serial_putc(b' ');
            append_char(b' ');
        }
        serial_write(line);
        serial_write("\n");
        append_text(line);
        commit_line();
    }
    redraw();

    drop(guard);
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
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

    let mut entered = false;
    // 历史区是否变化 (决定整屏重绘还是只重画输入行)。
    let mut needs_full = false;
    unsafe {
        match c {
            b'\n' => {
                // 回车提交: 只把「用户输入段」(INPUT_BASE 起) 拷入输入行队列,
                // 不含此前打印的提示符, 再清空输入行。
                let base = if INPUT_ACTIVE { INPUT_BASE.min(CUR_LEN) } else { CUR_LEN };
                input_queue_push(&CUR_LINE[base..CUR_LEN]);
                return_to_input();
                commit_line();
                entered = true;
                needs_full = true;
            }
            _ => {
                if CUR_ROW > 0 {
                    insert_hist_char(c);
                    needs_full = true;
                } else {
                    // 本轮首个按键: 锁定输入起点 = 当前行尾 (提示符长度)。
                    if !INPUT_ACTIVE {
                        INPUT_BASE = CUR_LEN;
                        INPUT_ACTIVE = true;
                    }
                    if CUR_LEN >= max_cols() {
                        // 输入行满: 换行续写, 新行完全属于用户输入。
                        commit_line();
                        INPUT_BASE = 0;
                        INPUT_ACTIVE = true;
                        needs_full = true;
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
    if needs_full {
        redraw();
    } else {
        redraw_input_line();
    }

    // 有新输入行时唤醒阻塞在 SYS_READLINE 上的任务 (哨兵 wait_on = INPUT_WAIT)。
    if entered {
        crate::scheduler::wake_one(crate::scheduler::INPUT_WAIT);
    }

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

    let mut needs_full = false;
    unsafe {
        if CUR_ROW > 0 {
            backspace_hist_char();
            needs_full = true;
        } else if CUR_POS > 0 {
            // 不删到输入起点之前 (即不破坏提示符)。
            if !(INPUT_ACTIVE && CUR_POS <= INPUT_BASE) {
                for i in CUR_POS..CUR_LEN {
                    CUR_LINE[i - 1] = CUR_LINE[i];
                }
                CUR_LEN -= 1;
                CUR_POS -= 1;
            }
        }
    }
    if needs_full {
        redraw();
    } else {
        redraw_input_line();
    }

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

    let mut needs_full = false;
    unsafe {
        if CUR_ROW == 0 {
            // 不左移到输入起点之前 (即不进入提示符)。
            if CUR_POS > 0 && !(INPUT_ACTIVE && CUR_POS <= INPUT_BASE) {
                CUR_POS -= 1;
            }
        } else {
            CUR_COL = CUR_COL.saturating_sub(1);
            needs_full = true;
        }
    }
    if needs_full {
        redraw();
    } else {
        redraw_input_line();
    }

    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
}

/// 光标右移
pub fn term_right() {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();

    let mut needs_full = false;
    unsafe {
        if CUR_ROW == 0 {
            if CUR_POS < CUR_LEN {
                CUR_POS += 1;
            }
        } else {
            if CUR_COL < cur_hist_len() {
                CUR_COL += 1;
            }
            needs_full = true;
        }
    }
    if needs_full {
        redraw();
    } else {
        redraw_input_line();
    }

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