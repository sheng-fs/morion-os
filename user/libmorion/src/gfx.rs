//! 图形客户端库（libgfx）：绘制原语 + 共享表面，服务端是用户态 `gfx_srv`
//!
//! 一条链路：本域把一块**表面**（surface，像素缓冲）分页 `SYS_ALLOC_PAGE` 出来、用
//! `SYS_SHARE_PAGE` **同址**共享给 `gfx_srv`，再用 `SYS_CALL` 发一条绘图请求；`gfx_srv`
//! 把表面拷到帧缓冲（blit）并回读校验。屏幕本身由 `gfx_srv` 独占（`Capability::Fb`），
//! 客户端**不**持有帧缓冲能力 —— 画什么由服务决定，客户端只提供像素。
//!
//! # 为什么表面放在按域错开的高地址
//!
//! `SYS_SHARE_PAGE` 是把调用方的页映射进目标域的**同一虚拟地址**。`gfx_srv` 是多个客户端
//! 共同的接收方，若所有客户端都用同一个地址共享，第二个客户端就会在服务域撞上已映射页
//! (`map_user_page` panic) —— 与 libvfs 的中转页是同一个坑。故表面地址按域 id 错开。
//! 又因为表面可达数百 KiB，错开的步长取得比表面上限大（见 [`SURFACE_STRIDE`]）。
//!
//! # 调用前提
//!
//! 本域需持有对 `gfx_srv` 的 `SendTo`（发请求）与 `MapInto`（共享表面页）。

use crate::syscall::{
    domain_id, sys_alloc_page, sys_call_payload, sys_domain_alive, sys_key_try_read,
    sys_share_page, sys_sleep, sys_virt_to_phys,
};

/// `gfx_srv` 的固定域号（域号是 ABI，与内核建域顺序一致）。
pub const GFX_DOMAIN: u64 = 15;

/// 绘图请求的 IPC tag（与 `gfx_srv` 约定）。
pub const GFX_TAG: u64 = 0x4758_4646; // "GXFF"

/// 绘图操作码（`GfxReq.op`）。
/// 用纯色铺满整屏。
pub const GFX_OP_FILL: u64 = 0;
/// 在屏幕上画一个纯色矩形。
pub const GFX_OP_RECT: u64 = 1;
/// 把客户端表面拷到屏幕（裁剪到屏幕内），服务端**回读校验**后回复结果。
pub const GFX_OP_BLIT: u64 = 2;
/// 存活探测：服务在跑就回 1。
pub const GFX_OP_PING: u64 = 3;
/// 往**终端光标**处写一段 UTF-8 文本（`buf` = 文本页地址，`w` = 字节数）。服务逐像素
/// 写后回读校验，全对才回 1。
pub const GFX_OP_TEXT: u64 = 4;

/// 清屏并把光标归零。
pub const GFX_OP_CLEAR: u64 = 5;
/// 定位光标（`x` = 列，`y` = 行）；越界回 0。
pub const GFX_OP_MOVE: u64 = 6;
/// 问光标位置，回复 `(行 << 32) | 列`。
pub const GFX_OP_QUERY: u64 = 7;
/// **自测用**: 让服务回复本请求后**退出** (监督者应把它就地重启)。生产代码不该发它。
pub const GFX_OP_EXIT: u64 = 8;

/// 创建一个窗口（`x,y,w,h` = 屏幕几何；`buf` = 表面 VA、`stride` = 表面跨度像素）。
/// 回复窗口 id（≥1）；0 = 失败、[`GFX_REPLY_NO_SESSION`] = 共享会话失效（可重建后重试）。
pub const GFX_OP_WIN_CREATE: u64 = 9;
/// 移动窗口（`color` = 窗口 id；`x,y` = 新位置）。
pub const GFX_OP_WIN_MOVE: u64 = 10;
/// 把窗口置顶（`color` = 窗口 id）。
pub const GFX_OP_WIN_RAISE: u64 = 11;
/// 销毁窗口（`color` = 窗口 id）。
pub const GFX_OP_WIN_DESTROY: u64 = 12;
/// 重新合成该窗口（`color` = 窗口 id）—— 往窗口表面里画完后用它上屏。
pub const GFX_OP_WIN_FLUSH: u64 = 13;
/// 读帧缓冲一个像素（`x,y`）；回复低 24 位 RGB，越界回 `u64::MAX`。
pub const GFX_OP_PIXEL: u64 = 14;
/// 全屏重合成（桌面背景 + 所有窗口按 z 序重铺一遍）。
pub const GFX_OP_COMPOSE: u64 = 15;
/// 问屏幕几何；回复 `(宽 << 32) | 高`。
pub const GFX_OP_INFO: u64 = 16;
/// 查询 / 设置**焦点窗口**：`color` = 窗口 id（`0` = 只查询）。
///
/// 查询回复当前焦点窗口 id（无焦点回 0）；设置成功回 1，并把该窗口同时置顶。
/// 焦点是应用侧事件循环的约定：只有焦点窗口该消费键盘（见 [`AppWindow`]）。
pub const GFX_OP_WIN_FOCUS: u64 = 17;
/// 只重绘窗口内一块**脏矩形**：`color` = 窗口 id，`x,y,w,h` = **窗口内**坐标。
///
/// 合成器只把这一小块重新铺桌面背景 + 按 z 序叠窗口，避免整窗 / 整屏重画。
/// 空矩形是合法的空操作（回 1）。
pub const GFX_OP_WIN_FLUSH_RECT: u64 = 18;

/// 服务端回复值: 请求引用的**共享页在服务域里没有映射** (通常是服务刚重启过、旧映射没了)。
///
/// 客户端据此判定"会话失效", 重建共享 (`SYS_SHARE_PAGE`) 后重试 —— 与 `u64::MAX`
/// (服务完全不可用) 区分开, 后者由 [`call`] 直接作废会话。
pub const GFX_REPLY_NO_SESSION: u64 = 2;

/// 绘图请求（序列化进 IPC payload；`repr(C)`，与 `gfx_srv` 严格对应）。
///
/// 8 × u64 = 64 字节 ≤ [`PAYLOAD_LEN`]。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GfxReq {
    /// 操作码（`GFX_OP_*`）。
    pub op: u64,
    /// 目标矩形左上角 x（屏幕坐标；fill 忽略）。
    pub x: u64,
    /// 目标矩形左上角 y（屏幕坐标；fill 忽略）。
    pub y: u64,
    /// 宽度（像素）。
    pub w: u64,
    /// 高度（像素）。
    pub h: u64,
    /// 颜色（`0x00RRGGBB`；blit 忽略）。
    pub color: u64,
    /// 表面虚拟地址（blit 用；同址共享，故服务端能直接读）。
    pub buf: u64,
    /// 表面行跨度（**像素**，blit 用）。
    pub stride: u64,
}

/// 表面基址：`USER_BASE + 64 MiB`，远离程序镜像、固定缓冲（至 `+8 MiB`）、用户栈
/// （`+4 MiB` 起 8 页）与 `gfx_srv` 的帧缓冲映射（`USER_BASE + 1 GiB`）。
const SURFACE_BASE: u64 = 0x0000_0080_0400_0000;

/// 每个域的表面地址步长（4 MiB）：必须大于单个表面上限 [`SURFACE_MAX_PAGES`]，
/// 否则相邻域的表面区间会重叠。
const SURFACE_STRIDE: u64 = 0x0040_0000;

/// 单个表面最多占用的页数（256 页 = 1 MiB）。
pub const SURFACE_MAX_PAGES: u64 = 256;

/// 本域的表面基址（按域 id 错开，见模块头注释）。
pub fn surface_va() -> u64 {
    SURFACE_BASE + domain_id() * SURFACE_STRIDE
}

/// 每个域窗口（[`SURFACE_STRIDE`] = 4 MiB）内的表面槽数：3 个 1 MiB 表面槽，尾部 1 MiB
/// 留给 [`text_va`] 的文本页。
pub const SURFACE_SLOTS: usize = 3;

/// 相邻表面槽的地址步长（1 MiB = [`SURFACE_MAX_PAGES`] 页）。
const SURFACE_SLOT_STRIDE: u64 = 0x0010_0000;

/// 第 `slot` 个表面槽在本域的虚拟地址（按域 id 错开，见模块头注释）。
///
/// 一个域可同时持有多个表面（多窗口），各占一个槽 —— 同址共享给 `gfx_srv` 时不会互相踩。
pub fn surface_slot_va(slot: usize) -> u64 {
    surface_va() + slot as u64 * SURFACE_SLOT_STRIDE
}

/// 一块客户端表面：本域内的一段连续像素缓冲，已同址共享给 `gfx_srv`。
///
/// 布局为紧凑的行主序 BGRA（每像素 4 字节，`stride` 单位像素）。一个进程可持有**多块**表面
/// （多窗口用），各占本域窗口里的一个**表面槽**（见 [`surface_slot_va`]）；[`Surface::new`]
/// 自动取一个空闲槽并共享给 `gfx_srv`。
#[derive(Clone, Copy)]
pub struct Surface {
    /// 表面基址（本域虚拟地址）。
    pub va: u64,
    pub width: u32,
    pub height: u32,
    /// 行跨度（像素）。
    pub stride: u32,
    /// 表面槽下标（决定共享就绪标志与 VA）。
    slot: usize,
}

/// 本域各表面槽是否已**就绪**（已分配并共享给 `gfx_srv`）。
///
/// 服务重启会把它域内的页表清空，之前共享过去的映射随之消失 —— 此时 [`call`] 会把整张表
/// 与 [`TEXT_READY`] 一起清掉，下一次使用由 [`ensure_shared`] 重建（只重发 `share`，不再
/// `alloc`），故重复 `share_page` 不会在服务域撞 panic。
static mut SURFACE_READY: [bool; SURFACE_SLOTS] = [false; SURFACE_SLOTS];

/// **本进程内**各表面槽是否已被占用（避免两次分配落到同一 VA）。
static mut SLOT_USED: [bool; SURFACE_SLOTS] = [false; SURFACE_SLOTS];

/// 取一个空闲表面槽并标记占用；槽耗尽返回 `None`。
fn alloc_slot() -> Option<usize> {
    unsafe {
        let mut s = 0;
        while s < SURFACE_SLOTS {
            if !SLOT_USED[s] {
                SLOT_USED[s] = true;
                return Some(s);
            }
            s += 1;
        }
    }
    None
}

impl Surface {
    /// 分配一块 `width × height` 的表面（自动取一个空闲槽）并共享给 `gfx_srv`；失败返回 `None`。
    ///
    /// 首次分配会分配并共享全部页；若服务重启过，这里会**重新共享**（页仍在本域，无需再分配）。
    pub fn new(width: u32, height: u32) -> Option<Surface> {
        let slot = alloc_slot()?;
        Surface::in_slot(slot, width, height)
    }

    /// 在指定槽内分配并共享一块表面（供 [`Window`] 等多表面场景使用）。
    fn in_slot(slot: usize, width: u32, height: u32) -> Option<Surface> {
        if width == 0 || height == 0 || slot >= SURFACE_SLOTS {
            return None;
        }
        let stride = width;
        let pages = surface_pages(height, stride);
        if pages == 0 || pages > SURFACE_MAX_PAGES {
            return None;
        }
        let va = surface_slot_va(slot);
        unsafe {
            if !SURFACE_READY[slot] {
                if !ensure_shared(va, pages) {
                    return None;
                }
                SURFACE_READY[slot] = true;
            }
        }
        Some(Surface {
            va,
            width,
            height,
            stride,
            slot,
        })
    }

    /// 写一个像素（`color` 为 `0x00RRGGBB`）。
    pub fn pixel(&self, x: u32, y: u32, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let off = (y as u64 * self.stride as u64 + x as u64) * 4;
        unsafe {
            *((self.va + off) as *mut u32) = 0xFF00_0000 | (color & 0x00FF_FFFF);
        }
    }

    /// 读一个像素（低 24 位 RGB）—— 表面在本域，可直接读。
    pub fn read(&self, x: u32, y: u32) -> u32 {
        let off = (y as u64 * self.stride as u64 + x as u64) * 4;
        unsafe { *((self.va + off) as *const u32) & 0x00FF_FFFF }
    }

    /// 用纯色铺满整块表面。
    pub fn fill(&self, color: u32) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.pixel(x, y, color);
            }
        }
    }

    /// 在表面内画一个纯色矩形（裁剪到表面内）。
    pub fn rect(&self, x0: u32, y0: u32, w: u32, h: u32, color: u32) {
        for y in y0..y0.saturating_add(h) {
            for x in x0..x0.saturating_add(w) {
                self.pixel(x, y, color);
            }
        }
    }

    /// 把整块表面拷到屏幕 `(x, y)` 处；成功（且服务端回读校验通过）返回 `true`。
    ///
    /// 若服务端回 `GFX_REPLY_NO_SESSION`（服务重启后本域表面在它那儿已失效），会**重建共享
    /// 再试一次**。
    pub fn blit(&self, x: u32, y: u32) -> bool {
        let r = self.send_blit(x, y);
        if r == 1 {
            return true;
        }
        if r == GFX_REPLY_NO_SESSION {
            unsafe {
                SURFACE_READY[self.slot] = false;
            }
            return self.send_blit(x, y) == 1;
        }
        false
    }

    /// 一次 blit 提交（必要时先重建共享）。返回服务端回复值。
    fn send_blit(&self, x: u32, y: u32) -> u64 {
        // 服务重启过就先把表面重新共享过去，否则它读不到本域这块表面。
        unsafe {
            if !SURFACE_READY[self.slot] {
                if !ensure_shared(self.va, surface_pages(self.height, self.stride)) {
                    return u64::MAX;
                }
                SURFACE_READY[self.slot] = true;
            }
        }
        let req = GfxReq {
            op: GFX_OP_BLIT,
            x: x as u64,
            y: y as u64,
            w: self.width as u64,
            h: self.height as u64,
            color: 0,
            buf: self.va,
            stride: self.stride as u64,
        };
        call(&req)
    }
}

/// 用纯色铺满整屏（服务端操作，无需表面）。
pub fn fill_screen(color: u32) -> bool {
    call(&GfxReq {
        op: GFX_OP_FILL,
        color: color as u64,
        ..Default::default()
    }) == 1
}

/// 在屏幕上画一个纯色矩形（服务端操作，无需表面）。
pub fn screen_rect(x: u32, y: u32, w: u32, h: u32, color: u32) -> bool {
    call(&GfxReq {
        op: GFX_OP_RECT,
        x: x as u64,
        y: y as u64,
        w: w as u64,
        h: h as u64,
        color: color as u64,
        ..Default::default()
    }) == 1
}

/// 探测 `gfx_srv` 是否在跑（返回 1）。
pub fn ping() -> bool {
    call(&GfxReq {
        op: GFX_OP_PING,
        ..Default::default()
    }) == 1
}

/// 文本页相对本域表面窗口的偏移：`SURFACE_SLOTS` 个表面槽（各 1 MiB）之后，文本从 +3 MiB
/// 起另开一页（不占表面槽）。
const TEXT_OFF: u64 = 0x0030_0000;

/// 一次能提交的文本上限（字节）。文本页就是一页，服务端也只收这么多。
pub const TEXT_MAX: usize = 4096;

/// 本域文本页地址（与 `gfx_srv` **同址**共享）。
pub fn text_va() -> u64 {
    surface_va() + TEXT_OFF
}

/// 本域文本页是否已**就绪**（已分配并共享）。服务重启后由 [`call`] 清掉、下次 [`print`]
/// 重建，理由同 [`SURFACE_READY`]。
static mut TEXT_READY: bool = false;

/// 往屏幕终端写一段 UTF-8 文本（当前光标处；自动换行、到底滚动）；成功返回 `true`。
///
/// 文本经**共享页**传（不塞进 IPC payload）：一行可能很长，含汉字时一字节一列对不上，
/// 走共享页就不必关心长度上限，也不需要分片。服务重启后会先重建共享（见 [`ensure_shared`]）；
/// 若服务端回 [`GFX_REPLY_NO_SESSION`]（重启发生在上次调用之后、本域还没察觉），会**重建
/// 共享再试一次**。
pub fn print(text: &str) -> bool {
    let r = send_text(text);
    if r == 1 {
        return true;
    }
    if r == GFX_REPLY_NO_SESSION {
        unsafe {
            TEXT_READY = false;
        }
        return send_text(text) == 1;
    }
    false
}

/// 一次文本提交（必要时先重建共享）。返回服务端回复值。
fn send_text(text: &str) -> u64 {
    let bytes = text.as_bytes();
    let n = bytes.len().min(TEXT_MAX);
    unsafe {
        if !TEXT_READY {
            if !ensure_shared(text_va(), 1) {
                return u64::MAX;
            }
            TEXT_READY = true;
        }
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), text_va() as *mut u8, n);
    }
    call(&GfxReq {
        op: GFX_OP_TEXT,
        buf: text_va(),
        w: n as u64,
        ..Default::default()
    })
}

/// 清屏并把光标归零。
pub fn clear_screen() -> bool {
    call(&GfxReq {
        op: GFX_OP_CLEAR,
        ..Default::default()
    }) == 1
}

/// 把光标移到字符格 `(col, row)`；越界返回 `false`（服务端不夹取，直接拒）。
pub fn move_cursor(col: u32, row: u32) -> bool {
    call(&GfxReq {
        op: GFX_OP_MOVE,
        x: col as u64,
        y: row as u64,
        ..Default::default()
    }) == 1
}

/// 问服务端的光标位置 `(col, row)`；服务不可用时返回 `None`。
pub fn cursor() -> Option<(u32, u32)> {
    let r = call(&GfxReq {
        op: GFX_OP_QUERY,
        ..Default::default()
    });
    if r == u64::MAX {
        return None;
    }
    Some(((r & 0xFFFF_FFFF) as u32, (r >> 32) as u32))
}

/// **自测用**：让 `gfx_srv` 回复本请求后退出（监督者应把它就地重启）。
///
/// 只服务于「服务崩溃 → 监督重启 → 客户端会话重建」这条链路的取证；生产代码不该发它。
/// 注意服务是**先回复再退出**，故本调用返回 `true` 时它可能刚好开始退出。
pub fn exit_server() -> bool {
    call(&GfxReq {
        op: GFX_OP_EXIT,
        ..Default::default()
    }) == 1
}

/// 发一条绘图请求给 `gfx_srv`，返回回复值（`1` = 成功）。
///
/// 回复 `u64::MAX` 表示服务**不可用**（内核判定目标域已无存活任务，见 `ipc::call`）——
/// 此时把共享会话标记为失效：服务重启会清空它域内的页表，之前共享过去的映射随之消失，
/// 下一次使用必须由 [`ensure_shared`] 重发 `SYS_SHARE_PAGE`。
fn call(req: &GfxReq) -> u64 {
    let payload = unsafe {
        core::slice::from_raw_parts(
            req as *const GfxReq as *const u8,
            core::mem::size_of::<GfxReq>(),
        )
    };
    let r = sys_call_payload(GFX_DOMAIN, GFX_TAG, payload);
    if r == u64::MAX {
        unsafe {
            let mut s = 0;
            while s < SURFACE_SLOTS {
                SURFACE_READY[s] = false;
                s += 1;
            }
            TEXT_READY = false;
        }
    }
    r
}

/// 表面占用的页数（`stride` 单位像素）。
fn surface_pages(height: u32, stride: u32) -> u64 {
    (height as u64 * stride as u64 * 4).div_ceil(4096)
}

/// 逐页确保本域的页**共享映射**进了 `gfx_srv`（**同址**）。
///
/// 与"首次分配"的区别：已经映射在**本域**的页（`SYS_VIRT_TO_PHYS != 0`）不再 `alloc`，
/// 只重发 `share` —— 这正是"服务重启后重建会话"要的（页仍在本域，只是服务侧的映射没了）。
///
/// 重发 `share` 前必须确认 `gfx_srv` 已**重启完成**：它 `reset` 之后旧映射才被清掉，这时
/// 重发才是安全的（否则会在服务域撞上已映射页而 panic）。故先有界等待它活过来。
unsafe fn ensure_shared(va: u64, pages: u64) -> bool {
    if !wait_alive(GFX_DOMAIN) {
        return false;
    }
    for i in 0..pages {
        let p = va + i * 4096;
        if sys_virt_to_phys(p) == 0 && sys_alloc_page(p) != 1 {
            return false;
        }
        if sys_share_page(p, GFX_DOMAIN) != 1 {
            return false;
        }
    }
    true
}

/// 有界等待 `domain` 有存活任务（最多 [`ALIVE_WAIT_MS`]）。
///
/// 用于"服务正在被监督者重启"的窗口：等它回来再重建共享，等不到就如实返回失败。
fn wait_alive(domain: u64) -> bool {
    let mut waited = 0u64;
    while sys_domain_alive(domain) == 0 {
        if waited >= ALIVE_WAIT_MS {
            return false;
        }
        sys_sleep(1);
        waited += 1;
    }
    true
}

/// [`wait_alive`] 的等待上限（ms）。
const ALIVE_WAIT_MS: u64 = 1000;

/// 表面页是否已映射（供调用方判断能否复用，避免重复 `alloc_page` panic）。
pub fn surface_mapped() -> bool {
    sys_virt_to_phys(surface_va()) != 0
}

/// 一个客户端窗口：一块共享表面 + 屏幕矩形，由 `gfx_srv` 的**合成器**按 z 序合成上屏。
///
/// 生命周期：`create` 建立窗口并取一块共享表面 → 往 [`Window::surface`] 里画 →
/// [`Window::present`] 让合成器上屏 → 可 [`Window::move_to`] / [`Window::raise`] /
/// [`Window::destroy`]。
///
/// ⚠️ 窗口表在 `gfx_srv` 域内，服务重启即清空: 重启后旧 [`Window`] 的 `id` 失效，须重新
/// `create`（[`ensure_shared`] 的会话重建只恢复**共享表面**，不恢复窗口）。
pub struct Window {
    /// 服务端窗口 id（≥1；0 保留给文本控制台窗口）。
    pub id: u64,
    surface: Surface,
    x: u32,
    y: u32,
}

impl Window {
    /// 在屏幕 `(x, y)` 处创建一个 `width × height` 的窗口并取一块共享表面；失败返回 `None`。
    ///
    /// 新窗口位于**最上层**；窗口矩形可超出屏幕（服务端按屏幕边界裁剪）。
    pub fn create(x: u32, y: u32, width: u32, height: u32) -> Option<Window> {
        let surface = Surface::new(width, height)?;
        let mut r = send_win_create(&surface, x, y, width, height);
        if r == GFX_REPLY_NO_SESSION {
            // 服务重启落在"共享之后、建窗之前": 重建共享再试一次。
            unsafe {
                SURFACE_READY[surface.slot] = false;
            }
            r = send_win_create(&surface, x, y, width, height);
        }
        if r == 0 || r == u64::MAX {
            return None;
        }
        Some(Window {
            id: r,
            surface,
            x,
            y,
        })
    }

    /// 底层共享表面（往它里面画，再 [`Window::present`] 上屏）。
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    /// 窗口当前位置 `(x, y)`。
    pub fn position(&self) -> (u32, u32) {
        (self.x, self.y)
    }

    /// 窗口表面尺寸 `(宽, 高)`。
    pub fn size(&self) -> (u32, u32) {
        (self.surface.width, self.surface.height)
    }

    /// 本窗口当前是否是焦点窗口（向服务端查询）。
    pub fn focused(&self) -> bool {
        call(&GfxReq {
            op: GFX_OP_WIN_FOCUS,
            color: 0,
            ..Default::default()
        }) == self.id
    }

    /// 请求把本窗口设为焦点（服务端同时把它置顶）；成功返回 `true`。
    pub fn request_focus(&self) -> bool {
        win_call(GFX_OP_WIN_FOCUS, self.id, 0, 0) == 1
    }

    /// 让合成器只重铺窗口内矩形 `(x, y, w, h)`（**窗口内**坐标）；成功返回 `true`。
    ///
    /// 与服务端回读校验的 `present` 走同一条合成路径，只是范围更小。
    pub fn flush_rect(&self, x: u32, y: u32, w: u32, h: u32) -> bool {
        win_call_rect(GFX_OP_WIN_FLUSH_RECT, self.id, x, y, w, h) == 1
    }

    /// 让合成器把本窗口（连同其下各层）重新合成到屏幕；画完表面后调用。
    pub fn present(&self) -> bool {
        win_call(GFX_OP_WIN_FLUSH, self.id, 0, 0) == 1
    }

    /// 把窗口移到 `(x, y)`。
    pub fn move_to(&mut self, x: u32, y: u32) -> bool {
        if win_call(GFX_OP_WIN_MOVE, self.id, x, y) == 1 {
            self.x = x;
            self.y = y;
            true
        } else {
            false
        }
    }

    /// 把窗口置顶（z 序最高）。
    pub fn raise(&self) -> bool {
        win_call(GFX_OP_WIN_RAISE, self.id, 0, 0) == 1
    }

    /// 销毁窗口（服务端窗口表槽释放；本域的表面页仍保留）。
    pub fn destroy(&self) -> bool {
        win_call(GFX_OP_WIN_DESTROY, self.id, 0, 0) == 1
    }
}

/// 发一条建窗请求（必要时先重建共享）。返回服务端回复值。
fn send_win_create(surface: &Surface, x: u32, y: u32, w: u32, h: u32) -> u64 {
    unsafe {
        if !SURFACE_READY[surface.slot] {
            if !ensure_shared(surface.va, surface_pages(surface.height, surface.stride)) {
                return u64::MAX;
            }
            SURFACE_READY[surface.slot] = true;
        }
    }
    call(&GfxReq {
        op: GFX_OP_WIN_CREATE,
        x: x as u64,
        y: y as u64,
        w: w as u64,
        h: h as u64,
        color: 0,
        buf: surface.va,
        stride: surface.stride as u64,
    })
}

/// 发一条窗口控制请求（`color` 携带窗口 id）。
fn win_call(op: u64, id: u64, x: u32, y: u32) -> u64 {
    call(&GfxReq {
        op,
        x: x as u64,
        y: y as u64,
        w: 0,
        h: 0,
        color: id,
        buf: 0,
        stride: 0,
    })
}

/// 发一条带矩形的窗口控制请求（`color` 携带窗口 id）。
fn win_call_rect(op: u64, id: u64, x: u32, y: u32, w: u32, h: u32) -> u64 {
    call(&GfxReq {
        op,
        x: x as u64,
        y: y as u64,
        w: w as u64,
        h: h as u64,
        color: id,
        buf: 0,
        stride: 0,
    })
}

/// 读帧缓冲上 `(x, y)` 处像素（低 24 位 RGB）；越界或服务不可用返回 `None`。
pub fn read_screen_pixel(x: u32, y: u32) -> Option<u32> {
    let r = call(&GfxReq {
        op: GFX_OP_PIXEL,
        x: x as u64,
        y: y as u64,
        ..Default::default()
    });
    if r == u64::MAX {
        None
    } else {
        Some(r as u32)
    }
}

/// 问屏幕几何 `(宽, 高)`；服务不可用返回 `None`。
pub fn screen_size() -> Option<(u32, u32)> {
    let r = call(&GfxReq {
        op: GFX_OP_INFO,
        ..Default::default()
    });
    if r == u64::MAX {
        return None;
    }
    Some(((r >> 32) as u32, (r & 0xFFFF_FFFF) as u32))
}

/// 触发一次**全屏重合成**（桌面背景 + 所有窗口按 z 序重铺一遍）；成功返回 `true`。
pub fn compose() -> bool {
    call(&GfxReq {
        op: GFX_OP_COMPOSE,
        ..Default::default()
    }) == 1
}

// ===========================================================================
// 控件库 + 应用侧事件循环
//
// 全部是**纯函数**：只依赖传入的 [`Surface`] / 矩形 / 主题，不持有状态、不发新 syscall。
// 应用自己保存状态（文本、选中项、滚动位置），在事件到来时重画受影响的小块。
// 文本目前只画 ASCII（与服务端 `gfx_srv` 的 8×16 字库同源）；非 ASCII 字节显示为 `?`。
// ===========================================================================

/// 一个点（表面 / 窗口内坐标）。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Point {
    pub x: u32,
    pub y: u32,
}

impl Point {
    pub const fn new(x: u32, y: u32) -> Point {
        Point { x, y }
    }
}

/// 一个矩形（`w` 或 `h` 为 0 表示空）。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// 空矩形（并集单位元）。
    pub const EMPTY: Rect = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// 点是否落在矩形内（右下边界不含）。
    pub const fn contains(&self, p: Point) -> bool {
        p.x >= self.x
            && p.y >= self.y
            && p.x < self.x.saturating_add(self.w)
            && p.y < self.y.saturating_add(self.h)
    }

    /// 交集；不相交返回空矩形。
    pub fn intersect(self, o: Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.x.saturating_add(self.w).min(o.x.saturating_add(o.w));
        let y1 = self.y.saturating_add(self.h).min(o.y.saturating_add(o.h));
        if x1 <= x0 || y1 <= y0 {
            Rect::EMPTY
        } else {
            Rect::new(x0, y0, x1 - x0, y1 - y0)
        }
    }

    /// 并集（外接矩形）；空矩形是单位元。
    pub fn union(self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = self.x.saturating_add(self.w).max(o.x.saturating_add(o.w));
        let y1 = self.y.saturating_add(self.h).max(o.y.saturating_add(o.h));
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

/// 字库字符格宽度（像素）。
pub const CHAR_WIDTH: u32 = 8;
/// 字库字符格高度（像素）。
pub const CHAR_HEIGHT: u32 = 16;
/// 列表一行的高度（字符高 + 上下留白）。
pub const LIST_ROW_HEIGHT: u32 = CHAR_HEIGHT + 4;

/// 控件配色。所有字段都是 `0x00RRGGBB`。
#[derive(Clone, Copy)]
pub struct Theme {
    /// 窗口 / 画布底色。
    pub window: u32,
    /// 面板 / 输入框底色。
    pub panel: u32,
    /// 正文颜色。
    pub fg: u32,
    /// 次要文字 / 未聚焦边框。
    pub dim: u32,
    /// 选中行 / 焦点边框。
    pub accent: u32,
    /// 选中行上的文字。
    pub accent_fg: u32,
    /// 按钮常态底色。
    pub button: u32,
    /// 按钮按下底色。
    pub button_pressed: u32,
}

impl Theme {
    /// 默认深色主题（与 `gfx_srv` 的控制台配色相近）。
    pub const DEFAULT: Theme = Theme {
        window: 0x10_18_28,
        panel: 0x18_22_34,
        fg: 0xD0_D8_E0,
        dim: 0x60_70_88,
        accent: 0x3A_6E_A8,
        accent_fg: 0xFF_FF_FF,
        button: 0x28_34_48,
        button_pressed: 0x1C_26_38,
    };
}

/// 8×16 ASCII 字模（`0x20..=0x7E`，95 个字符，每字符 16 行，高位在左）。
///
/// 与 `user/srv/src/gfx/font.rs` 的服务端字库同源 —— 客户端往自己的表面里画字时用，
/// 不必把文字发回服务端。
#[rustfmt::skip]
const FONT_ASCII: [[u8; 16]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x18, 0x3C, 0x3C, 0x3C, 0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x66, 0x66, 0x66, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x6C, 0x6C, 0xFE, 0x6C, 0x6C, 0x6C, 0xFE, 0x6C, 0x6C, 0x00, 0x00, 0x00, 0x00],
    [0x18, 0x18, 0x7C, 0xC6, 0xC2, 0xC0, 0x7C, 0x06, 0x86, 0xC6, 0x7C, 0x18, 0x18, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0xC2, 0xC6, 0x0C, 0x18, 0x30, 0x60, 0xC6, 0x86, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x38, 0x6C, 0x6C, 0x38, 0x76, 0xDC, 0xCC, 0xCC, 0xCC, 0x76, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x30, 0x30, 0x30, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x0C, 0x18, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x18, 0x0C, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x30, 0x18, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x18, 0x30, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x66, 0x3C, 0xFF, 0x3C, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x7E, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x18, 0x30, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x02, 0x06, 0x0C, 0x18, 0x30, 0x60, 0xC0, 0x80, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x7C, 0xC6, 0xCE, 0xDE, 0xF6, 0xE6, 0xC6, 0xC6, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x18, 0x38, 0x78, 0x18, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x7C, 0xC6, 0x06, 0x0C, 0x18, 0x30, 0x60, 0xC0, 0xFE, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x7C, 0xC6, 0x06, 0x06, 0x3C, 0x06, 0x06, 0xC6, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x0C, 0x1C, 0x3C, 0x6C, 0xCC, 0xFE, 0x0C, 0x0C, 0x1E, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xFE, 0xC0, 0xC0, 0xFC, 0x06, 0x06, 0x06, 0xC6, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x38, 0x60, 0xC0, 0xC0, 0xFC, 0xC6, 0xC6, 0xC6, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xFE, 0xC6, 0x06, 0x06, 0x0C, 0x18, 0x30, 0x30, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x7C, 0xC6, 0xC6, 0xC6, 0x7C, 0xC6, 0xC6, 0xC6, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x7C, 0xC6, 0xC6, 0xC6, 0x7E, 0x06, 0x06, 0x0C, 0x78, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x0E, 0x1E, 0x7C, 0x7C, 0x38, 0x00],
    [0x00, 0x00, 0x00, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x30, 0x18, 0x0C, 0x06, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0x00, 0x00, 0xFE, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x60, 0x30, 0x18, 0x0C, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x7C, 0xC6, 0xC6, 0x0C, 0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x7C, 0xC6, 0xC6, 0xDE, 0xDE, 0xDE, 0xDC, 0xC0, 0x7C, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x10, 0x38, 0x6C, 0xC6, 0xC6, 0xFE, 0xC6, 0xC6, 0xC6, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xFC, 0x66, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x66, 0xFC, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x3C, 0x66, 0xC2, 0xC0, 0xC0, 0xC0, 0xC2, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xF8, 0x6C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x6C, 0xF8, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xFE, 0x66, 0x62, 0x68, 0x78, 0x68, 0x62, 0x66, 0xFE, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xFE, 0x66, 0x62, 0x68, 0x78, 0x68, 0x60, 0x60, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x3C, 0x66, 0xC2, 0xC0, 0xC0, 0xDE, 0xC6, 0x66, 0x3A, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xC6, 0xC6, 0xC6, 0xC6, 0xFE, 0xC6, 0xC6, 0xC6, 0xC6, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x3C, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0xCC, 0xCC, 0x78, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xE6, 0x66, 0x6C, 0x6C, 0x78, 0x6C, 0x6C, 0x66, 0xE6, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xF0, 0x60, 0x60, 0x60, 0x60, 0x60, 0x62, 0x66, 0xFE, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xC6, 0xEE, 0xFE, 0xFE, 0xD6, 0xC6, 0xC6, 0xC6, 0xC6, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xC6, 0xE6, 0xF6, 0xFE, 0xDE, 0xCE, 0xC6, 0xC6, 0xC6, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x38, 0x6C, 0xC6, 0xC6, 0xC6, 0xC6, 0xC6, 0x6C, 0x38, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xFC, 0x66, 0x66, 0x66, 0x7C, 0x60, 0x60, 0x60, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x7C, 0xC6, 0xC6, 0xC6, 0xC6, 0xD6, 0xDE, 0x7C, 0x0C, 0x0E, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xFC, 0x66, 0x66, 0x66, 0x7C, 0x6C, 0x66, 0x66, 0xE6, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x7C, 0xC6, 0xC6, 0x60, 0x38, 0x0C, 0xC6, 0xC6, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x7E, 0x7E, 0x5A, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xC6, 0xC6, 0xC6, 0xC6, 0xC6, 0xC6, 0xC6, 0xC6, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xC6, 0xC6, 0xC6, 0xC6, 0xC6, 0xC6, 0x6C, 0x38, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xC6, 0xC6, 0xC6, 0xC6, 0xD6, 0xD6, 0xFE, 0x6C, 0x6C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xC6, 0xC6, 0x6C, 0x6C, 0x38, 0x6C, 0x6C, 0xC6, 0xC6, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x18, 0x18, 0x18, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xFE, 0xC6, 0x86, 0x0C, 0x18, 0x30, 0x62, 0xC6, 0xFE, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x3C, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x80, 0xC0, 0xE0, 0x70, 0x38, 0x1C, 0x0E, 0x06, 0x02, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x3C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x10, 0x38, 0x6C, 0xC6, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00],
    [0x30, 0x30, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x0C, 0x7C, 0xCC, 0xCC, 0x76, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xE0, 0x60, 0x60, 0x78, 0x6C, 0x66, 0x66, 0x66, 0xDC, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x7C, 0xC6, 0xC0, 0xC0, 0xC6, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x1C, 0x0C, 0x0C, 0x3C, 0x6C, 0xCC, 0xCC, 0xCC, 0x76, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x7C, 0xC6, 0xFE, 0xC0, 0xC6, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x1C, 0x36, 0x32, 0x30, 0x78, 0x30, 0x30, 0x30, 0x78, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x76, 0xCC, 0xCC, 0xCC, 0x7C, 0x0C, 0xCC, 0x78, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xE0, 0x60, 0x60, 0x6C, 0x76, 0x66, 0x66, 0x66, 0xE6, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x18, 0x18, 0x00, 0x38, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x06, 0x06, 0x00, 0x0E, 0x06, 0x06, 0x06, 0x06, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0xE0, 0x60, 0x60, 0x66, 0x6C, 0x78, 0x6C, 0x66, 0xE6, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x38, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xEC, 0xFE, 0xD6, 0xD6, 0xD6, 0xC6, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xDC, 0x66, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x7C, 0xC6, 0xC6, 0xC6, 0xC6, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xDC, 0x66, 0x66, 0x66, 0x7C, 0x60, 0x60, 0xF0, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x76, 0xCC, 0xCC, 0xCC, 0x7C, 0x0C, 0x0C, 0x1E, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xDC, 0x76, 0x66, 0x60, 0x60, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x7C, 0xC6, 0x70, 0x1C, 0xC6, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x10, 0x30, 0x30, 0xFC, 0x30, 0x30, 0x30, 0x36, 0x1C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0x76, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xC6, 0xC6, 0xD6, 0xD6, 0xFE, 0x6C, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xC6, 0x6C, 0x38, 0x38, 0x6C, 0xC6, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xC6, 0xC6, 0xC6, 0xC6, 0x7E, 0x06, 0x0C, 0xF8, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0xCC, 0x18, 0x30, 0x66, 0xFE, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x0E, 0x18, 0x18, 0x18, 0x70, 0x18, 0x18, 0x18, 0x0E, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x18, 0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x70, 0x18, 0x18, 0x18, 0x0E, 0x18, 0x18, 0x18, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x76, 0xDC, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
];

/// 取一个字节对应的字模；不可打印的字节退化成 `?`。
fn glyph(ch: u8) -> &'static [u8; 16] {
    let c = if (0x20..=0x7E).contains(&ch) {
        ch
    } else {
        b'?'
    };
    &FONT_ASCII[(c - 0x20) as usize]
}

/// 一段文本按 8 px/字符估算的像素宽度（非 ASCII 字节也占一格）。
pub fn text_width(text: &str) -> u32 {
    (text.len() as u32).saturating_mul(CHAR_WIDTH)
}

impl Surface {
    /// 在表面 `(x, y)` 画一个 8×16 字符；`bg = Some` 时整格铺底（便于擦除旧内容）。
    pub fn draw_char(&self, x: u32, y: u32, ch: u8, fg: u32, bg: Option<u32>) {
        let bits = glyph(ch);
        for (row, byte) in bits.iter().enumerate() {
            for col in 0..CHAR_WIDTH {
                let px = x.saturating_add(col);
                let py = y.saturating_add(row as u32);
                if byte & (0x80 >> col) != 0 {
                    self.pixel(px, py, fg);
                } else if let Some(bgc) = bg {
                    self.pixel(px, py, bgc);
                }
            }
        }
    }

    /// 在表面画一段 ASCII 文本，返回推进的像素宽度。
    pub fn draw_text(&self, x: u32, y: u32, text: &str, fg: u32, bg: Option<u32>) -> u32 {
        self.draw_text_limited(x, y, self.width.saturating_sub(x), text, fg, bg)
    }

    /// 同 [`Surface::draw_text`]，但最多占 `max_w` 像素宽（超出裁剪，不越界写表面）。
    pub fn draw_text_limited(
        &self,
        x: u32,
        y: u32,
        max_w: u32,
        text: &str,
        fg: u32,
        bg: Option<u32>,
    ) -> u32 {
        let mut pen = 0u32;
        for &b in text.as_bytes() {
            if pen.saturating_add(CHAR_WIDTH) > max_w {
                break;
            }
            self.draw_char(x + pen, y, b, fg, bg);
            pen += CHAR_WIDTH;
        }
        pen
    }
}

/// 画一个实心面板 + 1 像素边框（自动裁剪到表面）。
pub fn draw_panel(s: &Surface, r: Rect, fill: u32, border: u32) {
    if r.is_empty() {
        return;
    }
    s.rect(r.x, r.y, r.w, r.h, fill);
    s.rect(r.x, r.y, r.w, 1, border);
    s.rect(r.x, r.y + r.h - 1, r.w, 1, border);
    s.rect(r.x, r.y, 1, r.h, border);
    s.rect(r.x + r.w - 1, r.y, 1, r.h, border);
}

/// 按钮的视觉状态。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    Normal,
    Hover,
    Pressed,
    Disabled,
}

/// 画一个按钮：面板 + 居中标签。
pub fn draw_button(s: &Surface, r: Rect, label: &str, th: &Theme, state: ButtonState) {
    let (fill, fg, border) = match state {
        ButtonState::Pressed => (th.button_pressed, th.fg, th.accent),
        ButtonState::Hover => (th.button, th.accent_fg, th.accent),
        ButtonState::Disabled => (th.panel, th.dim, th.dim),
        ButtonState::Normal => (th.button, th.fg, th.dim),
    };
    draw_panel(s, r, fill, border);
    let tw = text_width(label).min(r.w);
    let tx = r.x + (r.w - tw) / 2;
    let ty = r.y + (r.h.saturating_sub(CHAR_HEIGHT)) / 2;
    s.draw_text_limited(tx, ty, r.w.saturating_sub(tx - r.x), label, fg, None);
}

/// 按钮命中测试（禁用态由调用方自行决定是否传入）。
pub fn button_hit(r: Rect, p: Point) -> bool {
    r.contains(p)
}

/// 画一个列表：面板 + 可选选中行 + 每行文字（由 `top` 决定首行条目）。
///
/// 列表本身不保存滚动位置：`top` / `selected` 都由调用方维护，这样控件保持纯函数。
pub fn draw_list(
    s: &Surface,
    r: Rect,
    items: &[&str],
    top: usize,
    selected: Option<usize>,
    th: &Theme,
) {
    if r.is_empty() {
        return;
    }
    s.rect(r.x, r.y, r.w, r.h, th.panel);
    let rows = (r.h / LIST_ROW_HEIGHT) as usize;
    for row in 0..rows {
        let idx = top + row;
        if idx >= items.len() {
            break;
        }
        let y = r.y + row as u32 * LIST_ROW_HEIGHT;
        let sel = selected == Some(idx);
        if sel {
            s.rect(
                r.x + 1,
                y,
                r.w.saturating_sub(2),
                LIST_ROW_HEIGHT,
                th.accent,
            );
        }
        let fg = if sel { th.accent_fg } else { th.fg };
        s.draw_text_limited(r.x + 6, y + 2, r.w.saturating_sub(8), items[idx], fg, None);
    }
}

/// 列表命中测试：返回**绝对条目下标**（已加 `top`）；点在空白行/面板外返回 `None`。
pub fn list_hit(r: Rect, top: usize, count: usize, p: Point) -> Option<usize> {
    if !r.contains(p) {
        return None;
    }
    let row = (p.y - r.y) / LIST_ROW_HEIGHT;
    if row >= r.h / LIST_ROW_HEIGHT {
        return None;
    }
    let idx = top + row as usize;
    if idx < count {
        Some(idx)
    } else {
        None
    }
}

/// 画一个单行文本框：面板 + 文本 + 光标竖线（`focused` 时用强调色）。
pub fn draw_text_box(s: &Surface, r: Rect, text: &str, cursor: usize, th: &Theme, focused: bool) {
    if r.is_empty() {
        return;
    }
    draw_panel(s, r, th.panel, if focused { th.accent } else { th.dim });
    let ty = r.y + (r.h.saturating_sub(CHAR_HEIGHT)) / 2;
    let inner_w = r.w.saturating_sub(8);
    s.draw_text_limited(r.x + 4, ty, inner_w, text, th.fg, None);
    if focused {
        let before = text.get(..cursor.min(text.len())).unwrap_or(text);
        let cx = (r.x + 4 + text_width(before)).min(r.x + r.w.saturating_sub(4));
        s.rect(cx, ty, 1, CHAR_HEIGHT, th.accent_fg);
    }
}

/// 文本框命中测试。
pub fn text_box_hit(r: Rect, p: Point) -> bool {
    r.contains(p)
}

/// 一个按键事件（由 [`Key::from_byte`] 从 `SYS_KEY_READ` 的字节解得）。
///
/// 键盘域目前只产 ASCII 与控制字节；`Up` / `Down` / `PageUp` / `PageDown` 是**预留**变体
/// （PS/2 键盘现在把方向键用于内核日志滚动，接线时需再约定字节）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(u8),
    Enter,
    Backspace,
    Tab,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Delete,
    PageUp,
    PageDown,
    /// Ctrl+C。
    Cancel,
    /// Ctrl+S。
    Save,
    /// Ctrl+Q。
    Quit,
    Unknown(u8),
}

impl Key {
    /// 把 `SYS_KEY_READ` 的字节翻译成键（与 `morion::console` 的控制字节约定一致）。
    pub fn from_byte(b: u8) -> Key {
        match b {
            b'\n' | b'\r' => Key::Enter,
            0x08 | 0x7F => Key::Backspace,
            b'\t' => Key::Tab,
            0x1B => Key::Escape,
            0x02 => Key::Left,
            0x06 => Key::Right,
            0x01 => Key::Home,
            0x05 => Key::End,
            0x04 => Key::Delete,
            0x03 => Key::Cancel,
            0x13 => Key::Save,
            0x11 => Key::Quit,
            0x20..=0x7E => Key::Char(b),
            _ => Key::Unknown(b),
        }
    }
}

/// 鼠标按键动作（**预留**：当前 `kbd_srv` 不产鼠标事件）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Down,
    Up,
    Move,
}

/// 一个鼠标事件（坐标是**窗口内**坐标）。
#[derive(Clone, Copy)]
pub struct MouseEvent {
    pub x: u32,
    pub y: u32,
    pub kind: MouseKind,
}

/// 应用事件循环里的一步。
#[derive(Clone, Copy)]
pub enum Event {
    Key(Key),
    Mouse(MouseEvent),
    /// 有脏矩形待重画（窗口内坐标）；应用据此重画并调用 [`AppWindow::present`]。
    Repaint(Rect),
    /// 焦点变化（`true` = 本窗口刚拿到焦点）。
    Focus(bool),
}

/// 事件轮询间隔（ms）：没有推送通道，焦点与按键都靠轮询（见 [`AppWindow::wait_event`]）。
const EVENT_POLL_MS: u64 = 10;

/// 应用侧窗口 + 事件循环：把 [`Window`] 的绘制/上屏与键盘读取收成一套固定用法。
///
/// 典型循环：
///
/// ```ignore
/// let mut app = AppWindow::open(40, 40, 640, 400)?;
/// app.request_repaint(Rect::new(0, 0, 640, 400));
/// loop {
///     match app.wait_event() {
///         Event::Repaint(_) => { draw(&app); app.present(); }
///         Event::Key(k) => { let d = handle(k); app.request_repaint(d); }
///         _ => {}
///     }
/// }
/// ```
///
/// `present` 只把累积的脏矩形交给 `gfx_srv` 重合成（[`Window::flush_rect`]），不是整屏重画。
pub struct AppWindow {
    win: Window,
    dirty: Rect,
    focused: bool,
}

impl AppWindow {
    /// 建一个 `w × h` 的窗口（位置 `(x, y)`），初始把整窗标为脏区。
    pub fn open(x: u32, y: u32, w: u32, h: u32) -> Option<AppWindow> {
        let win = Window::create(x, y, w, h)?;
        Some(AppWindow {
            win,
            dirty: Rect::new(0, 0, w, h),
            focused: false,
        })
    }

    /// 底层窗口（需要 `raise` / `move_to` / `destroy` 时用）。
    pub fn window(&self) -> &Window {
        &self.win
    }

    /// 可绘制表面。
    pub fn surface(&self) -> &Surface {
        self.win.surface()
    }

    /// 窗口表面尺寸 `(宽, 高)`。
    pub fn size(&self) -> (u32, u32) {
        self.win.size()
    }

    /// 把窗口内矩形并入待重绘脏区（超出窗口的部分会被裁掉）。
    pub fn request_repaint(&mut self, r: Rect) {
        let full = Rect::new(0, 0, self.win.size().0, self.win.size().1);
        self.dirty = self.dirty.union(r.intersect(full));
    }

    /// 把累积脏区交给合成器重铺，成功后清空脏区；失败（窗口已失效）时保留脏区。
    pub fn present(&mut self) -> bool {
        if self.dirty.is_empty() {
            return true;
        }
        let d = self.dirty;
        if self.win.flush_rect(d.x, d.y, d.w, d.h) {
            self.dirty = Rect::EMPTY;
            true
        } else {
            false
        }
    }

    /// 非阻塞取一个事件；没有事件返回 `None`。
    ///
    /// 先报告焦点变化，再报告待重绘，最后才在**有焦点时**取按键 —— 非焦点窗口不消费
    /// 键盘队列，避免抢走焦点应用的输入。
    pub fn poll_event(&mut self) -> Option<Event> {
        let focused = self.win.focused();
        if focused != self.focused {
            self.focused = focused;
            return Some(Event::Focus(focused));
        }
        if !self.dirty.is_empty() {
            return Some(Event::Repaint(self.dirty));
        }
        if focused {
            if let Some(b) = sys_key_try_read() {
                return Some(Event::Key(Key::from_byte(b)));
            }
        }
        None
    }

    /// 阻塞到有事件为止（无推送通道，内部按 [`EVENT_POLL_MS`] 轮询 + 睡眠）。
    pub fn wait_event(&mut self) -> Event {
        loop {
            if let Some(e) = self.poll_event() {
                return e;
            }
            sys_sleep(EVENT_POLL_MS);
        }
    }

    /// 销毁服务端窗口（本域表面页保留）。
    pub fn close(self) {
        self.win.destroy();
    }
}
