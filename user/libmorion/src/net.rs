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
const NETS_OP_TSOCKET: u64 = 5;
const NETS_OP_TBIND: u64 = 6;
const NETS_OP_TCONNECT: u64 = 7;
const NETS_OP_TSEND: u64 = 8;
const NETS_OP_TRECV: u64 = 9;
const NETS_OP_TCLOSE: u64 = 10;
const NETS_OP_TLISTEN: u64 = 11;
const NETS_OP_TACCEPT: u64 = 12;
const NETS_PAYLOAD_MAX: u64 = 1472;

/// 与 netstack_srv 传递负载的共享页**基址**（同址共享）。
///
/// 每个客户端域实际用**各自的一页** = 基址 + `domain_id * 4 KiB`。netstack 按请求里的
/// `buf` 在**同址**读写，若多个客户端用同一个 VA，后共享者的页会覆盖前者 —— netstack
/// 于是读到/写到别的域的页。libvfs 用 `RESULT_BUF` / `SHELL_RESULT_BUF` 区分是同一道理。
const SHARE_BASE: u64 = 0x0000_0080_001B_0000;

/// 本域的负载共享页虚拟地址（按域派生，保证 netstack 的多个客户端互不覆盖）。
fn share_vaddr() -> u64 {
    SHARE_BASE + crate::syscall::domain_id() * 0x1000
}

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
        let va = share_vaddr();
        if sys_alloc_page(va) != 1 || sys_share_page(va, NETSTACK_DOMAIN) != 1 {
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
        buf: share_vaddr(),
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
    socket_on(0)
}

/// 建一个绑定到出口网卡 `nic` 的 UDP socket（N9.2 多网卡：0=virtio-net, 1=e1000e）。
pub fn socket_on(nic: u64) -> u64 {
    if !ensure() {
        return 0;
    }
    call(NETS_OP_SOCKET, nic, 0, 0, 0)
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
        core::ptr::copy_nonoverlapping(payload.as_ptr(), share_vaddr() as *mut u8, n);
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
        core::ptr::copy_nonoverlapping(share_vaddr() as *const u8, out.as_mut_ptr(), n as usize);
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

/// 建一个 TCP 连接（NIC 0=virtio-net）；失败返回 0。
pub fn tcp_socket() -> u64 {
    if !ensure() {
        return 0;
    }
    call(NETS_OP_TSOCKET, 0, 0, 0, 0)
}

/// 绑 TCP 本地端口 `port`：先经内核登记归属（需 `Net` 能力），再让协议栈生效。
pub fn tcp_bind(sock: u64, port: u16) -> bool {
    if !ensure() || sys_net_bind(port) != 1 {
        return false;
    }
    call(NETS_OP_TBIND, sock, port as u64, 0, 0) == 1
}

/// 主动连接 `addr:port`（`addr` 用 [`ip4`] 编码）；成功仅表示 SYN 已发出。
pub fn tcp_connect(sock: u64, addr: u32, port: u16) -> bool {
    if !ensure() {
        return false;
    }
    call(NETS_OP_TCONNECT, sock, port as u64, addr as u64, 0) == 1
}

/// 发 TCP 数据（`payload` 拷进共享页）。
pub fn tcp_send(sock: u64, payload: &[u8]) -> bool {
    if !ensure() {
        return false;
    }
    let n = payload.len().min(NETS_PAYLOAD_MAX as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(payload.as_ptr(), share_vaddr() as *mut u8, n);
    }
    call(NETS_OP_TSEND, sock, 0, 0, n as u64) == 1
}

/// 收 TCP 数据：有则拷进 `out` 并返回长度（无返回 0）。
pub fn tcp_recv(sock: u64, out: &mut [u8]) -> u64 {
    if !ensure() {
        return 0;
    }
    let r = call(NETS_OP_TRECV, sock, 0, 0, 0);
    if r == 0 || r == u64::MAX {
        return 0;
    }
    let n = r.min(out.len() as u64).min(NETS_PAYLOAD_MAX);
    unsafe {
        core::ptr::copy_nonoverlapping(share_vaddr() as *const u8, out.as_mut_ptr(), n as usize);
    }
    n
}

/// 关闭 TCP 连接。
pub fn tcp_close(sock: u64) -> bool {
    if !ensure() {
        return false;
    }
    call(NETS_OP_TCLOSE, sock, 0, 0, 0) == 1
}

/// 在 NIC 0 上建 TCP 监听者（被动打开）：先经内核登记端口归属（需 `Net` 能力），再让协议栈
/// 监听。返回监听者 id（>0）/ 0，可反复喂给 [`tcp_accept`]。
pub fn tcp_listen(port: u16) -> u64 {
    if !ensure() || sys_net_bind(port) != 1 {
        return 0;
    }
    call(NETS_OP_TLISTEN, 0, port as u64, 0, 0)
}

/// 接受一个已建立的连接（`listener` 为 [`tcp_listen`] 返回的 id）。返回服务端连接 id（>0）/ 0。
pub fn tcp_accept(listener: u64) -> u64 {
    if !ensure() {
        return 0;
    }
    call(NETS_OP_TACCEPT, listener, 0, 0, 0)
}

/// 把 IPv4 地址编码成 `a<<24 | b<<16 | c<<8 | d`（`sendto` 的 `addr` 用）。
pub const fn ip4(a: u8, b: u8, c: u8, d: u8) -> u32 {
    ((a as u32) << 24) | ((b as u32) << 16) | ((c as u32) << 8) | d as u32
}

// ---------------------------------------------------------------------------
// N8 — DNS 最小客户端（A 记录）
// ---------------------------------------------------------------------------

/// slirp 内置 DNS 服务地址（`10.0.2.3`）。
const NET_DNS: u32 = ip4(10, 0, 2, 3);
/// DNS 服务端口与本地源端口（源端口须落在应用的 `Net` 能力范围内）。
const DNS_PORT: u16 = 53;
const DNS_LOCAL_PORT: u16 = 12347;
/// 查询 id（固定值，便于解析时校验；同时用于自证）。
const DNS_ID: u16 = 0x4D4F;

fn wr8b(b: &mut [u8], p: usize, v: u8) {
    b[p] = v;
}
fn wr16b(b: &mut [u8], p: usize, v: u16) {
    b[p] = (v >> 8) as u8;
    b[p + 1] = v as u8;
}
fn rd16b(b: &[u8], p: usize) -> u16 {
    ((b[p] as u16) << 8) | b[p + 1] as u16
}

/// 组装 DNS A 查询：`name` 形如 `a.b.c`；写入 `out`，返回报文长度（0 = 非法）。
fn dns_build(name: &str, out: &mut [u8]) -> usize {
    if out.len() < 12 + name.len() + 6 || name.is_empty() {
        return 0;
    }
    // 头部：id / flags(RD) / qd=1 / 其余 0。
    wr16b(out, 0, DNS_ID);
    wr16b(out, 2, 0x0100);
    wr16b(out, 4, 1);
    wr16b(out, 6, 0);
    wr16b(out, 8, 0);
    wr16b(out, 10, 0);
    let mut p = 12usize;
    // QNAME：按 '.' 切分，每段前置长度；非法(空段/超长)则失败。
    let bytes = name.as_bytes();
    let mut start = 0usize;
    let mut idx = 0usize;
    while idx <= bytes.len() {
        if idx == bytes.len() || bytes[idx] == b'.' {
            let seg = idx - start;
            if seg == 0 || seg > 63 {
                return 0;
            }
            wr8b(out, p, seg as u8);
            p += 1;
            let mut k = 0;
            while k < seg {
                wr8b(out, p, bytes[start + k]);
                p += 1;
                k += 1;
            }
            start = idx + 1;
        }
        idx += 1;
    }
    wr8b(out, p, 0); // 根标签
    p += 1;
    wr16b(out, p, 1); // QTYPE = A
    p += 2;
    wr16b(out, p, 1); // QCLASS = IN
    p += 2;
    p
}

/// 跳过一个 DNS 名字（支持 0xC0 压缩指针）。
fn dns_skip_name(b: &[u8], mut p: usize) -> usize {
    loop {
        if p >= b.len() {
            return b.len();
        }
        let c = b[p];
        if c == 0 {
            return p + 1;
        }
        if c & 0xc0 == 0xc0 {
            return p + 2;
        }
        p += 1 + c as usize;
    }
}

/// 从 DNS 应答里取第一条 A 记录的地址。
fn dns_parse_a(b: &[u8]) -> Option<[u8; 4]> {
    if b.len() < 12 || rd16b(b, 0) != DNS_ID {
        return None;
    }
    let qd = rd16b(b, 4) as usize;
    let an = rd16b(b, 6) as usize;
    let mut p = 12usize;
    let mut i = 0;
    while i < qd {
        p = dns_skip_name(b, p);
        p += 4; // qtype + qclass
        i += 1;
    }
    let mut j = 0;
    while j < an {
        p = dns_skip_name(b, p);
        if p + 10 > b.len() {
            return None;
        }
        let rtype = rd16b(b, p);
        let rclass = rd16b(b, p + 2);
        let rdlen = rd16b(b, p + 8) as usize;
        p += 10;
        if p + rdlen > b.len() {
            return None;
        }
        if rtype == 1 && rclass == 1 && rdlen == 4 {
            return Some([b[p], b[p + 1], b[p + 2], b[p + 3]]);
        }
        p += rdlen;
        j += 1;
    }
    None
}

/// 解析器确定性自证：手工构造一个含 A 记录 `198.51.100.7` 的应答，验证能正确取出。
/// （不依赖网络，保证回归稳定。）
pub fn dns_selftest() -> Option<[u8; 4]> {
    // 头部 + 问题 "a.com" + 回答（压缩指针 0xC00C）。
    let msg: [u8; 39] = [
        0x4d, 0x4f, 0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, // header
        1, b'a', 3, b'c', b'o', b'm', 0, // QNAME
        0x00, 0x01, 0x00, 0x01, // QTYPE/QCLASS
        0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, // answer hdr
        198, 51, 100, 7, // RDATA
    ];
    dns_parse_a(&msg)
}

/// 最小 `getaddrinfo`：向 slirp 内置 DNS 查 `name` 的 A 记录。成功写入 `out_ip`。
pub fn getaddrinfo(name: &str, out_ip: &mut [u8; 4]) -> bool {
    if !ensure() {
        return false;
    }
    let mut q = [0u8; 256];
    let qn = dns_build(name, &mut q);
    if qn == 0 {
        return false;
    }
    let sock = socket();
    if sock == 0 {
        return false;
    }
    let mut ok = false;
    if bind(sock, DNS_LOCAL_PORT) && sendto(sock, DNS_PORT, NET_DNS, &q[..qn]) {
        let mut rbuf = [0u8; 512];
        let mut i = 0;
        while i < 50 {
            let n = recvfrom(sock, &mut rbuf);
            if n > 0 {
                if let Some(ip) = dns_parse_a(&rbuf[..n as usize]) {
                    *out_ip = ip;
                    ok = true;
                }
                break;
            }
            sys_sleep(20);
            i += 1;
        }
    }
    let _ = close(sock);
    ok
}
