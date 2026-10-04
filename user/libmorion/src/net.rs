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
const NETS_OP_SENDTO6: u64 = 13;
const NETS_OP_NETINFO: u64 = 14;
const NETS_OP_PING4: u64 = 15;
const NETS_OP_PING6: u64 = 16;
const NETS_PAYLOAD_MAX: u64 = 1472;

/// 一条链路的状态快照（与 `common::NetLinkInfo` 逐字段一致；shell `net` 命令用）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LinkInfo {
    pub nic: u64,
    pub kind: u64, // NIC_KIND_*
    pub up: u64,
    pub v4: u64, // packed a<<24 | b<<16 | c<<8 | d
    pub gw4: u64,
    pub mac: u64, // 低 48 位有效
    pub v6_up: u64,
    pub v6: [u8; 16],
    pub v6_gw: [u8; 16],
}

impl LinkInfo {
    /// 全零快照（数组初始化用）。
    pub const fn zeroed() -> LinkInfo {
        LinkInfo {
            nic: 0,
            kind: 0,
            up: 0,
            v4: 0,
            gw4: 0,
            mac: 0,
            v6_up: 0,
            v6: [0; 16],
            v6_gw: [0; 16],
        }
    }
    /// `kind` → 可读型号名。
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            1 => "virtio-net",
            2 => "e1000e",
            3 => "e1000",
            _ => "nic",
        }
    }
    pub fn is_up(&self) -> bool {
        self.up != 0
    }
    pub fn ipv4(&self) -> [u8; 4] {
        [
            (self.v4 >> 24) as u8,
            (self.v4 >> 16) as u8,
            (self.v4 >> 8) as u8,
            self.v4 as u8,
        ]
    }
    pub fn gw4(&self) -> [u8; 4] {
        [
            (self.gw4 >> 24) as u8,
            (self.gw4 >> 16) as u8,
            (self.gw4 >> 8) as u8,
            self.gw4 as u8,
        ]
    }
    pub fn mac_bytes(&self) -> [u8; 6] {
        let b = self.mac.to_le_bytes();
        [b[0], b[1], b[2], b[3], b[4], b[5]]
    }
}

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

/// 发 UDP 到 IPv6 地址 `addr`（V6.4 双栈）：目的地址（16 字节）写在共享页前 16 字节，负载紧随其后。
/// `::ffff:a.b.c.d`（[`v4_mapped`]）走 IPv4 路径；`::1` / 本机 v6 地址走栈内回环。
pub fn sendto6(sock: u64, port: u16, addr: &[u8; 16], payload: &[u8]) -> bool {
    if !ensure() {
        return false;
    }
    let n = payload.len().min(NETS_PAYLOAD_MAX as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(addr.as_ptr(), share_vaddr() as *mut u8, 16);
        core::ptr::copy_nonoverlapping(payload.as_ptr(), (share_vaddr() + 16) as *mut u8, n);
    }
    call(NETS_OP_SENDTO6, sock, port as u64, 0, n as u64) == 1
}

/// 把 IPv4 编码成 v4-mapped IPv6 地址 `::ffff:a.b.c.d`（供 [`sendto6`] 的双栈路径）。
pub const fn v4_mapped(ip: [u8; 4]) -> [u8; 16] {
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, ip[0], ip[1], ip[2], ip[3],
    ]
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
/// DNS 客户端源端口**基址**：实际端口按域派生 = `base + domain_id()`。
///
/// 内核的端口归属表**一个端口只归一个域**（`kernel/src/net.rs`），若所有客户端都用同一个
/// 源端口，先绑的域会把端口占住（引导期域不会销毁 → 归属不清），别的域再也绑不上 ——
/// 表现为「app 能解析、shell 不能」。故源端口按域错开；调用方（app / shell）的 `Net` 能力
/// 范围须覆盖 `base + 自己的域号`。
const DNS_LOCAL_PORT_BASE: u16 = 12345;

/// 本域的 DNS 源端口（`base + domain_id`）。
fn dns_local_port() -> u16 {
    DNS_LOCAL_PORT_BASE + crate::syscall::domain_id() as u16
}
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

/// 组装 DNS 查询（QTYPE 由 `qtype` 给出：1=A，28=AAAA）；写入 `out`，返回报文长度（0 = 非法）。
fn dns_build(name: &str, qtype: u16, out: &mut [u8]) -> usize {
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
    wr16b(out, p, qtype); // QTYPE
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
    let qn = dns_build(name, 1, &mut q);
    if qn == 0 {
        return false;
    }
    let sock = socket();
    if sock == 0 {
        return false;
    }
    let mut ok = false;
    if bind(sock, dns_local_port()) && sendto(sock, DNS_PORT, NET_DNS, &q[..qn]) {
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

// ---------------------------------------------------------------------------
// V6.3 — 应用侧 IPv6 地址面（IpAddr + DNS AAAA）
// ---------------------------------------------------------------------------

/// 应用侧 IP 地址（V4 = 点分四段；V6 = 16 字节）。
#[derive(Clone, Copy)]
pub enum IpAddr {
    V4([u8; 4]),
    V6([u8; 16]),
}

/// 从 DNS 应答里取第一条 AAAA 记录的地址（QTYPE 28，RDATA 16 字节）。
fn dns_parse_aaaa(b: &[u8]) -> Option<[u8; 16]> {
    if b.len() < 12 || rd16b(b, 0) != DNS_ID {
        return None;
    }
    let qd = rd16b(b, 4) as usize;
    let an = rd16b(b, 6) as usize;
    let mut p = 12usize;
    let mut i = 0;
    while i < qd {
        p = dns_skip_name(b, p);
        p += 4;
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
        if rtype == 28 && rclass == 1 && rdlen == 16 {
            let mut a = [0u8; 16];
            a.copy_from_slice(&b[p..p + 16]);
            return Some(a);
        }
        p += rdlen;
        j += 1;
    }
    None
}

/// AAAA 解析器确定性自证：手工构造含 AAAA 记录 `2001:db8::1` 的应答，验证能取出。
pub fn dns_selftest_aaaa() -> Option<[u8; 16]> {
    let msg: [u8; 51] = [
        0x4d, 0x4f, 0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, // header
        1, b'a', 3, b'c', b'o', b'm', 0, // QNAME
        0x00, 0x1c, 0x00, 0x01, // QTYPE=AAAA / QCLASS=IN
        0xc0, 0x0c, 0x00, 0x1c, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, // answer hdr
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, // RDATA 2001:db8::1
    ];
    dns_parse_aaaa(&msg)
}

/// 最小 `getaddrinfo`（AAAA）：向 slirp 内置 DNS 查 `name` 的 AAAA 记录。成功写入 `out_ip`。
pub fn getaddrinfo6(name: &str, out_ip: &mut [u8; 16]) -> bool {
    if !ensure() {
        return false;
    }
    let mut q = [0u8; 256];
    let qn = dns_build(name, 28, &mut q);
    if qn == 0 {
        return false;
    }
    let sock = socket();
    if sock == 0 {
        return false;
    }
    let mut ok = false;
    if bind(sock, dns_local_port()) && sendto(sock, DNS_PORT, NET_DNS, &q[..qn]) {
        let mut rbuf = [0u8; 512];
        let mut i = 0;
        while i < 50 {
            let n = recvfrom(sock, &mut rbuf);
            if n > 0 {
                if let Some(ip) = dns_parse_aaaa(&rbuf[..n as usize]) {
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

/// 双栈解析：先试 A，再试 AAAA。
pub fn resolve(name: &str) -> Option<IpAddr> {
    let mut v4 = [0u8; 4];
    if getaddrinfo(name, &mut v4) {
        return Some(IpAddr::V4(v4));
    }
    let mut v6 = [0u8; 16];
    if getaddrinfo6(name, &mut v6) {
        return Some(IpAddr::V6(v6));
    }
    None
}

// ---------------------------------------------------------------------------
// 链路状态 / ICMP echo / 地址字面量解析（shell `net` / `ping` / `ping6`）
// ---------------------------------------------------------------------------

/// 查询网卡 `nic` 的链路状态（`NETS_OP_NETINFO`）。
pub fn link_info(nic: u64) -> Option<LinkInfo> {
    if !ensure() {
        return None;
    }
    if call(NETS_OP_NETINFO, nic, 0, 0, 0) == 0 {
        return None;
    }
    Some(unsafe { core::ptr::read_unaligned(share_vaddr() as *const LinkInfo) })
}

/// 查询**所有**网卡的链路状态（按序写入 `out`，返回条数）。
pub fn link_infos(out: &mut [LinkInfo]) -> usize {
    if !ensure() {
        return 0;
    }
    let n = call(NETS_OP_NETINFO, u64::MAX, 0, 0, 0) as usize;
    let m = n.min(out.len());
    unsafe {
        core::ptr::copy_nonoverlapping(share_vaddr() as *const LinkInfo, out.as_mut_ptr(), m);
    }
    m
}

/// IPv4 ICMP echo（`ping`）。返回是否收到应答。
pub fn ping4(addr: [u8; 4], nic: u64) -> bool {
    if !ensure() {
        return false;
    }
    let a = ((addr[0] as u64) << 24)
        | ((addr[1] as u64) << 16)
        | ((addr[2] as u64) << 8)
        | addr[3] as u64;
    call(NETS_OP_PING4, nic, 0, a, 0) == 1
}

/// IPv6 ICMPv6 echo（`ping6`）。返回是否收到应答。
pub fn ping6(addr: &[u8; 16], nic: u64) -> bool {
    if !ensure() {
        return false;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(addr.as_ptr(), share_vaddr() as *mut u8, 16);
    }
    call(NETS_OP_PING6, nic, 0, 0, 0) == 1
}

/// 解析点分十进制 IPv4 字面量（恰好 4 段、每段 0..=255）。
pub fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut part = 0usize;
    let mut val = 0u32;
    let mut digits = 0usize;
    for c in s.bytes() {
        match c {
            b'0'..=b'9' => {
                val = val * 10 + (c - b'0') as u32;
                if val > 255 {
                    return None;
                }
                digits += 1;
            }
            b'.' => {
                if digits == 0 || part >= 3 {
                    return None;
                }
                out[part] = val as u8;
                part += 1;
                val = 0;
                digits = 0;
            }
            _ => return None,
        }
    }
    if digits == 0 || part != 3 {
        return None;
    }
    out[3] = val as u8;
    Some(out)
}

/// 把一段冒号分隔的 16 进制组解析进 `out`（从 `start` 起），返回写到的下标。
fn parse_hex_groups(s: &str, out: &mut [u16], start: usize) -> Option<usize> {
    let mut n = start;
    if s.is_empty() {
        return Some(n);
    }
    for g in s.split(':') {
        if g.is_empty() || n >= out.len() {
            return None;
        }
        let mut v: u32 = 0;
        let mut d = 0;
        for c in g.bytes() {
            let x = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a' + 10) as u32,
                b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => return None,
            };
            v = v * 16 + x;
            d += 1;
            if d > 4 {
                return None;
            }
        }
        out[n] = v as u16;
        n += 1;
    }
    Some(n)
}

/// 解析 IPv6 字面量（支持 `::` 压缩与末尾内嵌 IPv4，如 `::ffff:10.0.2.15`）。
pub fn parse_ipv6(s: &str) -> Option<[u8; 16]> {
    // 末尾内嵌 IPv4（点分）→ 先用两组 u16 表示，稍后写入末尾。
    let mut work = s;
    let mut v4_tail: Option<[u8; 4]> = None;
    if let Some(pos) = s.rfind(':') {
        let last = &s[pos + 1..];
        if last.contains('.') {
            v4_tail = Some(parse_ipv4(last)?);
            work = &s[..pos];
        }
    }
    let mut groups = [0u16; 8];
    let (left, right, compress) = match work.find("::") {
        Some(i) => (&work[..i], &work[i + 2..], true),
        None => (work, "", false),
    };
    let nl = parse_hex_groups(left, &mut groups, 0)?;
    let v4s = usize::from(v4_tail.is_some()) * 2;
    if compress {
        let mut rg = [0u16; 8];
        let nr = parse_hex_groups(right, &mut rg, 0)?;
        let used = nl + nr + v4s;
        if used > 8 {
            return None;
        }
        let rstart = nl + (8 - used);
        for (k, g) in rg[..nr].iter().enumerate() {
            groups[rstart + k] = *g;
        }
    } else if nl + v4s != 8 {
        return None;
    }
    if let Some(v4) = v4_tail {
        groups[6] = ((v4[0] as u16) << 8) | v4[1] as u16;
        groups[7] = ((v4[2] as u16) << 8) | v4[3] as u16;
    }
    let mut out = [0u8; 16];
    for (i, g) in groups.iter().enumerate() {
        out[i * 2] = (*g >> 8) as u8;
        out[i * 2 + 1] = *g as u8;
    }
    Some(out)
}
