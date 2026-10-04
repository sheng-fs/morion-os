//! libnetv — 应用侧网络库（N6.5）：UDP socket 风格 API，底层经 IPC 调 `netstack_srv`（域 21）。
//!
//! 与 libvfs 同款：负载经**共享页**传递（首次使用时分配一页并同址共享给 netstack_srv）。
//! `bind` 会先经内核登记端口归属（需 `Net` 能力），协议栈收到请求后再用 `SYS_NET_OWNER`
//! 核对发起域 —— 端口门禁因此**不可伪造**。

use crate::syscall::*;

/// netstack_srv 域号（与内核建域 / `common::NETSTACK_DOMAIN` 一致）。
pub const NETSTACK_DOMAIN: u64 = 21;

/// 套接字服务 tag / 操作码（与 `common.rs` 一致）。
const NETS_REQ_TAG: u64 = 0x4E53_544B; // "NSTK"
const NETS_OP_SOCKET: u64 = 0;
const NETS_OP_BIND: u64 = 1;
const NETS_OP_SENDTO: u64 = 2;
const NETS_OP_RECVFROM: u64 = 3;
const NETS_OP_CLOSE: u64 = 4;
const NETS_PAYLOAD_MAX: u64 = 1472;

/// 与 netstack_srv 传递负载的共享页（同址共享）。
const SHARE_VADDR: u64 = 0x0000_0080_001B_0000;

/// 套接字服务请求（与 `common::NetSReq` 逐字节一致）。
#[repr(C)]
#[derive(Clone, Copy)]
struct NetSReq {
    op: u64,
    sock: u64,
    port: u64,
    addr: u64,
    len: u64,
    buf: u64,
}

/// 负载页是否已分配并共享给 netstack_srv。
static mut INITED: bool = false;

fn ensure() -> bool {
    unsafe {
        if INITED {
            return true;
        }
        if sys_alloc_page(SHARE_VADDR) != 1 || sys_share_page(SHARE_VADDR, NETSTACK_DOMAIN) != 1 {
            return false;
        }
        INITED = true;
        true
    }
}

fn call(op: u64, sock: u64, port: u64, addr: u64, len: u64) -> u64 {
    let req = NetSReq {
        op,
        sock,
        port,
        addr,
        len,
        buf: SHARE_VADDR,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &req as *const NetSReq as *const u8,
            core::mem::size_of::<NetSReq>(),
        )
    };
    sys_call_payload(NETSTACK_DOMAIN, NETS_REQ_TAG, bytes)
}

/// 建一个 UDP socket；失败返回 0。
pub fn socket() -> u64 {
    if !ensure() {
        return 0;
    }
    call(NETS_OP_SOCKET, 0, 0, 0, 0)
}

/// 绑定本地端口 `port`：先经内核登记归属（需 `Net` 能力），再让协议栈生效。
pub fn bind(sock: u64, port: u16) -> bool {
    if !ensure() || sys_net_bind(port) != 1 {
        return false; // 内核门禁未过（无覆盖该端口的能力）
    }
    call(NETS_OP_BIND, sock, port as u64, 0, 0) == 1
}

/// 发 UDP 到 `addr:port`（`addr` 用 [`ip4`] 编码）；`payload` 拷进共享页。
pub fn sendto(sock: u64, port: u16, addr: u32, payload: &[u8]) -> bool {
    if !ensure() {
        return false;
    }
    let n = payload.len().min(NETS_PAYLOAD_MAX as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(payload.as_ptr(), SHARE_VADDR as *mut u8, n);
    }
    call(NETS_OP_SENDTO, sock, port as u64, addr as u64, n as u64) == 1
}

/// 收 UDP：有数据则拷进 `out` 并返回长度（无数据返回 0）。
pub fn recvfrom(sock: u64, out: &mut [u8]) -> u64 {
    if !ensure() {
        return 0;
    }
    let r = call(NETS_OP_RECVFROM, sock, 0, 0, 0);
    if r == 0 || r == u64::MAX {
        return 0;
    }
    let n = r.min(out.len() as u64).min(NETS_PAYLOAD_MAX);
    unsafe {
        core::ptr::copy_nonoverlapping(SHARE_VADDR as *const u8, out.as_mut_ptr(), n as usize);
    }
    n
}

/// 关闭 socket。
pub fn close(sock: u64) -> bool {
    if !ensure() {
        return false;
    }
    call(NETS_OP_CLOSE, sock, 0, 0, 0) == 1
}

/// 把 IPv4 地址编码成 `a<<24 | b<<16 | c<<8 | d`（`sendto` 的 `addr` 用）。
pub const fn ip4(a: u8, b: u8, c: u8, d: u8) -> u32 {
    ((a as u32) << 24) | ((b as u32) << 16) | ((c as u32) << 8) | d as u32
}
