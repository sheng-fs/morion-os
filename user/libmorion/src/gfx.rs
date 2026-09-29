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
    domain_id, sys_alloc_page, sys_call_payload, sys_share_page, sys_virt_to_phys,
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

/// 一块客户端表面：本域内的一段连续像素缓冲，已同址共享给 `gfx_srv`。
///
/// 布局为紧凑的行主序 BGRA（每像素 4 字节，`stride` 单位像素）。表面**只有一块**（VA 由
/// 域 id 决定），故 [`Surface::new`] 在一个进程里只应调用一次；重复调用会复用已分配的页。
#[derive(Clone, Copy)]
pub struct Surface {
    /// 表面基址（本域虚拟地址）。
    pub va: u64,
    pub width: u32,
    pub height: u32,
    /// 行跨度（像素）。
    pub stride: u32,
}

/// 本域的表面页是否已分配并共享给 `gfx_srv`（重复 `share_page` 会在服务域撞 panic）。
static mut SURFACE_READY: bool = false;

impl Surface {
    /// 分配一块 `width × height` 的表面并共享给 `gfx_srv`；失败返回 `None`。
    ///
    /// 首次调用分配并共享全部页；此后复用（同一进程只支持一块表面）。
    pub fn new(width: u32, height: u32) -> Option<Surface> {
        if width == 0 || height == 0 {
            return None;
        }
        let stride = width;
        let bytes = height as u64 * stride as u64 * 4;
        let pages = bytes.div_ceil(4096);
        if pages == 0 || pages > SURFACE_MAX_PAGES {
            return None;
        }
        let va = surface_va();
        unsafe {
            if !SURFACE_READY {
                for i in 0..pages {
                    let p = va + i * 4096;
                    if sys_alloc_page(p) != 1 {
                        return None;
                    }
                    if sys_share_page(p, GFX_DOMAIN) != 1 {
                        return None;
                    }
                }
                SURFACE_READY = true;
            }
        }
        Some(Surface {
            va,
            width,
            height,
            stride,
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
    pub fn blit(&self, x: u32, y: u32) -> bool {
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
        call(&req) == 1
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

/// 发一条绘图请求给 `gfx_srv`，返回回复值（`1` = 成功）。
fn call(req: &GfxReq) -> u64 {
    let payload = unsafe {
        core::slice::from_raw_parts(
            req as *const GfxReq as *const u8,
            core::mem::size_of::<GfxReq>(),
        )
    };
    sys_call_payload(GFX_DOMAIN, GFX_TAG, payload)
}

/// 表面页是否已映射（供调用方判断能否复用，避免重复 `alloc_page` panic）。
pub fn surface_mapped() -> bool {
    sys_virt_to_phys(surface_va()) != 0
}
