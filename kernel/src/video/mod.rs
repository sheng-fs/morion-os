//! 视频输出 — 全局帧缓冲 + **只输出**文本控制台 (G4)
//!
//! 屏幕布局 (自顶向下): 从 `MARGIN` 起是「历史区」, 逐行显示已提交的输出。
//!
//! **内核这边只管输出**。输入行编辑 / 行历史导航 / 光标在 G4 全部搬到了用户态:
//! 键盘字节经 [`crate::key`] 交出去, 由 `gfx_srv` 的屏幕控制台读键、回显、编辑
//! (客户端用 `GFX_OP_LINE` 要一行)。内核终端因此只服务两件事 —— **引导期日志**
//! (`gfx_srv` 接管前) 与 **panic 屏**。
//!
//! 字体: ASCII 用 `font.rs` 的 8x16 位图; 其余字符 (汉字 / 全角标点 / 杂项符号) 内核
//! **已不带字库** (G3c 起字库归用户态 `gfx_srv`), 一律画空心豆腐块占位。行缓冲存的是
//! **UTF-8 字节**, 排版按**显示列**算 (汉字 16x16 占 2 列), 显示位置一律经
//! `unicode::str_width` 折算。
//!
//! 所有输出同时镜像到 **COM1 串口** —— headless 回归 (`scripts/fs-regress.sh`) 靠它取证。

pub mod bg;
pub mod font;
pub mod framebuffer;
pub mod logo;
pub mod unicode;

use crate::bootinfo::BootInfo;
use core::sync::atomic::{AtomicBool, Ordering};
use framebuffer::Framebuffer;
use spin::Mutex;

// 全局帧缓冲状态 (初始化后只读访问, 启动期单线程)
static mut FB: Framebuffer = Framebuffer::empty();

/// 帧缓冲物理基址与字节数 (初始化时记录), 供 `SYS_FB_INFO` / `SYS_FB_MAP` 查询。
static mut FB_BASE: u64 = 0;
static mut FB_BYTES: u64 = 0;
/// 帧缓冲几何 `(宽, 高, 行跨度像素, 每像素位数)`。
static mut FB_GEOM: (u32, u32, u32, u32) = (0, 0, 0, 0);

/// 用户态图形服务是否已**接管**显示 (`SYS_FB_TAKEOVER`)。
///
/// 置位后内核不再写帧缓冲 (重绘直接跳过), 输出只保留 COM1 —— 屏幕从此由用户态
/// `gfx_srv` 负责。早期引导与 panic 输出仍走内核路径 (那时还没接管), 故 panic 永远
/// 不依赖用户态服务。
static FB_TAKEN_OVER: AtomicBool = AtomicBool::new(false);

const MARGIN: u32 = 16;
/// 行高 (字符高 + 行间距)
const LINE_HEIGHT: u32 = font::CHAR_HEIGHT + 4;
/// 前景色 (字符)
const FG: u32 = 0xFFFFFF;

// ---- 行历史环形缓冲 (屏幕上滚的源头) ----
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

/// **当前正在输出**的那一行 (还没以换行结束)。
///
/// 它单独画在历史区下方, 不在环形缓冲里 —— 提交 (`commit_line`) 才进历史。
static mut LINE: [u8; LINE_BYTES] = [0; LINE_BYTES];
/// 当前行长度 (字节)
static mut LINE_LEN: usize = 0;

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

/// 向 COM1 写一个字节 (等待发送保持寄存器空)。
fn serial_putc(c: u8) {
    use x86_64::instructions::port::Port;
    unsafe {
        let mut lsr: Port<u8> = Port::new(COM1 + 5);
        let mut data: Port<u8> = Port::new(COM1);
        // 等待 THR 空 (bit5), 加个自旋上限避免串口异常时死等。
        let mut spins = 0u32;
        while lsr.read() & 0x20 == 0 && spins < 100_000 {
            spins += 1;
        }
        data.write(c);
    }
}

/// 向 COM1 写字符串 (`\n` 补 `\r`)。
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
        FB_BASE = info.fb_addr;
        FB_BYTES = info.fb_height as u64 * info.fb_stride as u64 * (info.fb_bpp as u64 / 8);
        FB_GEOM = (info.fb_width, info.fb_height, info.fb_stride, info.fb_bpp);
    }
    bg_fill_all();
}

/// 帧缓冲是否可用 (panic/异常处理在打印前检查)
pub fn ready() -> bool {
    unsafe { FB.is_ready() }
}

/// 帧缓冲物理基址 (供 `SYS_FB_INFO` / `SYS_FB_MAP`)。
pub fn fb_base() -> u64 {
    unsafe { FB_BASE }
}

/// 帧缓冲字节数 (按 `height * stride * bpp/8` 计算, 可能与帧分配器口径一致)。
pub fn fb_bytes() -> u64 {
    unsafe { FB_BYTES }
}

/// 帧缓冲几何 `(宽, 高, 行跨度像素, 每像素位数)`。
pub fn fb_geometry() -> (u32, u32, u32, u32) {
    unsafe { FB_GEOM }
}

/// 宣告用户态**接管**显示: 之后内核不再写帧缓冲 (输出只留 COM1)。
///
/// 由 `SYS_FB_TAKEOVER` 在持 `Capability::Fb` 时调用; 幂等。
pub fn take_over() {
    FB_TAKEN_OVER.store(true, Ordering::SeqCst);
}

/// 显示是否已被用户态接管。
pub fn is_taken_over() -> bool {
    FB_TAKEN_OVER.load(Ordering::SeqCst)
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

/// 清屏并复位终端状态 (历史 / 当前行), 供 `SYS_CLEAR` 使用。
pub fn clear_screen() {
    if is_taken_over() {
        return; // 屏幕已交给用户态图形服务, 内核不再干预
    }
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    unsafe {
        HISTORY_COUNT = 0;
        HISTORY_START = 0;
        LINE = [0; LINE_BYTES];
        LINE_LEN = 0;
    }
    bg_fill_all();
    if was_enabled {
        x86_64::instructions::interrupts::enable();
    }
}

// ---------------------------------------------------------------------------
// 布局计算
// ---------------------------------------------------------------------------

/// 文本区可容纳的行数
fn visible_lines() -> usize {
    unsafe { ((FB.height() - MARGIN) / LINE_HEIGHT) as usize }
}

/// 历史区可显示的行数 (**给当前正在输出的那一行留一行**)。
fn hist_visible() -> usize {
    visible_lines().saturating_sub(1)
}

/// 每行可显示的字符数 (列数)
fn max_cols() -> usize {
    unsafe { ((FB.width() - MARGIN) / font::CHAR_WIDTH) as usize }
}

// ---------------------------------------------------------------------------
// 行历史
// ---------------------------------------------------------------------------

/// 把一行压入历史环形缓冲 (满了就挤掉最老的)。
fn history_push(bytes: &[u8]) {
    unsafe {
        let idx = (HISTORY_START + HISTORY_COUNT) % HISTORY_LINES;
        if HISTORY_COUNT < HISTORY_LINES {
            HISTORY_COUNT += 1;
        } else {
            HISTORY_START = (HISTORY_START + 1) % HISTORY_LINES;
        }
        let n = if bytes.len() > LINE_BYTES {
            LINE_BYTES
        } else {
            bytes.len()
        };
        HISTORY[idx][..n].copy_from_slice(&bytes[..n]);
        HISTORY_LEN[idx] = n;
    }
}

/// 提交当前输出行到历史, 并清空行缓冲。
fn commit_line() {
    unsafe {
        history_push(&LINE[..LINE_LEN]);
        LINE_LEN = 0;
    }
}

// ---------------------------------------------------------------------------
// 重绘
// ---------------------------------------------------------------------------

/// 画一行: 逐**字符**推进 (而非逐字节 —— 汉字是 3 字节的 UTF-8、占 2 个字符格,
/// 逐字节只会把 3 个字节当 3 个孤立字符画), 超出右边界就截断。
///
/// # Safety
/// 调用方须确保 `FB` 已初始化。
unsafe fn draw_line(bytes: &[u8], y: u32) {
    let mut x = MARGIN;
    let mut bi = 0;
    while bi < bytes.len() {
        let (cp, n) = unicode::decode(bytes, bi);
        let w = unicode::width(cp);
        if cp == 0 {
            break;
        }
        if w == 0 {
            bi += n;
            continue;
        }
        if x + w * font::CHAR_WIDTH > FB.width() {
            break;
        }
        unicode::draw(&mut FB, x, y, cp, FG);
        x += w * font::CHAR_WIDTH;
        bi += n;
    }
}

/// 重绘整个文本区: 历史里最后 `hist_visible()` 行 + 当前正在输出的那一行。
///
/// 显示已被用户态接管时是**空操作** —— 屏幕归 `gfx_srv`。
fn redraw() {
    if is_taken_over() {
        return;
    }
    unsafe {
        // 用背景渐变擦除内容区 (而非纯色), 以便背景图在每次重绘后保持。
        bg_fill_rect(0, MARGIN, FB.width(), FB.height() - MARGIN);
        let visible = hist_visible();
        let total = HISTORY_COUNT;
        let start = total.saturating_sub(visible);

        let mut y = MARGIN;
        for i in start..total {
            let idx = (HISTORY_START + i) % HISTORY_LINES;
            draw_line(&HISTORY[idx][..HISTORY_LEN[idx]], y);
            y += LINE_HEIGHT;
        }
        // 当前行紧跟历史 (历史满时正好落在给最后一行留出的位置上)。
        draw_line(&LINE[..LINE_LEN], y);
    }
}

// ---------------------------------------------------------------------------
// 输出
// ---------------------------------------------------------------------------

/// 输出互斥锁: 串行化各域的打印, 防止并发下字符交错 (多核防御)。
static PRINT_LOCK: Mutex<()> = Mutex::new(());

/// 把一个字符追加到**当前输出行** (行满则先提交另起一行)。
///
/// 「行满」按**显示列数**判断 (汉字占 2 列), 并额外受 `LINE_BYTES` 字节容量限制
/// —— 汉字 3 字节/个, 光看字节数会让一行提早或过晚换行。
fn append_cp(cp: u32) {
    let w = unicode::width(cp);
    if w == 0 {
        return; // 控制字符 / 组合符: 终端不显示, 丢弃
    }
    let mut buf = [0u8; 4];
    let n = match char::from_u32(cp) {
        Some(c) => c.encode_utf8(&mut buf).len(),
        None => return,
    };
    unsafe {
        let cols = unicode::str_width(&LINE[..LINE_LEN]) as usize;
        if LINE_LEN + n > LINE_BYTES || cols + w as usize > max_cols() {
            commit_line();
        }
        if LINE_LEN + n <= LINE_BYTES {
            LINE[LINE_LEN..LINE_LEN + n].copy_from_slice(&buf[..n]);
            LINE_LEN += n;
        }
    }
}

/// 追加一个 ASCII 字节 (LOGO 缩进等纯 ASCII 路径的便捷入口)。
fn append_char(ch: u8) {
    append_cp(ch as u32)
}

/// 追加一段文本 (`\n` 提交当前行)。
///
/// 按 `&str` 的**字符**迭代 (而非字节): `SYS_PUTS` 传进来的就是 UTF-8 字符串,
/// 汉字因此以一整个字符为单位落到行缓冲里。
fn append_text(s: &str) {
    for ch in s.chars() {
        if ch == '\n' {
            commit_line();
        } else {
            append_cp(ch as u32);
        }
    }
}

/// 打印字符串 (支持 '\n' 换行)。
///
/// 先镜像到 COM1; 显示已被用户态接管时不再碰帧缓冲, 输出到此为止。
pub fn print(s: &str) {
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let guard = PRINT_LOCK.lock();

    // 镜像到 COM1 串口, 供 headless 调试捕获。
    serial_write(s);
    // Phase 0 / P0.1: 同时进**环形日志缓冲**, 供真机 (无串口) 经 shell `dmesg` 取出。
    // 放在「显示已被接管」的提前返回**之前** —— 接管后仍要留住日志。
    crate::klog::capture_bytes(s.as_bytes());

    // 显示已被用户态接管: 不再碰帧缓冲, 输出到此为止 (串口已写)。
    if is_taken_over() {
        drop(guard);
        if was_enabled {
            x86_64::instructions::interrupts::enable();
        }
        return;
    }

    append_text(s);
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

/// 打印启动 LOGO: 整块水平居中, 一次性追加 + 单次重绘。
///
/// 注意必须按**整块**居中 (常量左缩进): 逐行各自居中的话, 每行长度不同会把图形
/// 横向错切。原先逐字符/逐行调用 `print()` 还会触发上百次整屏重绘 (启动明显卡顿),
/// 这里改为批量追加后只重绘一次。
pub fn print_logo() {
    if is_taken_over() {
        return;
    }
    let was_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let guard = PRINT_LOCK.lock();

    let cols = max_cols();
    let indent = if cols > logo::WIDTH {
        (cols - logo::WIDTH) / 2
    } else {
        0
    };

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
