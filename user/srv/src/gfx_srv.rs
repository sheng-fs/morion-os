//! 域 15 — gfx_srv (图形 / 屏幕控制台服务, G1 → G5)
//!
//! 内核把**帧缓冲**交给用户态: 本服务持 `Capability::Fb`, 先用 `SYS_FB_INFO` 取几何、
//! `SYS_FB_MAP` 把整块帧缓冲映射进自己的地址空间, 再用 `SYS_FB_TAKEOVER` 宣告接管显示 ——
//! 之后内核终端不再写帧缓冲 (它的输出只保留 COM1), 屏幕由本服务负责。
//!
//! - **G1** 起屏: 取几何 → 映射 → 画测试图案 → 回读校验 → 接管。
//! - **G2** 绘制原语: 客户端把一块**表面**同址共享过来, `GFX_OP_BLIT` 把表面拷到帧缓冲并
//!   回读校验 —— "画上去了"在无显示器时也可断言。
//! - **G3a** 文本渲染: 字库 (ASCII 8x16 + 汉字 16x16 `cjk.bin`) 与终端状态 (光标 / 换行 /
//!   滚动 / 清屏) 从内核搬到本服务 (见 [`crate::gfx`]); 客户端用 `GFX_OP_TEXT` 写字、
//!   `GFX_OP_QUERY` 问光标。落笔**逐像素写后回读**。
//! - **G4** 输入搬出内核: 键盘字节由 `kbd_srv` 经 `SYS_KEY_PUSH` 推进内核键队列, 客户端用
//!   `SYS_KEY_READ` 阻塞取键、在用户态做行编辑/回显 ([`morion::console`]); 本服务只负责
//!   把回显画到屏幕上 (含 `\b` 退格擦除)。
//! - **G5** surface 合成 / 多窗口: 本服务维护一张**窗口表**, 每个窗口有几何 + z 序 + 一块
//!   同址共享的表面; **文本控制台是窗口 0** (渲染进它的后备表面), 合成器先铺桌面背景、
//!   再按 z 序把各窗口表面 blit 到帧缓冲 (带屏幕 / 窗口边界裁剪)。于是控制台重绘**不再**
//!   擦掉客户端窗口。协议见 `morion::gfx::{GFX_OP_WIN_*, GFX_OP_PIXEL, GFX_OP_COMPOSE}`。

use crate::common::Message;
use crate::gfx::font::{CHAR_HEIGHT, CHAR_WIDTH};
use crate::gfx::term::Term;
use crate::gfx::{Fb, Rect};
use morion::gfx::{
    GfxReq, GFX_OP_BLIT, GFX_OP_CLEAR, GFX_OP_COMPOSE, GFX_OP_EXIT, GFX_OP_FILL, GFX_OP_INFO,
    GFX_OP_MOVE, GFX_OP_PING, GFX_OP_PIXEL, GFX_OP_QUERY, GFX_OP_RECT, GFX_OP_TEXT,
    GFX_OP_WIN_CREATE, GFX_OP_WIN_DESTROY, GFX_OP_WIN_FLUSH, GFX_OP_WIN_MOVE, GFX_OP_WIN_RAISE,
    GFX_REPLY_NO_SESSION, GFX_TAG,
};
use morion::syscall::*;

/// 帧缓冲在本域的映射基址: `USER_SPACE_BASE + 1 GiB`。
///
/// 放这么高是因为程序镜像、共享缓冲、用户栈都挤在 `USER_SPACE_BASE` 起的头几 MiB 内,
/// 而用户空间有 512 GiB —— 挪到 1 GiB 处远离它们, 帧缓冲再大也撞不上。
const FB_VADDR: u64 = 0x0000_0080_4000_0000;

/// 控制台窗口后备表面的映射基址: `USER_SPACE_BASE + 512 MiB`。
///
/// 远离客户端共享表面区 (`+64 MiB` 起, 最多到 `+~140 MiB`)、固定数据区与帧缓冲 (`+1 GiB`)。
/// 这块后备表面是本服务**私有**页 (不经共享), 按控制台窗口几何分页分配。
const CONSOLE_VADDR: u64 = 0x0000_0080_2000_0000;

/// 窗口表容量 (含窗口 0 = 控制台); 即最多 3 个客户端窗口。
const MAX_WINDOWS: usize = 4;

/// 窗口 id 的基址：**必须避开小整数回复码**（`0`=失败 / `1`=成功 / `2`=`GFX_REPLY_NO_SESSION`）——
/// 否则"槽 2 → id 2"会被客户端误判成"会话失效"而触发重共享。id = 基址 + 槽号（槽 0 是控制台）。
const WIN_ID_BASE: u64 = 0x100;
/// 窗口 0 = 文本控制台 (始终存在, 位于 z 序最底)。
const CONSOLE: usize = 0;

/// 单次 `GFX_OP_TEXT` 允许的最大字节数 (客户端共享页就是一页)。
const MAX_TEXT_BYTES: usize = 4096;

/// 控制台窗口底色 (深蓝黑); 也是终端背景。
const BG: u32 = 0x10_18_28;
/// **桌面背景色** (合成器底色): 没有窗口覆盖处显示它。故意与控制台底色不同,
/// 于是"窗口外 = 桌面背景"这条合成断言可以逐像素区分。
const DESK_BG: u32 = 0x20_28_38;
/// 测试图案的前景色块 (琥珀); 只用于接管前那次"映射可写"的回读校验。
const FG: u32 = 0xE0_A0_30;
/// 终端文字颜色 (浅灰)。
const TEXT_FG: u32 = 0xD0_D8_E0;

/// 域 15 入口: 取几何 → 映射 → 画图 + 回读校验 → 接管 → 建合成器, 然后常驻服务请求。
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

    // 先**探测映射** (仍在接管之前): 证明"映射可写 + 几何算得对", 否则不该宣告接管 ——
    // 映射坏了还接管, 内核控制台与屏幕会一起没。
    if !probe(&fb) {
        println("gfx: framebuffer readback FAILED");
        return;
    }

    // 探测通过才宣告接管 —— 此刻起内核终端不再写屏, 屏幕只由本服务负责。
    if sys_fb_takeover() != 1 {
        println("gfx: SYS_FB_TAKEOVER FAILED");
        return;
    }

    // 接管**之后**才整屏绘制并回读校验: 此时屏幕只有一个写者, 校验结果才是确定的。
    // (顺序不能倒过来 —— 接管前内核随时可能重绘, 会把我们要校验的像素擦成背景渐变。)
    paint(&fb);
    if !verify(&fb) {
        println("gfx: full-screen paint verify FAILED (after takeover)");
        return;
    }

    // G5: 建合成器 (含窗口 0 = 文本控制台的后备表面), 用服务内终端写一行开机横幅, 再全屏合成。
    let mut comp = match Compositor::new(fb) {
        Some(c) => c,
        None => {
            println("gfx: compositor init FAILED (console backing alloc)");
            return;
        }
    };
    let banner = b"MorionOS gfx_srv - windowed console + compositor (G5)\n";
    if !comp.console_write(banner) {
        println("gfx: banner write FAILED (console readback mismatch)");
        return;
    }
    comp.compose_all();
    print("gfx: ");
    print_u64(info.width as u64);
    print("x");
    print_u64(info.height as u64);
    println(" text console ready (kernel console detached)");
    println("gfx: compositor ready (console = window 0, z-order + clipping)");

    // G2/G3a/G5: 服务绘图 / 文本 / 窗口请求。注意请求循环在**接管与起终端之后**才启动, 故任何
    // 客户端的第一条请求都必然落在"屏幕已归用户态"之后 —— 客户端无需自己等待接管完成。
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        let (reply, exit) = handle(&mut comp, msg.tag, msg.payload.as_ptr());
        sys_reply(reply);
        if exit {
            // 自测用退出钩子: **先回复再退出** (若先退出, 请求方会等不到回复)。
            // 退出后域仍由 init 监督 —— 它会就地重启本服务 (域号不变)。
            println("gfx: exit requested (self-test), stopping for supervisor restart");
            return;
        }
    }
}

/// 一个窗口: 几何 + z 序 + 一块**同址共享**的表面。
#[derive(Clone, Copy)]
struct Win {
    used: bool,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    /// z 序 (越大越靠上; 控制台恒为 0)。
    z: u32,
    /// 表面基址 (本域视角; 客户端同址共享过来)。
    surf: u64,
    /// 表面行跨度 (像素)。
    stride: u32,
}

impl Win {
    /// 空槽。
    const fn empty() -> Win {
        Win {
            used: false,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            z: 0,
            surf: 0,
            stride: 0,
        }
    }

    /// 窗口矩形 (屏幕坐标)。
    fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, self.w, self.h)
    }
}

/// surface 合成器: 窗口表 + 桌面背景 + 文本控制台 (窗口 0)。
struct Compositor {
    fb: Fb,
    wins: [Win; MAX_WINDOWS],
    /// 下一个可用 z 序。
    next_z: u32,
    /// 控制台窗口矩形 (等于 `wins[CONSOLE]` 的几何)。
    console: Rect,
    /// 服务内文本终端: 渲染进控制台窗口的**后备表面**。
    term: Term,
}

impl Compositor {
    /// 建合成器: 分配控制台窗口的后备表面 (私有页), 建窗口 0, 清屏。
    fn new(fb: Fb) -> Option<Compositor> {
        // 控制台窗口: 从屏面右侧/底部内缩 128px, 留出可见的桌面背景带 —— 这样"窗口外 =
        // 桌面背景"在合成取证里可逐像素区分 (控制台底色与桌面底色不同)。
        let cw = fb.width.saturating_sub(128).max(CHAR_WIDTH * 20);
        let ch = fb.height.saturating_sub(128).max(CHAR_HEIGHT * 24);

        // 后备表面按页分配 (本服务私有, 不经共享)。
        let pages = (cw as u64 * ch as u64 * 4).div_ceil(4096);
        for i in 0..pages {
            if sys_alloc_page(CONSOLE_VADDR + i * 4096) != 1 {
                return None;
            }
        }
        let console_fb = Fb {
            base: CONSOLE_VADDR,
            width: cw,
            height: ch,
            stride: cw,
        };

        let mut wins = [Win::empty(); MAX_WINDOWS];
        wins[CONSOLE] = Win {
            used: true,
            x: 0,
            y: 0,
            w: cw,
            h: ch,
            z: 0,
            surf: CONSOLE_VADDR,
            stride: cw,
        };

        // 终端渲染进后备表面; 先铺底色, 并**丢弃**这次清屏产生的脏区 (紧接着会全屏合成)。
        let mut term = Term::new(console_fb, TEXT_FG, BG);
        term.clear();
        let _ = term.take_damage();

        Some(Compositor {
            fb,
            wins,
            next_z: 1,
            console: Rect::new(0, 0, cw, ch),
            term,
        })
    }

    /// 整屏矩形。
    fn full_rect(&self) -> Rect {
        Rect::new(0, 0, self.fb.width, self.fb.height)
    }

    /// 全屏重合成 (桌面背景 + 所有窗口按 z 序)。
    fn compose_all(&mut self) -> bool {
        self.composite(self.full_rect())
    }

    /// 合成给定屏幕矩形: 先铺桌面背景, 再按 z 升序把各窗口表面 blit 上来 (裁剪到矩形内)。
    ///
    /// 返回 `false` 表示某个窗口的像素**回读校验**失败 (合成没真正落到帧缓冲)。
    fn composite(&self, r: Rect) -> bool {
        let r = r.intersect(self.full_rect());
        if r.w == 0 || r.h == 0 {
            return true;
        }
        // 1) 桌面背景
        self.fb.rect(r.x, r.y, r.w, r.h, DESK_BG);
        // 2) 窗口按 z 升序 (窗口数很少, 用插入排序; 不引入分配)
        let mut order = [0usize; MAX_WINDOWS];
        let mut n = 0usize;
        for (i, win) in self.wins.iter().enumerate() {
            if win.used {
                order[n] = i;
                n += 1;
            }
        }
        for a in 1..n {
            let mut b = a;
            while b > 0 && self.wins[order[b - 1]].z > self.wins[order[b]].z {
                order.swap(b - 1, b);
                b -= 1;
            }
        }
        // 3) 逐窗口覆盖 (屏幕边界 = r, 窗口边界 = win.rect(), 取交集即完成双向裁剪)
        for &idx in order[..n].iter() {
            let win = self.wins[idx];
            let ir = r.intersect(win.rect());
            if ir.w == 0 || ir.h == 0 {
                continue;
            }
            if !self.blit_win(&win, ir) {
                return false;
            }
        }
        true
    }

    /// 把窗口表面在 `ir` (已在窗口与屏幕内) 区域拷到帧缓冲, 并回读中心像素校验。
    fn blit_win(&self, win: &Win, ir: Rect) -> bool {
        for y in ir.y..ir.y + ir.h {
            let srow = (y - win.y) as u64 * win.stride as u64;
            for x in ir.x..ir.x + ir.w {
                let off = (srow + (x - win.x) as u64) * 4;
                let c = unsafe { *((win.surf + off) as *const u32) } & 0x00FF_FFFF;
                self.fb.pixel(x, y, c);
            }
        }
        // 回读中心一个像素: 证明这块窗口确实被合成上了帧缓冲。
        let sx = ir.x + ir.w / 2;
        let sy = ir.y + ir.h / 2;
        let off = ((sy - win.y) as u64 * win.stride as u64 + (sx - win.x) as u64) * 4;
        let want = unsafe { *((win.surf + off) as *const u32) } & 0x00FF_FFFF;
        self.fb.read(sx, sy) == want
    }

    /// 往控制台窗口写文本并重铺脏区; 成功 (写后回读 + 合成回读都对) 返回 `true`。
    fn console_write(&mut self, bytes: &[u8]) -> bool {
        if !self.term.write(bytes) {
            return false;
        }
        self.flush_damage()
    }

    /// 清控制台窗口 (铺底色) 并重铺脏区。
    fn console_clear(&mut self) -> bool {
        self.term.clear();
        self.flush_damage()
    }

    /// 把终端累积的脏区 (相对后备表面) 平移成屏幕坐标并重合成。
    fn flush_damage(&mut self) -> bool {
        let d = self.term.take_damage();
        if d.w == 0 || d.h == 0 {
            return true;
        }
        let r = Rect::new(self.console.x + d.x, self.console.y + d.y, d.w, d.h);
        self.composite(r)
    }

    /// 终端光标位置 `(行, 列)`。
    fn cursor(&self) -> (u32, u32) {
        self.term.cursor()
    }

    /// 定位终端光标 (越界拒绝)。
    fn move_cursor(&mut self, col: u32, row: u32) -> bool {
        self.term.move_to(col, row)
    }

    /// 建窗口: 校验共享表面已映射, 取一个空槽, 记几何/表面, 置顶并合成。
    fn win_create(&mut self, req: &GfxReq) -> u64 {
        let (w, h, stride) = (req.w as u32, req.h as u32, req.stride as u32);
        if req.buf == 0 || w == 0 || h == 0 || stride < w {
            return 0;
        }
        // 表面必须已映射进本域 (合并时读它才不会缺页)。
        let last = req.buf + (h as u64 - 1) * stride as u64 * 4 + (w as u64 - 1) * 4;
        if sys_virt_to_phys(req.buf) == 0 || sys_virt_to_phys(last) == 0 {
            return GFX_REPLY_NO_SESSION;
        }
        let slot = match (1..MAX_WINDOWS).find(|&i| !self.wins[i].used) {
            Some(s) => s,
            None => return 0,
        };
        let z = self.next_z;
        self.next_z += 1;
        self.wins[slot] = Win {
            used: true,
            x: req.x as u32,
            y: req.y as u32,
            w,
            h,
            z,
            surf: req.buf,
            stride,
        };
        if self.composite(self.wins[slot].rect()) {
            WIN_ID_BASE + slot as u64
        } else {
            0
        }
    }

    /// 解析窗口 id (`color` 字段, 形如 `WIN_ID_BASE + 槽号`); 越界/未用/控制台返回 `None`。
    fn win_slot(&self, req: &GfxReq) -> Option<usize> {
        let id = req.color;
        if id < WIN_ID_BASE {
            return None;
        }
        let slot = (id - WIN_ID_BASE) as usize;
        if slot == 0 || slot >= MAX_WINDOWS || !self.wins[slot].used {
            return None;
        }
        Some(slot)
    }

    /// 移动窗口 (重合成旧 + 新矩形)。
    fn win_move(&mut self, req: &GfxReq) -> u64 {
        let id = match self.win_slot(req) {
            Some(i) => i,
            None => return 0,
        };
        let old = self.wins[id].rect();
        self.wins[id].x = req.x as u32;
        self.wins[id].y = req.y as u32;
        let new = self.wins[id].rect();
        if self.composite(old.union(new)) {
            1
        } else {
            0
        }
    }

    /// 把窗口置顶并重合成。
    fn win_raise(&mut self, req: &GfxReq) -> u64 {
        let id = match self.win_slot(req) {
            Some(i) => i,
            None => return 0,
        };
        self.wins[id].z = self.next_z;
        self.next_z += 1;
        if self.composite(self.wins[id].rect()) {
            1
        } else {
            0
        }
    }

    /// 销毁窗口 (释放槽并重合成其矩形)。
    fn win_destroy(&mut self, req: &GfxReq) -> u64 {
        let id = match self.win_slot(req) {
            Some(i) => i,
            None => return 0,
        };
        let r = self.wins[id].rect();
        self.wins[id] = Win::empty();
        if self.composite(r) {
            1
        } else {
            0
        }
    }

    /// 重新合成某窗口 (在其表面里画完后上屏)。
    fn win_flush(&mut self, req: &GfxReq) -> u64 {
        let id = match self.win_slot(req) {
            Some(i) => i,
            None => return 0,
        };
        if self.composite(self.wins[id].rect()) {
            1
        } else {
            0
        }
    }
}

/// 处理一条绘图/文本/窗口请求, 返回 `(回复值, 是否请求退出)`。
fn handle(comp: &mut Compositor, tag: u64, payload: *const u8) -> (u64, bool) {
    if tag != GFX_TAG {
        return (u64::MAX, false);
    }
    let req: GfxReq = unsafe { core::ptr::read_unaligned(payload as *const GfxReq) };
    // 坐标为 u64, 一律先夹到 u32 可表示的范围 (屏幕坐标本就很小), 免得截断成怪值。
    if req.x > u32::MAX as u64
        || req.y > u32::MAX as u64
        || req.w > u32::MAX as u64
        || req.h > u32::MAX as u64
    {
        return (0, false);
    }
    let reply = match req.op {
        GFX_OP_FILL => {
            comp.fb.fill(req.color as u32);
            1
        }
        GFX_OP_RECT => {
            comp.fb.rect(
                req.x as u32,
                req.y as u32,
                req.w as u32,
                req.h as u32,
                req.color as u32,
            );
            1
        }
        GFX_OP_BLIT => blit_to_screen(&comp.fb, &req),
        GFX_OP_TEXT => text(comp, &req),
        GFX_OP_CLEAR => {
            if comp.console_clear() {
                1
            } else {
                0
            }
        }
        GFX_OP_MOVE => {
            if comp.move_cursor(req.x as u32, req.y as u32) {
                1
            } else {
                0
            }
        }
        GFX_OP_QUERY => {
            let (row, col) = comp.cursor();
            ((row as u64) << 32) | col as u64
        }
        GFX_OP_WIN_CREATE => comp.win_create(&req),
        GFX_OP_WIN_MOVE => comp.win_move(&req),
        GFX_OP_WIN_RAISE => comp.win_raise(&req),
        GFX_OP_WIN_DESTROY => comp.win_destroy(&req),
        GFX_OP_WIN_FLUSH => comp.win_flush(&req),
        GFX_OP_PIXEL => {
            if req.x as u32 >= comp.fb.width || req.y as u32 >= comp.fb.height {
                u64::MAX
            } else {
                comp.fb.read(req.x as u32, req.y as u32) as u64
            }
        }
        GFX_OP_COMPOSE => {
            if comp.compose_all() {
                1
            } else {
                0
            }
        }
        GFX_OP_INFO => ((comp.fb.width as u64) << 32) | comp.fb.height as u64,
        // 自测用: 回 1, 由调用方在回复后退出 (见请求循环)。
        GFX_OP_EXIT => 1,
        GFX_OP_PING => 1,
        _ => 0,
    };
    (reply, req.op == GFX_OP_EXIT)
}

/// 把客户端共享过来的文本写进终端 (落笔逐像素回读校验 + 合成回读); 成功返回 1。
///
/// 文本页由客户端 `SYS_SHARE_PAGE` **同址**共享过来, 故这里可直接按 `req.buf` 读。读之前
/// 先确认首尾字节**落在本域已映射的页里** (`SYS_VIRT_TO_PHYS` 反映的是调用方 = 本域的映射);
/// 不在映射里多半是**本服务刚重启过** (旧共享映射已随 `reset` 消失), 回
/// [`GFX_REPLY_NO_SESSION`] 让客户端重建共享后重试。
fn text(comp: &mut Compositor, req: &GfxReq) -> u64 {
    let len = req.w as usize;
    if req.buf == 0 || len == 0 || len > MAX_TEXT_BYTES {
        return 0;
    }
    let last = req.buf + len as u64 - 1;
    if sys_virt_to_phys(req.buf) == 0 || sys_virt_to_phys(last) == 0 {
        return GFX_REPLY_NO_SESSION;
    }
    let bytes = unsafe { core::slice::from_raw_parts(req.buf as *const u8, len) };
    if comp.console_write(bytes) {
        1
    } else {
        0
    }
}

/// 把客户端表面**直接**拷到屏幕并回读校验; 成功返回 1。
///
/// 这是 G2 的屏幕级原语 (`GFX_OP_BLIT`), 绕开窗口模型直接写帧缓冲 —— 供不需要窗口的
/// 客户端使用。表面由客户端 `SYS_SHARE_PAGE` **同址**共享过来, 故这里可直接按 `req.buf` 读。
/// 读之前先确认首尾像素**落在本域已映射的页里**; 不在映射里回 [`GFX_REPLY_NO_SESSION`]
/// (多半是本服务刚重启过), 让客户端重建共享。
fn blit_to_screen(fb: &Fb, req: &GfxReq) -> u64 {
    let (sw, sh, sstride) = (req.w, req.h, req.stride);
    if req.buf == 0 || sw == 0 || sh == 0 || sstride == 0 {
        return 0;
    }
    let src = req.buf;
    let last = src + (sh - 1) * sstride * 4 + (sw - 1) * 4;
    if sys_virt_to_phys(src) == 0 || sys_virt_to_phys(last) == 0 {
        return GFX_REPLY_NO_SESSION;
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

/// 映射探测: 证明这块帧缓冲映射**确实可写** (而不是画到空气里)。
///
/// 只动**内核终端不会写**的顶部带 (`y < MARGIN`, 即 `0` 那一行): 接管之前内核还在往帧缓冲
/// 整幅重绘日志 (它的文本区自 `y = MARGIN` 起), 探测点若落在文本区, 内核一次重绘就能把它
/// 擦成背景渐变 —— 那是**假失败**, 正是这个竞态迫使我们先探测、后接管、最后才整屏绘制校验。
fn probe(fb: &Fb) -> bool {
    let (x0, x1) = (0, fb.width - 1);
    fb.pixel(x0, 0, FG);
    fb.pixel(x1, 0, BG);
    fb.read(x0, 0) == FG && fb.read(x1, 0) == BG
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
