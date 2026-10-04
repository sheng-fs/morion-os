//! 域 21 — netstack_srv：用户态网络协议栈（N6）。
//!
//! 架构（见 `docs/plan-network.md`）：
//!
//! ```text
//! 应用 ── libnetv ──▶ netstack_srv(本服务: ARP/IPv4/UDP + socket + 端口能力门禁)
//!                          │  帧级 IPC (NetReq, 帧走共享页)
//!                          ├──▶ net_srv(域 16: virtio-net)      = NIC 0
//!                          └──▶ e1000e_srv(域 22: e1000e, N9)   = NIC 1
//! ```
//!
//! **N6.5/N6.6**：实现 IPv4/UDP 与 UDP socket 服务；`bind` 时用 `SYS_NET_OWNER` 核对
//! 发起域在内核登记的端口归属（不可伪造）；支持**本机自投递**（loopback，用途确定性自测）
//! 与经真实网卡的收发（先 ARP 学网关 MAC，再发帧；收到的帧解析后投递给 socket）。
//!
//! **N9.2**：驱动/栈解耦的直接收益 —— 本服务不认识任何具体网卡，只按**网卡索引**（NIC index）
//! 通过同一套 `NetReq` 收发裸以太帧。NIC 0 = virtio-net，NIC 1 = e1000e；socket 建时可指定
//! 出口网卡，**上层 socket API 不变**（`socket_on(nic)` 只是在 `socket()` 上多带一个索引）。

use crate::common::*;
use morion::syscall::*;

// DRV-A: 网卡接线**表驱动** —— 内核把探测到的 NIC 表（域号 + IO 页 VA + 型号）写进
// [`NIC_TABLE_VADDR`] 只读页（见 `common.rs`），协议栈启动时读取；不再硬编码
// 「网卡域号 / IO 页 VA / 网卡数量」。加一台网卡只需内核探测后往表里追加一条。

/// NIC 型号 → 硬件名（`netstack: links=N (...)` 用）。
fn kind_hw_name(kind: u64) -> &'static str {
    match kind {
        NIC_KIND_VIRTIO_NET => "virtio-net",
        NIC_KIND_E1000E => "e1000e",
        NIC_KIND_E1000 => "e1000",
        _ => "nic",
    }
}

/// NIC 型号 → 驱动服务名（拉起日志沿用既有措辞：virtio-net 的服务名即 `net_srv`）。
fn kind_svc_name(kind: u64) -> &'static str {
    match kind {
        NIC_KIND_VIRTIO_NET => "net_srv",
        NIC_KIND_E1000E => "e1000e",
        NIC_KIND_E1000 => "e1000",
        _ => "nic",
    }
}

/// 本机静态配置（与网卡驱动的 DHCP 回落值一致）。
const OUR_IP: [u8; 4] = [10, 0, 2, 15];
const GW_IP: [u8; 4] = [10, 0, 2, 2];

const MAX_SOCKS: usize = 4;
const MAX_TCONS: usize = 6;
const PAY_MAX: usize = NETS_PAYLOAD_MAX as usize;

const ETH_IPV4: u16 = 0x0800;
const ETH_ARP: u16 = 0x0806;
const ETH_IPV6: u16 = 0x86DD;
const IP_PROTO_ICMP: u8 = 1;
const IP_PROTO_UDP: u8 = 17;
const ICMP_UNREACH: u8 = 3;
/// ICMPv4 echo request / reply（shell `ping`）。
const ICMP_ECHO_REQ: u8 = 8;
const ICMP_ECHO_REPLY: u8 = 0;

/// IPv6 目的 = 所有路由器组播 `ff02::2` 及其对应的组播 MAC。
const V6_ALL_ROUTERS: [u8; 16] = [0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
const V6_MAC_ALL_ROUTERS: [u8; 6] = [0x33, 0x33, 0x00, 0x00, 0x00, 0x02];
const IP6_PROTO_ICMPV6: u8 = 58;
const ICMPV6_RS: u8 = 133;
const ICMPV6_RA: u8 = 134;
const ICMPV6_NS: u8 = 135;
const ICMPV6_NA: u8 = 136;
const ICMPV6_ECHO_REQ: u8 = 128;
const ICMPV6_ECHO_REPLY: u8 = 129;
const ICMPV6_DEST_UNREACH: u8 = 1;
const ICMPV6_CODE_PORT_UNREACH: u8 = 4;
/// 构造回帧时的临时暂存偏移（调用包拷到这里，避免覆盖正在解析的帧）。
const SCRATCH_OFF: u64 = 1600;

// ---------------------------------------------------------------------------
// 字节序 / 校验和（在 u64 虚拟地址上操作）
// ---------------------------------------------------------------------------

fn rd8(a: u64) -> u8 {
    unsafe { *(a as *const u8) }
}
fn rd16be(a: u64) -> u16 {
    ((rd8(a) as u16) << 8) | rd8(a + 1) as u16
}
fn wr8(a: u64, v: u8) {
    unsafe {
        *(a as *mut u8) = v;
    }
}
fn wr16be(a: u64, v: u16) {
    wr8(a, (v >> 8) as u8);
    wr8(a + 1, v as u8);
}

/// 16 位反码校验和（RFC 1071）。
fn csum(base: u64, len: u64) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0u64;
    while i + 1 < len {
        sum += rd16be(base + i) as u32;
        i += 2;
    }
    if i < len {
        sum += (rd8(base + i) as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn mac_bytes(mac: u64) -> [u8; 6] {
    let b = mac.to_le_bytes();
    [b[0], b[1], b[2], b[3], b[4], b[5]]
}
fn put_mac(a: u64, m: [u8; 6]) {
    for (i, byte) in m.iter().enumerate() {
        wr8(a + i as u64, *byte);
    }
}

// ---------------------------------------------------------------------------
// 网卡链路（帧级通道）
// ---------------------------------------------------------------------------

/// 一条发送/接收裸以太帧的链路（对应内核交出的一台网卡驱动服务）。
#[derive(Clone, Copy)]
struct Link {
    domain: u64,
    io: u64,
    /// NIC 型号（`NIC_KIND_*`，来自 NIC 表；`net` 命令展示用）。
    kind: u64,
    mac: u64,
    gw_mac: Option<[u8; 6]>,
    up: bool,
    /// IPv6（V6.1）：SLAAC 全局地址、路由器地址、路由器 MAC、是否已配置。
    v6_global: [u8; 16],
    v6_gw: [u8; 16],
    gw6_mac: Option<[u8; 6]>,
    v6_up: bool,
}

impl Link {
    const fn down(domain: u64, io: u64, kind: u64) -> Link {
        Link {
            domain,
            io,
            kind,
            mac: 0,
            gw_mac: None,
            up: false,
            v6_global: [0; 16],
            v6_gw: [0; 16],
            gw6_mac: None,
            v6_up: false,
        }
    }
}

/// 调一次链路的帧级 ops（`NET_OP_*`）。
fn link_call(l: &Link, op: u64, len: u64) -> u64 {
    let req = NetReq { op, len, buf: l.io };
    let b = unsafe {
        core::slice::from_raw_parts(
            &req as *const NetReq as *const u8,
            core::mem::size_of::<NetReq>(),
        )
    };
    sys_call_payload(l.domain, NET_REQ_TAG, b)
}
/// 发一帧（帧已构造在链路的共享页里）。
fn link_tx(l: &Link, len: u64) -> bool {
    link_call(l, NET_OP_TX, len) == 1
}
/// 收一帧到链路共享页；返回帧长（无帧 `None`）。
fn link_rx(l: &Link) -> Option<u64> {
    let r = link_call(l, NET_OP_RX, 0);
    if r == 0 || r == u64::MAX {
        None
    } else {
        Some(r)
    }
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Sock {
    used: bool,
    port: u16,
    /// 出口网卡索引。
    nic: usize,
    len: u64,
    data: [u8; PAY_MAX],
}

impl Sock {
    const fn new() -> Sock {
        Sock {
            used: false,
            port: 0,
            nic: 0,
            len: 0,
            data: [0; PAY_MAX],
        }
    }
}

struct Stack {
    links: [Link; NIC_MAX],
    /// 实际网卡数（引导期由 NIC 表填入）。
    nics: usize,
    socks: [Sock; MAX_SOCKS],
    tcons: [TcpConn; MAX_TCONS],
}

impl Stack {
    const fn zeroed() -> Stack {
        Stack {
            links: [Link::down(0, 0, 0); NIC_MAX],
            nics: 0,
            socks: [Sock::new(); MAX_SOCKS],
            tcons: [TcpConn::new(); MAX_TCONS],
        }
    }
}

/// 整个协议栈状态放**静态区**：4 条 TCP 连接 + 4 个 UDP socket 的缓冲合计近 18 KiB，
/// 放 `run()` 的栈帧会超出 32 KiB 用户栈（N7.2 实测溢出 → 触发 pager `map_anon` 失败）。
static mut STACK: Stack = Stack::zeroed();

fn idle() -> ! {
    loop {
        sys_sleep(500);
    }
}

// ---------------------------------------------------------------------------
// 帧构造 / 解析
// ---------------------------------------------------------------------------

/// 在某链路的共享页里拼一个广播 ARP 请求问 `target` 的 MAC；返回帧长。
fn build_arp_request(l: &Link, target: [u8; 4]) -> u64 {
    let f = l.io;
    put_mac(f, [0xff; 6]);
    put_mac(f + 6, mac_bytes(l.mac));
    wr16be(f + 12, ETH_ARP);
    wr16be(f + 14, 0x0001); // htype = Ethernet
    wr16be(f + 16, ETH_IPV4); // ptype = IPv4
    wr8(f + 18, 6);
    wr8(f + 19, 4);
    wr16be(f + 20, 0x0001); // oper = request
    put_mac(f + 22, mac_bytes(l.mac));
    for (i, b) in OUR_IP.iter().enumerate() {
        wr8(f + 28 + i as u64, *b);
    }
    put_mac(f + 32, [0; 6]);
    for (i, b) in target.iter().enumerate() {
        wr8(f + 38 + i as u64, *b);
    }
    42
}

/// 把 UDP 数据报构造进链路共享页（负载从 `payload_va` 拷入），返回帧长。
fn build_udp(
    l: &Link,
    dst_mac: [u8; 6],
    dst_ip: [u8; 4],
    sport: u16,
    dport: u16,
    payload_va: u64,
    plen: u64,
) -> u64 {
    let f = l.io;
    put_mac(f, dst_mac);
    put_mac(f + 6, mac_bytes(l.mac));
    wr16be(f + 12, ETH_IPV4);
    let ip = f + 14;
    wr8(ip, 0x45);
    wr8(ip + 1, 0);
    wr16be(ip + 2, (20 + 8 + plen) as u16);
    wr16be(ip + 4, 0);
    wr16be(ip + 6, 0x4000); // DF
    wr8(ip + 8, 64);
    wr8(ip + 9, IP_PROTO_UDP);
    wr16be(ip + 10, 0);
    for (i, b) in OUR_IP.iter().enumerate() {
        wr8(ip + 12 + i as u64, *b);
        wr8(ip + 16 + i as u64, dst_ip[i]);
    }
    let c = csum(ip, 20);
    wr16be(ip + 10, c);
    let udp = ip + 20;
    wr16be(udp, sport);
    wr16be(udp + 2, dport);
    wr16be(udp + 4, (8 + plen) as u16);
    wr16be(udp + 6, 0); // UDP 校验和可选 (IPv4): 置 0 = 不校验
    unsafe {
        core::ptr::copy_nonoverlapping(
            payload_va as *const u8,
            (udp + 8) as *mut u8,
            plen as usize,
        );
    }
    14 + 20 + 8 + plen
}

// ---------------------------------------------------------------------------
// IPv6（R1 取证：RS / RA）
//
// 本段只为**取证** slirp 的 IPv6 行为（是否有 RA、前缀、DNSv6 地址），据此校准 V6.1 的 SLAAC。
// 只发一个路由请求、打印收到的通告，**不建地址/邻居状态**。
// ---------------------------------------------------------------------------

/// 由 MAC 派生 EUI-64 链路本地地址 `fe80::/64`（RFC 4291 附录 A：中间插 `ff:fe`，首字节反转 U/L 位）。
fn link_local_from_mac(mac: [u8; 6]) -> [u8; 16] {
    [
        0xfe,
        0x80,
        0,
        0,
        0,
        0,
        0,
        0,
        mac[0] ^ 0x02,
        mac[1],
        mac[2],
        0xff,
        0xfe,
        mac[3],
        mac[4],
        mac[5],
    ]
}

/// 由 MAC 派生 EUI-64 接口标识（RFC 4291 附录 A：中间插 `ff:fe`，首字节反转 U/L 位）。
fn eui64_iid(mac: [u8; 6]) -> [u8; 8] {
    [
        mac[0] ^ 0x02,
        mac[1],
        mac[2],
        0xff,
        0xfe,
        mac[3],
        mac[4],
        mac[5],
    ]
}

/// 由 RA 前缀（`/64`）+ EUI-64 组成全局地址（SLAAC）。
fn global_from_prefix(prefix: &[u8; 16], mac: [u8; 6]) -> [u8; 16] {
    let mut a = *prefix;
    let iid = eui64_iid(mac);
    let mut i = 0;
    while i < 8 {
        a[8 + i] = iid[i];
        i += 1;
    }
    a
}

/// 16 字节地址相等。
fn v6_eq(a: &[u8; 16], b: &[u8; 16]) -> bool {
    let mut i = 0;
    while i < 16 {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// 全零地址（`::`）。
fn v6_is_zero(a: &[u8; 16]) -> bool {
    v6_eq(a, &[0u8; 16])
}

/// 写一个 16 字节 IPv6 地址到共享页。
fn put_v6(a: u64, v: &[u8; 16]) {
    let mut i = 0u64;
    while i < 16 {
        wr8(a + i, v[i as usize]);
        i += 1;
    }
}

/// 从共享页读一个 16 字节 IPv6 地址。
fn get_v6(a: u64) -> [u8; 16] {
    let mut v = [0u8; 16];
    let mut i = 0u64;
    while i < 16 {
        v[i as usize] = rd8(a + i);
        i += 1;
    }
    v
}

/// IPv6 伪首部 + 之上的 16 位反码校验和（RFC 8200 §8.1，供 ICMPv6 用）。
fn csum_v6(src: &[u8; 16], dst: &[u8; 16], next: u8, seg: u64, len: u64) -> u16 {
    let mut ph = [0u8; 40];
    ph[..16].copy_from_slice(src);
    ph[16..32].copy_from_slice(dst);
    ph[32..36].copy_from_slice(&(len as u32).to_be_bytes());
    ph[39] = next;
    let p = ph.as_ptr() as u64;
    let s = csum_acc(p, 40, 0);
    let s = csum_acc(seg, len, s);
    !csum_fold(s)
}

/// 在某链路共享页里拼一个 IPv6 路由请求（RS：目的 `ff02::2`，带「源链路层地址」选项）；返回帧长。
fn build_rs(l: &Link) -> u64 {
    let f = l.io;
    let src = link_local_from_mac(mac_bytes(l.mac));
    put_mac(f, V6_MAC_ALL_ROUTERS);
    put_mac(f + 6, mac_bytes(l.mac));
    wr16be(f + 12, ETH_IPV6);
    let ip = f + 14;
    wr32be(ip, 0x6000_0000); // version 6, 流量类别/流标签 = 0
    wr16be(ip + 4, 16); // payload length = ICMPv6(8) + SLLAO(8)
    wr8(ip + 6, IP6_PROTO_ICMPV6);
    wr8(ip + 7, 255); // hop limit
    let mut i = 0u64;
    while i < 16 {
        wr8(ip + 8 + i, src[i as usize]);
        wr8(ip + 24 + i, V6_ALL_ROUTERS[i as usize]);
        i += 1;
    }
    let ic = ip + 40;
    wr8(ic, ICMPV6_RS);
    wr8(ic + 1, 0); // code
    wr16be(ic + 2, 0); // 校验和字段须先清零再计算
    wr32be(ic + 4, 0); // reserved
    wr8(ic + 8, 1); // 选项：类型 1 = 源链路层地址
    wr8(ic + 9, 1); // 长度 1 → 8 字节
    put_mac(ic + 10, mac_bytes(l.mac));
    let c = csum_v6(&src, &V6_ALL_ROUTERS, IP6_PROTO_ICMPV6, ic, 16);
    wr16be(ic + 2, c);
    14 + 40 + 16
}

/// 以 `hhhh:hhhh:…` 打印 VA 处的 16 字节 IPv6 地址（取证用）。
fn print_v6(a: u64) {
    let mut i = 0u64;
    while i < 8 {
        print_hex(rd16be(a + i * 2) as u64);
        if i != 7 {
            print(":");
        }
        i += 1;
    }
}

/// 拼一个 IPv6 邻居请求（NS：目的 `dst`，目标 `target`，带「源链路层地址」选项）；返回帧长。
fn build_ns(
    io: u64,
    mac: [u8; 6],
    src: &[u8; 16],
    dst_mac: [u8; 6],
    dst: &[u8; 16],
    target: &[u8; 16],
) -> u64 {
    put_mac(io, dst_mac);
    put_mac(io + 6, mac);
    wr16be(io + 12, ETH_IPV6);
    let ip = io + 14;
    wr32be(ip, 0x6000_0000); // version 6, 流量类别/流标签 = 0
    wr16be(ip + 4, 32); // payload = NS 头(24) + SLLAO(8)
    wr8(ip + 6, IP6_PROTO_ICMPV6);
    wr8(ip + 7, 255);
    put_v6(ip + 8, src);
    put_v6(ip + 24, dst);
    let ic = ip + 40;
    wr8(ic, ICMPV6_NS);
    wr8(ic + 1, 0); // code
    wr16be(ic + 2, 0); // 校验和字段须先清零再计算
    wr32be(ic + 4, 0); // reserved
    put_v6(ic + 8, target);
    wr8(ic + 24, 1); // 选项：源链路层地址
    wr8(ic + 25, 1);
    put_mac(ic + 26, mac);
    let c = csum_v6(src, dst, IP6_PROTO_ICMPV6, ic, 32);
    wr16be(ic + 2, c);
    14 + 40 + 32
}

/// 在 `io` 处构造一个邻居通告（NA，回应 NS）；返回帧长。
fn build_na(
    io: u64,
    mac: [u8; 6],
    src: &[u8; 16],
    dst: &[u8; 16],
    dst_mac: [u8; 6],
    target: &[u8; 16],
) -> u64 {
    put_mac(io, dst_mac);
    put_mac(io + 6, mac);
    wr16be(io + 12, ETH_IPV6);
    let ip = io + 14;
    wr32be(ip, 0x6000_0000); // version 6, 流量类别/流标签 = 0
    wr16be(ip + 4, 32); // payload = NA 头(24) + TLLAO(8)
    wr8(ip + 6, IP6_PROTO_ICMPV6);
    wr8(ip + 7, 255);
    put_v6(ip + 8, src);
    put_v6(ip + 24, dst);
    let ic = ip + 40;
    wr8(ic, ICMPV6_NA);
    wr8(ic + 1, 0); // code
    wr16be(ic + 2, 0); // 校验和字段须先清零再计算
    wr32be(ic + 4, 0x6000_0000); // flags: Solicited + Override, 其余保留位 0
    put_v6(ic + 8, target);
    wr8(ic + 24, 2); // 选项：目标链路层地址
    wr8(ic + 25, 1);
    put_mac(ic + 26, mac);
    let c = csum_v6(src, dst, IP6_PROTO_ICMPV6, ic, 32);
    wr16be(ic + 2, c);
    14 + 40 + 32
}

/// 在 ICMPv6 选项列表（自 `ic + 16` 起）里找类型为 `want` 的选项，返回 `(选项 VA, 选项长度)`。
fn first_opt(ic: u64, plen: u64, want: u8) -> Option<(u64, u64)> {
    let mut o = ic + 16;
    let end = ic + plen;
    while o + 8 <= end {
        let ot = rd8(o);
        let ol = (rd8(o + 1) as u64) * 8;
        if ol < 8 {
            break;
        }
        if ot == want {
            return Some((o, ol));
        }
        o += ol;
    }
    None
}

/// 取选项里的链路层地址（选项 +2 起的 6 字节）。
fn opt_mac(o: u64) -> [u8; 6] {
    [
        rd8(o + 2),
        rd8(o + 3),
        rd8(o + 4),
        rd8(o + 5),
        rd8(o + 6),
        rd8(o + 7),
    ]
}

/// 处理收到的 IPv6 帧（ICMPv6）：RA → SLAAC；NS → 回 NA；NA → 记录路由器。
/// 若需回帧，帧已在 `io` 处构造好，返回其长度；否则返回 0。
fn icmpv6_input(st: &mut Stack, nic: usize, io: u64, n: u64) -> u64 {
    if n < 14 + 40 {
        return 0;
    }
    let ip = io + 14;
    if (rd8(ip) >> 4) != 6 || rd8(ip + 6) != IP6_PROTO_ICMPV6 {
        return 0;
    }
    let plen = rd16be(ip + 4) as u64;
    if plen < 4 || n < 14 + 40 + plen {
        return 0;
    }
    let src = get_v6(ip + 8);
    let ic = ip + 40;
    let mac = mac_bytes(st.links[nic].mac);
    match rd8(ic) {
        ICMPV6_RA => {
            // 选项：前缀信息(3) → 前缀在选项 +16；源链路层地址(1) → 路由器 MAC。
            let prefix = match first_opt(ic, plen, 3) {
                Some((o, ol)) if ol >= 32 => get_v6(o + 16),
                _ => [0u8; 16],
            };
            print("NET16 ipv6 ra rx, prefix=");
            print_v6(prefix.as_ptr() as u64);
            println("");
            if !v6_is_zero(&prefix) {
                st.links[nic].v6_global = global_from_prefix(&prefix, mac);
                st.links[nic].v6_gw = src;
                st.links[nic].gw6_mac = first_opt(ic, plen, 1).map(|(o, _)| opt_mac(o));
                st.links[nic].v6_up = true;
            }
            0
        }
        ICMPV6_NS => {
            let target = get_v6(ic + 8);
            let ll = link_local_from_mac(mac);
            if v6_eq(&target, &ll) || v6_eq(&target, &st.links[nic].v6_global) {
                // 回应 NA：源 = 被问地址，目的 = 请求方；目的 MAC 取其 SLLAO（无则用本机 MAC）。
                let dst_mac = first_opt(ic, plen, 1)
                    .map(|(o, _)| opt_mac(o))
                    .unwrap_or(mac);
                build_na(io, mac, &target, &src, dst_mac, &target)
            } else {
                0
            }
        }
        ICMPV6_NA => {
            if v6_eq(&src, &st.links[nic].v6_gw) {
                if let Some((o, _)) = first_opt(ic, plen, 2) {
                    st.links[nic].gw6_mac = Some(opt_mac(o));
                }
            }
            0
        }
        ICMPV6_ECHO_REQ => {
            // 目的须是本机地址（ll 或 SLAAC 全局）才应答。
            let dst = get_v6(ip + 24);
            let ll = link_local_from_mac(mac);
            if v6_eq(&dst, &ll) || v6_eq(&dst, &st.links[nic].v6_global) {
                echo6_reply(io, mac, plen)
            } else {
                0
            }
        }
        ICMPV6_ECHO_REPLY => 0,
        _ => 0,
    }
}

/// NDP 确定性自证：构造一个针对本机链路本地地址的 NS，投进 `icmpv6_input`，应产出 NA
/// （校验类型 + 校验和自洽）。不依赖真实对端。
fn ndp_selftest(st: &mut Stack, nic: usize) -> bool {
    let io = st.links[nic].io;
    let mac = mac_bytes(st.links[nic].mac);
    let ll = link_local_from_mac(mac);
    let ns_len = build_ns(io, mac, &ll, mac, &ll, &ll);
    let out = icmpv6_input(st, nic, io, ns_len);
    if out != 14 + 40 + 32 || rd8(io + 14 + 40) != ICMPV6_NA {
        return false;
    }
    let na = io + 14 + 40;
    let s = get_v6(io + 14 + 8);
    let d = get_v6(io + 14 + 24);
    csum_v6(&s, &d, IP6_PROTO_ICMPV6, na, out - 54) == 0
}

/// V6.1 取证/驱动：发 RS → 收 RA → SLAAC；打印链路本地、全局地址与路由器 MAC。
fn ipv6_probe(st: &mut Stack, nic: usize) {
    let io = st.links[nic].io;
    let ll = link_local_from_mac(mac_bytes(st.links[nic].mac));
    print("NET16 ipv6 ll=");
    print_v6(ll.as_ptr() as u64);
    println(" (rs/ra probe)");
    let len = build_rs(&st.links[nic]);
    let _ = link_tx(&st.links[nic], len);
    let mut tries = 0u64;
    while tries < 40 && !st.links[nic].v6_up {
        if let Some(n) = link_rx(&st.links[nic]) {
            if rd16be(io + 12) == ETH_IPV6 {
                let _ = icmpv6_input(st, nic, io, n);
            }
        }
        sys_sleep(10);
        tries += 1;
    }
    if st.links[nic].v6_up {
        print("NET16 ipv6 slaac OK (g=");
        print_v6(st.links[nic].v6_global.as_ptr() as u64);
        if st.links[nic].gw6_mac.is_some() {
            print(", gw mac learned");
        }
        println(")");
    } else {
        println("NET16 ipv6 slaac timeout (no RA in 400ms)");
    }
}

// ---------------------------------------------------------------------------
// V6.2 — IPv6 传输：ICMPv6 echo / 最小错误 + UDPv6 + TCPv6（复用 v4 状态机）
// ---------------------------------------------------------------------------

/// 解析出的 IPv6 报文（V6.2）。
struct Ipv6Info {
    src: [u8; 16],
    dst: [u8; 16],
    next: u8,
    payload_off: u64,
    payload_len: u64,
}

/// 解析以太帧里的 IPv6 报文（校验版本/负载长度，暂不处理扩展头）。
fn ipv6_parse(eth_va: u64, eth_len: u64) -> Option<Ipv6Info> {
    if eth_len < 14 + 40 || rd16be(eth_va + 12) != ETH_IPV6 {
        return None;
    }
    let ip = eth_va + 14;
    if (rd8(ip) >> 4) != 6 {
        return None;
    }
    let plen = rd16be(ip + 4) as u64;
    if 14 + 40 + plen > eth_len {
        return None;
    }
    Some(Ipv6Info {
        src: get_v6(ip + 8),
        dst: get_v6(ip + 24),
        next: rd8(ip + 6),
        payload_off: ip + 40,
        payload_len: plen,
    })
}

/// 本链路用于 IPv6 的源地址：SLAAC 全局地址（已配置）否则 EUI-64 链路本地。
fn link_v6_primary(st: &Stack, nic: usize) -> [u8; 16] {
    if st.links[nic].v6_up && !v6_is_zero(&st.links[nic].v6_global) {
        st.links[nic].v6_global
    } else {
        link_local_from_mac(mac_bytes(st.links[nic].mac))
    }
}

/// 构造一个 ICMPv6 echo request（目的 `dst`，负载从 `payload_va` 拷入）；返回帧长。
#[allow(clippy::too_many_arguments)]
fn build_echo6(
    io: u64,
    mac: [u8; 6],
    dst_mac: [u8; 6],
    src: &[u8; 16],
    dst: &[u8; 16],
    id: u16,
    seq: u16,
    payload_va: u64,
    plen: u64,
) -> u64 {
    put_mac(io, dst_mac);
    put_mac(io + 6, mac);
    wr16be(io + 12, ETH_IPV6);
    let ip = io + 14;
    let icmp_len = 8 + plen;
    wr32be(ip, 0x6000_0000);
    wr16be(ip + 4, icmp_len as u16);
    wr8(ip + 6, IP6_PROTO_ICMPV6);
    wr8(ip + 7, 64);
    put_v6(ip + 8, src);
    put_v6(ip + 24, dst);
    let ic = ip + 40;
    wr8(ic, ICMPV6_ECHO_REQ);
    wr8(ic + 1, 0);
    wr16be(ic + 2, 0);
    wr16be(ic + 4, id);
    wr16be(ic + 6, seq);
    unsafe {
        core::ptr::copy_nonoverlapping(payload_va as *const u8, (ic + 8) as *mut u8, plen as usize);
    }
    wr16be(ic + 2, csum_v6(src, dst, IP6_PROTO_ICMPV6, ic, icmp_len));
    14 + 40 + icmp_len
}

/// 就地把收到的 echo request 改写成 echo reply（交换 L2/L3 源目 + 重算校验和）；返回帧长。
fn echo6_reply(io: u64, mac: [u8; 6], plen: u64) -> u64 {
    let ip = io + 14;
    let peer_mac = [
        rd8(io + 6),
        rd8(io + 7),
        rd8(io + 8),
        rd8(io + 9),
        rd8(io + 10),
        rd8(io + 11),
    ];
    let src = get_v6(ip + 8);
    let dst = get_v6(ip + 24);
    put_mac(io, peer_mac);
    put_mac(io + 6, mac);
    put_v6(ip + 8, &dst);
    put_v6(ip + 24, &src);
    let ic = ip + 40;
    wr8(ic, ICMPV6_ECHO_REPLY);
    wr8(ic + 1, 0);
    wr16be(ic + 2, 0);
    wr16be(ic + 2, csum_v6(&dst, &src, IP6_PROTO_ICMPV6, ic, plen));
    14 + 40 + plen
}

/// 构造 UDPv6 数据报（IPv6 下 UDP 校验和**必需**，不可置 0）；返回帧长。
#[allow(clippy::too_many_arguments)]
fn build_udp6(
    io: u64,
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src: &[u8; 16],
    dst: &[u8; 16],
    sport: u16,
    dport: u16,
    payload_va: u64,
    plen: u64,
) -> u64 {
    put_mac(io, dst_mac);
    put_mac(io + 6, src_mac);
    wr16be(io + 12, ETH_IPV6);
    let ip = io + 14;
    let udp_len = 8 + plen;
    wr32be(ip, 0x6000_0000);
    wr16be(ip + 4, udp_len as u16);
    wr8(ip + 6, IP_PROTO_UDP);
    wr8(ip + 7, 64);
    put_v6(ip + 8, src);
    put_v6(ip + 24, dst);
    let udp = ip + 40;
    wr16be(udp, sport);
    wr16be(udp + 2, dport);
    wr16be(udp + 4, udp_len as u16);
    wr16be(udp + 6, 0);
    unsafe {
        core::ptr::copy_nonoverlapping(
            payload_va as *const u8,
            (udp + 8) as *mut u8,
            plen as usize,
        );
    }
    wr16be(udp + 6, csum_v6(src, dst, IP_PROTO_UDP, udp, udp_len));
    14 + 40 + udp_len
}

/// UDPv6 校验和（含 IPv6 伪首部）。段内已含校验和时结果为 0（合法）。
fn udp6_csum(src: &[u8; 16], dst: &[u8; 16], seg: u64, len: u64) -> u16 {
    csum_v6(src, dst, IP_PROTO_UDP, seg, len)
}

/// 构造 ICMPv6「目的不可达 / 端口不可达」，body 携带调用包（IPv6 头 + 前 8 字节）；返回帧长。
fn icmpv6_port_unreach(st: &Stack, nic: usize, io: u64, info: &Ipv6Info) -> u64 {
    let our = info.dst;
    let peer = info.src;
    let peer_mac = [
        rd8(io + 6),
        rd8(io + 7),
        rd8(io + 8),
        rd8(io + 9),
        rd8(io + 10),
        rd8(io + 11),
    ];
    let mac = mac_bytes(st.links[nic].mac);
    // 调用包（尽量大到 48 字节：IPv6 头 + 前 8 字节负载）先挪到 scratch，再原地构造回帧。
    let orig = 40 + info.payload_len;
    let ih = orig.min(48);
    let scratch = io + SCRATCH_OFF;
    let mut i = 0u64;
    while i < ih {
        wr8(scratch + i, rd8(io + 14 + i));
        i += 1;
    }
    put_mac(io, peer_mac);
    put_mac(io + 6, mac);
    wr16be(io + 12, ETH_IPV6);
    let ip = io + 14;
    let plen = 8 + ih; // ICMP 头(4) + 未用(4) + 调用包
    wr32be(ip, 0x6000_0000);
    wr16be(ip + 4, plen as u16);
    wr8(ip + 6, IP6_PROTO_ICMPV6);
    wr8(ip + 7, 64);
    put_v6(ip + 8, &our);
    put_v6(ip + 24, &peer);
    let ic = ip + 40;
    wr8(ic, ICMPV6_DEST_UNREACH);
    wr8(ic + 1, ICMPV6_CODE_PORT_UNREACH);
    wr16be(ic + 2, 0);
    wr32be(ic + 4, 0);
    let mut j = 0u64;
    while j < ih {
        wr8(ic + 8 + j, rd8(scratch + j));
        j += 1;
    }
    wr16be(ic + 2, csum_v6(&our, &peer, IP6_PROTO_ICMPV6, ic, plen));
    14 + 40 + plen
}

/// 处理收到的 UDPv6：校验（必需）→ 投递到本机 socket；无人接收则回 ICMPv6 端口不可达。
/// 返回需回发的帧长（0 = 无回帧）。
fn udp6_input(st: &mut Stack, nic: usize, io: u64, info: &Ipv6Info) -> u64 {
    if info.payload_len < 8 {
        return 0;
    }
    let udp = info.payload_off;
    let ulen = rd16be(udp + 4) as u64;
    if ulen < 8 || ulen > info.payload_len {
        return 0;
    }
    if udp6_csum(&info.src, &info.dst, udp, ulen) != 0 {
        return 0;
    }
    let ll = link_local_from_mac(mac_bytes(st.links[nic].mac));
    if !v6_eq(&info.dst, &ll) && !v6_eq(&info.dst, &st.links[nic].v6_global) {
        return 0; // 非本机地址
    }
    let dport = rd16be(udp + 2);
    if deliver(st, nic, dport, udp + 8, ulen - 8) {
        return 0;
    }
    icmpv6_port_unreach(st, nic, io, info)
}

/// V6.2 IPv6 分用：ICMPv6 → 处理器；UDP → 投递/错误；TCP → 复用 v4 状态机。
/// 返回需回发的帧长（ICMPv6/错误帧）。
fn ipv6_input(st: &mut Stack, nic: usize, io: u64, n: u64, now: u64) -> u64 {
    let info = match ipv6_parse(io, n) {
        Some(i) => i,
        None => return 0,
    };
    match info.next {
        IP6_PROTO_ICMPV6 => icmpv6_input(st, nic, io, n),
        IP_PROTO_UDP => udp6_input(st, nic, io, &info),
        IP_PROTO_TCP => {
            if let Some(seg) = tcp_parse6(&info) {
                let len = tcp_input(st, nic, SrcId::V6(info.src), &seg, now);
                tcp_emit_deliver(st, nic, len, now);
            }
            0
        }
        _ => 0,
    }
}

/// V6.2 确定性自证：echo request → 处理器产出 echo reply（类型/id/seq/负载/校验和）。
fn echo6_selftest(st: &mut Stack, nic: usize) -> bool {
    let io = st.links[nic].io;
    let mac = mac_bytes(st.links[nic].mac);
    let ll = link_local_from_mac(mac);
    let payload = b"MORION-V6.2";
    let len = build_echo6(
        io,
        mac,
        mac,
        &ll,
        &ll,
        0x4d4f,
        1,
        payload.as_ptr() as u64,
        payload.len() as u64,
    );
    let out = icmpv6_input(st, nic, io, len);
    if out != len {
        return false;
    }
    let ic = io + 14 + 40;
    if rd8(ic) != ICMPV6_ECHO_REPLY || rd16be(ic + 4) != 0x4d4f || rd16be(ic + 6) != 1 {
        return false;
    }
    let mut i = 0usize;
    while i < payload.len() {
        if rd8(ic + 8 + i as u64) != payload[i] {
            return false;
        }
        i += 1;
    }
    let s = get_v6(io + 14 + 8);
    let d = get_v6(io + 14 + 24);
    csum_v6(&s, &d, IP6_PROTO_ICMPV6, ic, out - 54) == 0
}

/// V6.2 确定性自证：UDPv6 校验和合法 + 投递到 socket；未绑端口 → ICMPv6 端口不可达。
fn udp6_selftest(st: &mut Stack, nic: usize) -> bool {
    let io = st.links[nic].io;
    let mac = mac_bytes(st.links[nic].mac);
    let ll = link_local_from_mac(mac);
    let payload = b"MORION-V6.2-UDP";
    let id = sock_alloc(st, nic);
    if id == 0 {
        return false;
    }
    st.socks[id as usize - 1].port = 0xbee0;
    let len = build_udp6(
        io,
        mac,
        mac,
        &ll,
        &ll,
        0x4d50,
        0xbee0,
        payload.as_ptr() as u64,
        payload.len() as u64,
    );
    let info = match ipv6_parse(io, len) {
        Some(i) => i,
        None => return false,
    };
    if udp6_input(st, nic, io, &info) != 0 {
        return false; // 已投递 → 不应有回帧
    }
    let s = &st.socks[id as usize - 1];
    if s.len != payload.len() as u64 {
        return false;
    }
    let mut i = 0usize;
    while i < payload.len() {
        if s.data[i] != payload[i] {
            return false;
        }
        i += 1;
    }
    st.socks[id as usize - 1].len = 0;
    // 未绑端口 → 生成 ICMPv6 端口不可达。
    let len = build_udp6(
        io,
        mac,
        mac,
        &ll,
        &ll,
        0x4d50,
        0xdead,
        payload.as_ptr() as u64,
        payload.len() as u64,
    );
    let info = match ipv6_parse(io, len) {
        Some(i) => i,
        None => return false,
    };
    let out = udp6_input(st, nic, io, &info);
    if out == 0 || rd8(io + 14 + 40) != ICMPV6_DEST_UNREACH {
        return false;
    }
    if rd8(io + 14 + 40 + 1) != ICMPV6_CODE_PORT_UNREACH {
        return false;
    }
    let s = get_v6(io + 14 + 8);
    let d = get_v6(io + 14 + 24);
    let ok = csum_v6(&s, &d, IP6_PROTO_ICMPV6, io + 14 + 40, out - 54) == 0;
    let _ = sock_close(st, id);
    ok
}

/// V6.2 链路取证：向路由器（RA 源）发 echo request，收 echo reply（best-effort）。
fn icmpv6_probe(st: &mut Stack, nic: usize) {
    if !st.links[nic].v6_up {
        println("NET17 ipv6 echo skip (no v6)");
        return;
    }
    let io = st.links[nic].io;
    let mac = mac_bytes(st.links[nic].mac);
    let ll = link_local_from_mac(mac);
    let gw = st.links[nic].v6_gw;
    let gw_mac = match st.links[nic].gw6_mac {
        Some(m) => m,
        None => {
            println("NET17 ipv6 echo skip (no gw mac)");
            return;
        }
    };
    let payload = b"morion-ping6";
    let len = build_echo6(
        io,
        mac,
        gw_mac,
        &ll,
        &gw,
        0x4d4f,
        1,
        payload.as_ptr() as u64,
        payload.len() as u64,
    );
    let _ = link_tx(&st.links[nic], len);
    let mut tries = 0u64;
    while tries < 40 {
        if let Some(n) = link_rx(&st.links[nic]) {
            if rd16be(io + 12) == ETH_IPV6 {
                match ipv6_parse(io, n) {
                    Some(info)
                        if info.next == IP6_PROTO_ICMPV6
                            && rd8(info.payload_off) == ICMPV6_ECHO_REPLY
                            && rd16be(info.payload_off + 4) == 0x4d4f =>
                    {
                        println("NET17 ipv6 echo OK (router replied)");
                        return;
                    }
                    _ => {
                        let _ = icmpv6_input(st, nic, io, n);
                    }
                }
            }
        }
        sys_sleep(10);
        tries += 1;
    }
    println("NET17 ipv6 echo timeout (no reply)");
}

/// 学某网卡的网关 MAC（发 ARP 请求 + 有界收包）。取不到则保持 `None`。
fn arp_learn_gw(st: &mut Stack, nic: usize) {
    let mut tries = 0u64;
    while st.links[nic].gw_mac.is_none() && tries < 40 {
        let len = build_arp_request(&st.links[nic], GW_IP);
        let _ = link_tx(&st.links[nic], len);
        for _ in 0..5 {
            if let Some(n) = link_rx(&st.links[nic]) {
                handle_frame(st, nic, n, 0);
            }
            sys_sleep(10);
        }
        tries += 1;
    }
}

/// 处理某网卡收到的一帧（帧在链路共享页，长 `n`）。
fn handle_frame(st: &mut Stack, nic: usize, n: u64, now: u64) {
    if n < 14 {
        return;
    }
    let f = st.links[nic].io;
    let et = rd16be(f + 12);
    if et == ETH_ARP {
        if n >= 42 && rd16be(f + 20) == 0x0002 {
            let sip = [rd8(f + 28), rd8(f + 29), rd8(f + 30), rd8(f + 31)];
            if sip == GW_IP {
                st.links[nic].gw_mac = Some([
                    rd8(f + 22),
                    rd8(f + 23),
                    rd8(f + 24),
                    rd8(f + 25),
                    rd8(f + 26),
                    rd8(f + 27),
                ]);
            }
        }
        return;
    }
    if et == ETH_IPV6 {
        // V6.2: IPv6 分用（ICMPv6 NDP/echo、UDPv6、TCPv6）；若需回帧，帧已就地构造好，直接发回。
        let out = ipv6_input(st, nic, f, n, now);
        if out > 0 {
            let _ = link_tx(&st.links[nic], out);
        }
        return;
    }
    if et != ETH_IPV4 || n < 34 {
        return;
    }
    let ip = f + 14;
    if (rd8(ip) >> 4) != 4 {
        return;
    }
    let ihl = ((rd8(ip) & 0x0f) as u64) * 4;
    if ihl < 20 || n < 14 + ihl {
        return;
    }
    let proto = rd8(ip + 9);
    let dst_ip = [rd8(ip + 16), rd8(ip + 17), rd8(ip + 18), rd8(ip + 19)];
    if proto == IP_PROTO_TCP {
        if let Some(info) = ipv4_parse(f, n) {
            if let Some(seg) = tcp_parse(&info) {
                let len = tcp_input(st, nic, SrcId::V4(info.src), &seg, now);
                tcp_emit_deliver(st, nic, len, now);
            }
        }
        return;
    }
    if proto == IP_PROTO_ICMP {
        let ic = ip + ihl;
        if n > 14 + ihl && rd8(ic) == ICMP_UNREACH {
            print("netstack: nic");
            if nic != 0 {
                print_u64(nic as u64);
            }
            println(" rx (icmp unreachable) OK");
        }
        return;
    }
    if proto == IP_PROTO_UDP && dst_ip == OUR_IP {
        let udp = ip + ihl;
        let dport = rd16be(udp + 2);
        let ulen = rd16be(udp + 4) as u64;
        if ulen >= 8 && n >= 14 + ihl + ulen {
            deliver(st, nic, dport, udp + 8, ulen - 8);
        }
    }
}

/// 把负载投递给**绑在该网卡**上、绑定 `dport` 的 socket；投递成功返回 `true`。
fn deliver(st: &mut Stack, nic: usize, dport: u16, src_va: u64, len: u64) -> bool {
    for s in st.socks.iter_mut() {
        if s.used && s.nic == nic && s.port == dport && len <= PAY_MAX as u64 {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src_va as *const u8,
                    s.data.as_mut_ptr(),
                    len as usize,
                );
            }
            s.len = len;
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// N7 — TCP（段构造/解析/校验和 + 连接状态机 + 重传）
// ---------------------------------------------------------------------------

const IP_PROTO_TCP: u8 = 6;
const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
const TCP_RST: u8 = 0x04;
const TCP_PSH: u8 = 0x08;
const TCP_ACK: u8 = 0x10;
/// 通告窗口（任意值）。
const TCP_WINDOW: u16 = 0x7210;
/// 初始重传超时（毫秒）与最大重传次数（超过判连接失败）。
const TCP_RTO_MS: u64 = 300;
const TCP_MAX_RETX: u32 = 5;
/// 单段最大载荷（亦为重传缓冲大小）。
const TCP_MSS: usize = 1400;

fn rd32be(a: u64) -> u32 {
    ((rd8(a) as u32) << 24)
        | ((rd8(a + 1) as u32) << 16)
        | ((rd8(a + 2) as u32) << 8)
        | rd8(a + 3) as u32
}
fn wr32be(a: u64, v: u32) {
    wr16be(a, (v >> 16) as u16);
    wr16be(a + 2, v as u16);
}

/// 在校验和中累加 `[base, base+len)` 的 16 位字（不折叠、不取反）。
fn csum_acc(base: u64, len: u64, mut sum: u32) -> u32 {
    let mut i = 0u64;
    while i + 1 < len {
        sum += rd16be(base + i) as u32;
        i += 2;
    }
    if i < len {
        sum += (rd8(base + i) as u32) << 8;
    }
    sum
}
fn csum_fold(mut s: u32) -> u16 {
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    s as u16
}

/// TCP 校验和（12 字节伪首部 + 段）。段内已含校验和字段时，结果应为 0（合法）。
fn tcp_csum(src: [u8; 4], dst: [u8; 4], seg: u64, len: u64) -> u16 {
    let mut ph = [0u8; 12];
    ph[0] = src[0];
    ph[1] = src[1];
    ph[2] = src[2];
    ph[3] = src[3];
    ph[4] = dst[0];
    ph[5] = dst[1];
    ph[6] = dst[2];
    ph[7] = dst[3];
    ph[8] = 0;
    ph[9] = IP_PROTO_TCP;
    ph[10] = (len >> 8) as u8;
    ph[11] = len as u8;
    let s = csum_acc(ph.as_ptr() as u64, 12, 0);
    let s = csum_acc(seg, len, s);
    !csum_fold(s)
}

/// TCP 段的 L3 来源（v4/v6 双栈共用同一状态机时的地址身份）。
#[derive(Clone, Copy)]
enum SrcId {
    V4([u8; 4]),
    V6([u8; 16]),
}

/// 解析出的 IPv4 报文。
struct Ipv4Info {
    src: [u8; 4],
    dst: [u8; 4],
    proto: u8,
    payload_off: u64,
    payload_len: u64,
}

/// 解析以太帧里的 IPv4 报文（校验版本/IHL/总长/头校验和，拒分片）。
fn ipv4_parse(eth_va: u64, eth_len: u64) -> Option<Ipv4Info> {
    if eth_len < 14 + 20 || rd16be(eth_va + 12) != ETH_IPV4 {
        return None;
    }
    let ip = eth_va + 14;
    let ip_len = eth_len - 14;
    let ver_ihl = rd8(ip);
    if ver_ihl >> 4 != 4 {
        return None;
    }
    let ihl = (ver_ihl & 0x0f) as u64 * 4;
    if ihl < 20 || ihl > ip_len {
        return None;
    }
    let total = rd16be(ip + 2) as u64;
    if total < ihl || total > ip_len {
        return None;
    }
    let frag = rd16be(ip + 6);
    if frag & 0x2000 != 0 || frag & 0x1fff != 0 {
        return None;
    }
    if csum(ip, ihl) != 0 {
        return None;
    }
    Some(Ipv4Info {
        src: [rd8(ip + 12), rd8(ip + 13), rd8(ip + 14), rd8(ip + 15)],
        dst: [rd8(ip + 16), rd8(ip + 17), rd8(ip + 18), rd8(ip + 19)],
        proto: rd8(ip + 9),
        payload_off: ip + ihl,
        payload_len: total - ihl,
    })
}

/// 解析出的 TCP 段。
struct TcpSeg {
    sport: u16,
    dport: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    payload_off: u64,
    payload_len: u64,
}

/// 解析 IPv4 报文里的 TCP 段（校验数据偏移与伪首部校验和）。
fn tcp_parse(info: &Ipv4Info) -> Option<TcpSeg> {
    if info.proto != IP_PROTO_TCP || info.payload_len < 20 {
        return None;
    }
    let tcp = info.payload_off;
    let doff = (rd8(tcp + 12) >> 4) as u64 * 4;
    if doff < 20 || doff > info.payload_len {
        return None;
    }
    if tcp_csum(info.src, info.dst, tcp, info.payload_len) != 0 {
        return None;
    }
    Some(TcpSeg {
        sport: rd16be(tcp),
        dport: rd16be(tcp + 2),
        seq: rd32be(tcp + 4),
        ack: rd32be(tcp + 8),
        flags: rd8(tcp + 13),
        window: rd16be(tcp + 14),
        payload_off: tcp + doff,
        payload_len: info.payload_len - doff,
    })
}

/// 在一个共享页里拼一个 TCP 段（以太 + IPv4 + TCP，无选项），返回帧长。
#[allow(clippy::too_many_arguments)]
fn tcp_build(
    io: u64,
    src_mac: u64,
    dst_mac: [u8; 6],
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    sport: u16,
    dport: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    payload: &[u8],
) -> u64 {
    let f = io;
    put_mac(f, dst_mac);
    put_mac(f + 6, mac_bytes(src_mac));
    wr16be(f + 12, ETH_IPV4);
    let ip = f + 14;
    let seg_len = 20 + payload.len() as u64;
    wr8(ip, 0x45);
    wr8(ip + 1, 0);
    wr16be(ip + 2, (20 + seg_len) as u16);
    wr16be(ip + 4, 0);
    wr16be(ip + 6, 0x4000); // DF
    wr8(ip + 8, 64);
    wr8(ip + 9, IP_PROTO_TCP);
    wr16be(ip + 10, 0);
    for (i, b) in src_ip.iter().enumerate() {
        wr8(ip + 12 + i as u64, *b);
        wr8(ip + 16 + i as u64, dst_ip[i]);
    }
    wr16be(ip + 10, csum(ip, 20));
    let tcp = ip + 20;
    wr16be(tcp, sport);
    wr16be(tcp + 2, dport);
    wr32be(tcp + 4, seq);
    wr32be(tcp + 8, ack);
    wr8(tcp + 12, 5 << 4); // data offset = 5
    wr8(tcp + 13, flags);
    wr16be(tcp + 14, window);
    wr16be(tcp + 16, 0); // checksum（回填）
    wr16be(tcp + 18, 0); // urgent pointer
    unsafe {
        core::ptr::copy_nonoverlapping(payload.as_ptr(), (tcp + 20) as *mut u8, payload.len());
    }
    wr16be(tcp + 16, tcp_csum(src_ip, dst_ip, tcp, seg_len));
    14 + 20 + seg_len
}

/// 拼一个 v6 的 TCP 段（以太 + IPv6 + TCP，无扩展头），返回帧长。
#[allow(clippy::too_many_arguments)]
fn tcp_build6(
    io: u64,
    src_mac: u64,
    dst_mac: [u8; 6],
    src_ip: &[u8; 16],
    dst_ip: &[u8; 16],
    sport: u16,
    dport: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    payload: &[u8],
) -> u64 {
    let f = io;
    put_mac(f, dst_mac);
    put_mac(f + 6, mac_bytes(src_mac));
    wr16be(f + 12, ETH_IPV6);
    let ip = f + 14;
    let seg_len = 20 + payload.len() as u64;
    wr32be(ip, 0x6000_0000);
    wr16be(ip + 4, seg_len as u16);
    wr8(ip + 6, IP_PROTO_TCP);
    wr8(ip + 7, 64);
    put_v6(ip + 8, src_ip);
    put_v6(ip + 24, dst_ip);
    let tcp = ip + 40;
    wr16be(tcp, sport);
    wr16be(tcp + 2, dport);
    wr32be(tcp + 4, seq);
    wr32be(tcp + 8, ack);
    wr8(tcp + 12, 5 << 4); // data offset = 5
    wr8(tcp + 13, flags);
    wr16be(tcp + 14, window);
    wr16be(tcp + 16, 0); // checksum（回填）
    wr16be(tcp + 18, 0);
    unsafe {
        core::ptr::copy_nonoverlapping(payload.as_ptr(), (tcp + 20) as *mut u8, payload.len());
    }
    wr16be(
        tcp + 16,
        csum_v6(src_ip, dst_ip, IP_PROTO_TCP, tcp, seg_len),
    );
    14 + 40 + seg_len
}

/// 解析 IPv6 报文里的 TCP 段（校验数据偏移与 v6 伪首部校验和）。
fn tcp_parse6(info: &Ipv6Info) -> Option<TcpSeg> {
    if info.next != IP_PROTO_TCP || info.payload_len < 20 {
        return None;
    }
    let tcp = info.payload_off;
    let doff = (rd8(tcp + 12) >> 4) as u64 * 4;
    if doff < 20 || doff > info.payload_len {
        return None;
    }
    if csum_v6(&info.src, &info.dst, IP_PROTO_TCP, tcp, info.payload_len) != 0 {
        return None;
    }
    Some(TcpSeg {
        sport: rd16be(tcp),
        dport: rd16be(tcp + 2),
        seq: rd32be(tcp + 4),
        ack: rd32be(tcp + 8),
        flags: rd8(tcp + 13),
        window: rd16be(tcp + 14),
        payload_off: tcp + doff,
        payload_len: info.payload_len - doff,
    })
}

/// 从 `io` 页解析一个 v6 TCP 段（自带以太/IPv6 头）。
fn parse_seg_io6(io: u64, fl: u64) -> Option<TcpSeg> {
    let info = ipv6_parse(io, fl)?;
    tcp_parse6(&info)
}

/// TCP 连接状态（含主动/被动打开子集）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynRcvd,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    TimeWait,
}

/// 连接在协议栈里的角色。
const TCP_ROLE_CLIENT: u8 = 0;
const TCP_ROLE_LISTENER: u8 = 1;
const TCP_ROLE_SERVER: u8 = 2;

/// 一条 TCP 连接（客户端/监听者/被接受的连接共用）。
#[derive(Clone, Copy)]
struct TcpConn {
    used: bool,
    /// 拥有该连接的域 (发起 `TSOCKET`/`TLISTEN` 的域); 其它域不得操作它。
    owner: u64,
    /// [`TCP_ROLE_CLIENT`] / [`TCP_ROLE_LISTENER`] / [`TCP_ROLE_SERVER`]。
    role: u8,
    state: TcpState,
    nic: usize,
    mac: u64,
    dst_mac: [u8; 6],
    local_port: u16,
    remote_ip: [u8; 4],
    remote_port: u16,
    /// L3 协议族：4 或 6（v4/v6 共用同一状态机）。
    family: u8,
    /// v6 对端地址（`family == 6` 时有效）。
    remote6: [u8; 16],
    /// v6 本端地址（`family == 6` 时有效）。
    local6: [u8; 16],
    /// 最老的未确认序号。
    snd_una: u32,
    /// 下一个要发的序号。
    snd_nxt: u32,
    /// 期望对端下一个序号。
    rcv_nxt: u32,
    retx: u32,
    rto_ms: u64,
    last_tx_ms: u64,
    /// 未确认数据长度（重传用）。
    tx_len: usize,
    tx: [u8; TCP_MSS],
    /// 收妥的对端数据。
    rx_len: u64,
    rx: [u8; PAY_MAX],
    /// 服务端连接是否已被 `accept` 取走。
    accepted: bool,
}

impl TcpConn {
    const fn new() -> TcpConn {
        TcpConn {
            used: false,
            owner: 0,
            role: TCP_ROLE_CLIENT,
            state: TcpState::Closed,
            nic: 0,
            mac: 0,
            dst_mac: [0; 6],
            local_port: 0,
            remote_ip: [0; 4],
            remote_port: 0,
            family: 4,
            remote6: [0; 16],
            local6: [0; 16],
            snd_una: 0,
            snd_nxt: 0,
            rcv_nxt: 0,
            retx: 0,
            rto_ms: TCP_RTO_MS,
            last_tx_ms: 0,
            tx_len: 0,
            tx: [0; TCP_MSS],
            rx_len: 0,
            rx: [0; PAY_MAX],
            accepted: false,
        }
    }
}

/// 用连接的寻址信息构造一个段（`seq`/`ack` 由调用方给出）。v4/v6 由 `conn.family` 决定。
fn tcp_seg(conn: &TcpConn, io: u64, seq: u32, ack: u32, flags: u8, payload: &[u8]) -> u64 {
    if conn.family == 6 {
        tcp_build6(
            io,
            conn.mac,
            conn.dst_mac,
            &conn.local6,
            &conn.remote6,
            conn.local_port,
            conn.remote_port,
            seq,
            ack,
            flags,
            TCP_WINDOW,
            payload,
        )
    } else {
        tcp_build(
            io,
            conn.mac,
            conn.dst_mac,
            OUR_IP,
            conn.remote_ip,
            conn.local_port,
            conn.remote_port,
            seq,
            ack,
            flags,
            TCP_WINDOW,
            payload,
        )
    }
}

/// 主动连接：发 SYN，进入 SynSent。返回 SYN 帧长。
fn tcp_connect(conn: &mut TcpConn, io: u64, isn: u32, now: u64) -> u64 {
    conn.snd_una = isn;
    conn.snd_nxt = isn;
    conn.rcv_nxt = 0;
    conn.retx = 0;
    conn.rto_ms = TCP_RTO_MS;
    conn.last_tx_ms = now;
    conn.tx_len = 0;
    conn.state = TcpState::SynSent;
    let len = tcp_seg(conn, io, isn, 0, TCP_SYN, &[]);
    conn.snd_nxt = isn.wrapping_add(1);
    len
}

/// 发送数据（须 Established 且无未确认数据）。返回帧长（0 = 当前不可发）。
fn tcp_send(conn: &mut TcpConn, io: u64, payload: &[u8], now: u64) -> u64 {
    if conn.state != TcpState::Established || conn.tx_len > 0 {
        return 0;
    }
    let n = payload.len().min(TCP_MSS);
    conn.tx[..n].copy_from_slice(&payload[..n]);
    conn.tx_len = n;
    conn.retx = 0;
    conn.rto_ms = TCP_RTO_MS;
    conn.last_tx_ms = now;
    let len = tcp_seg(
        conn,
        io,
        conn.snd_nxt,
        conn.rcv_nxt,
        TCP_PSH | TCP_ACK,
        &conn.tx[..n],
    );
    conn.snd_nxt = conn.snd_nxt.wrapping_add(n as u32);
    len
}

/// 主动关闭：发 FIN。返回帧长（0 = 当前不可关）。
fn tcp_close(conn: &mut TcpConn, io: u64, now: u64) -> u64 {
    match conn.state {
        TcpState::Established | TcpState::CloseWait => conn.state = TcpState::FinWait1,
        _ => return 0,
    }
    conn.retx = 0;
    conn.rto_ms = TCP_RTO_MS;
    conn.last_tx_ms = now;
    conn.tx_len = 0;
    let len = tcp_seg(conn, io, conn.snd_nxt, conn.rcv_nxt, TCP_FIN | TCP_ACK, &[]);
    conn.snd_nxt = conn.snd_nxt.wrapping_add(1);
    len
}

/// 处理对端一个 TCP 段；若有回应段，构造在 `io` 并返回帧长（0 = 无回应）。
fn tcp_handle(conn: &mut TcpConn, io: u64, seg: &TcpSeg, now: u64) -> u64 {
    match conn.state {
        TcpState::SynSent => {
            if seg.flags & TCP_RST != 0 {
                conn.state = TcpState::Closed;
                return 0;
            }
            if seg.flags & (TCP_SYN | TCP_ACK) == (TCP_SYN | TCP_ACK) && seg.ack == conn.snd_nxt {
                conn.rcv_nxt = seg.seq.wrapping_add(1);
                conn.snd_una = seg.ack;
                conn.retx = 0;
                conn.rto_ms = TCP_RTO_MS;
                conn.last_tx_ms = now;
                conn.state = TcpState::Established;
                return tcp_seg(conn, io, conn.snd_nxt, conn.rcv_nxt, TCP_ACK, &[]);
            }
            0
        }
        TcpState::SynRcvd => {
            if seg.flags & TCP_RST != 0 {
                conn.state = TcpState::Closed;
                return 0;
            }
            let mut need_ack = false;
            if seg.flags & TCP_ACK != 0 && seg.ack == conn.snd_nxt && conn.snd_una != conn.snd_nxt {
                conn.snd_una = conn.snd_nxt;
                conn.state = TcpState::Established;
                conn.retx = 0;
                conn.rto_ms = TCP_RTO_MS;
            }
            if seg.payload_len > 0 && seg.seq == conn.rcv_nxt {
                let n = seg.payload_len.min(PAY_MAX as u64);
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        seg.payload_off as *const u8,
                        conn.rx.as_mut_ptr(),
                        n as usize,
                    );
                }
                conn.rx_len = n;
                conn.rcv_nxt = conn.rcv_nxt.wrapping_add(seg.payload_len as u32);
                need_ack = true;
            }
            if need_ack {
                return tcp_seg(conn, io, conn.snd_nxt, conn.rcv_nxt, TCP_ACK, &[]);
            }
            0
        }
        TcpState::Established => {
            if seg.flags & TCP_RST != 0 {
                conn.state = TcpState::Closed;
                return 0;
            }
            let mut need_ack = false;
            if seg.payload_len > 0 && seg.seq == conn.rcv_nxt {
                let n = seg.payload_len.min(PAY_MAX as u64);
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        seg.payload_off as *const u8,
                        conn.rx.as_mut_ptr(),
                        n as usize,
                    );
                }
                conn.rx_len = n;
                conn.rcv_nxt = conn.rcv_nxt.wrapping_add(seg.payload_len as u32);
                need_ack = true;
            }
            if seg.flags & TCP_ACK != 0 && seg.ack == conn.snd_nxt && conn.snd_una != conn.snd_nxt {
                conn.snd_una = conn.snd_nxt;
                conn.tx_len = 0;
                conn.retx = 0;
                conn.rto_ms = TCP_RTO_MS;
            }
            if seg.flags & TCP_FIN != 0 {
                conn.rcv_nxt = conn.rcv_nxt.wrapping_add(1);
                conn.state = TcpState::CloseWait;
                need_ack = true;
            }
            if need_ack {
                return tcp_seg(conn, io, conn.snd_nxt, conn.rcv_nxt, TCP_ACK, &[]);
            }
            0
        }
        TcpState::CloseWait => {
            if seg.flags & TCP_ACK != 0 && seg.ack == conn.snd_nxt {
                conn.snd_una = conn.snd_nxt;
            }
            0
        }
        TcpState::FinWait1 => {
            if seg.flags & TCP_ACK != 0 && seg.ack == conn.snd_nxt {
                conn.snd_una = conn.snd_nxt;
                conn.state = TcpState::FinWait2;
            }
            if seg.flags & TCP_FIN != 0 {
                conn.rcv_nxt = conn.rcv_nxt.wrapping_add(1);
                conn.state = TcpState::TimeWait;
                return tcp_seg(conn, io, conn.snd_nxt, conn.rcv_nxt, TCP_ACK, &[]);
            }
            0
        }
        TcpState::FinWait2 => {
            if seg.flags & TCP_FIN != 0 {
                conn.rcv_nxt = conn.rcv_nxt.wrapping_add(1);
                conn.state = TcpState::TimeWait;
                return tcp_seg(conn, io, conn.snd_nxt, conn.rcv_nxt, TCP_ACK, &[]);
            }
            0
        }
        _ => 0,
    }
}

/// 定时器：有未确认内容且到 RTO 则重传（指数退避）；超次数判失败。返回重传帧长（0 = 无）。
fn tcp_tick(conn: &mut TcpConn, io: u64, now: u64) -> u64 {
    if conn.state == TcpState::Closed {
        return 0;
    }
    let pending = conn.state == TcpState::SynSent || conn.tx_len > 0;
    if !pending || now.wrapping_sub(conn.last_tx_ms) < conn.rto_ms {
        return 0;
    }
    conn.retx += 1;
    if conn.retx > TCP_MAX_RETX {
        conn.state = TcpState::Closed;
        return 0;
    }
    conn.last_tx_ms = now;
    conn.rto_ms *= 2;
    if conn.state == TcpState::SynSent {
        return tcp_seg(conn, io, conn.snd_una, 0, TCP_SYN, &[]);
    }
    tcp_seg(
        conn,
        io,
        conn.snd_una,
        conn.rcv_nxt,
        TCP_PSH | TCP_ACK,
        &conn.tx[..conn.tx_len],
    )
}

/// 从 `io` 页解析一个 TCP 段（自带以太/IPv4 头）。
fn parse_seg_io(io: u64, fl: u64) -> Option<TcpSeg> {
    let info = ipv4_parse(io, fl)?;
    tcp_parse(&info)
}

/// N7 确定性自证：驱动客户端状态机走完整生命周期（连接→收发→重传→对端 FIN→关闭），
/// 并校验合成段的字段与校验和。不依赖真实对端（用合成段），故可稳定回归。
fn tcp_selftest(io: u64) -> bool {
    let peer_mac = [0x02u8, 0, 0, 0, 0, 2];
    let isn = 0x4d4f_0004u32;
    let peer_isn = 0x1234_5678u32;
    let mut c = TcpConn::new();
    c.used = true;
    c.mac = 0x5634_1200_5452;
    c.dst_mac = peer_mac;
    c.local_port = 0x4d50;
    c.remote_ip = GW_IP;
    c.remote_port = 12345;

    // 1) 主动连接 → SYN（seq = isn）。
    let fl = tcp_connect(&mut c, io, isn, 0);
    let s = match parse_seg_io(io, fl) {
        Some(x) => x,
        None => return false,
    };
    if s.flags != TCP_SYN || s.seq != isn || s.dport != 12345 {
        return false;
    }

    // 2) 合成对端 SYN-ACK → ESTABLISHED 并回 ACK（seq=isn+1, ack=peer_isn+1）。
    let fl = tcp_build(
        io,
        0,
        mac_bytes(c.mac),
        GW_IP,
        OUR_IP,
        12345,
        c.local_port,
        peer_isn,
        isn.wrapping_add(1),
        TCP_SYN | TCP_ACK,
        TCP_WINDOW,
        &[],
    );
    let synack = match parse_seg_io(io, fl) {
        Some(x) => x,
        None => return false,
    };
    let r = tcp_handle(&mut c, io, &synack, 0);
    if c.state != TcpState::Established || r == 0 {
        return false;
    }
    let a = match parse_seg_io(io, r) {
        Some(x) => x,
        None => return false,
    };
    if a.flags & TCP_ACK == 0 || a.seq != isn.wrapping_add(1) || a.ack != peer_isn.wrapping_add(1) {
        return false;
    }

    // 3) 对端来数据 → 回 ACK，且收妥数据逐字节一致。
    let data = b"MORION-N7";
    let fl = tcp_build(
        io,
        0,
        mac_bytes(c.mac),
        GW_IP,
        OUR_IP,
        12345,
        c.local_port,
        peer_isn.wrapping_add(1),
        isn.wrapping_add(1),
        TCP_PSH | TCP_ACK,
        TCP_WINDOW,
        data,
    );
    let dseg = match parse_seg_io(io, fl) {
        Some(x) => x,
        None => return false,
    };
    let r = tcp_handle(&mut c, io, &dseg, 0);
    if r == 0 || c.rx_len != data.len() as u64 {
        return false;
    }
    let mut i = 0usize;
    while i < data.len() {
        if c.rx[i] != data[i] {
            return false;
        }
        i += 1;
    }

    // 4) 主动发数据 → PSH|ACK，载荷逐字节一致。
    let out = b"MORION-N7-DATA";
    let fl = tcp_send(&mut c, io, out, 0);
    let oseg = match parse_seg_io(io, fl) {
        Some(x) => x,
        None => return false,
    };
    if oseg.flags != (TCP_PSH | TCP_ACK) || oseg.payload_len != out.len() as u64 {
        return false;
    }
    let mut j = 0usize;
    while j < out.len() {
        if rd8(oseg.payload_off + j as u64) != out[j] {
            return false;
        }
        j += 1;
    }

    // 5) 未确认 → 到 RTO 重传同一段（载荷不变）。
    let fl2 = tcp_tick(&mut c, io, TCP_RTO_MS);
    let rseg = match parse_seg_io(io, fl2) {
        Some(x) => x,
        None => return false,
    };
    if rseg.flags != (TCP_PSH | TCP_ACK) || rseg.payload_len != out.len() as u64 {
        return false;
    }

    // 6) 对端 ACK 收妥 → 未确认清空。
    let fl = tcp_build(
        io,
        0,
        mac_bytes(c.mac),
        GW_IP,
        OUR_IP,
        12345,
        c.local_port,
        c.rcv_nxt,
        c.snd_nxt,
        TCP_ACK,
        TCP_WINDOW,
        &[],
    );
    let aseg = match parse_seg_io(io, fl) {
        Some(x) => x,
        None => return false,
    };
    let _ = tcp_handle(&mut c, io, &aseg, 0);
    if c.tx_len != 0 {
        return false;
    }

    // 7) 对端 FIN → 回 ACK 并进入 CloseWait。
    let fl = tcp_build(
        io,
        0,
        mac_bytes(c.mac),
        GW_IP,
        OUR_IP,
        12345,
        c.local_port,
        c.rcv_nxt,
        c.snd_nxt,
        TCP_FIN | TCP_ACK,
        TCP_WINDOW,
        &[],
    );
    let fseg = match parse_seg_io(io, fl) {
        Some(x) => x,
        None => return false,
    };
    let r = tcp_handle(&mut c, io, &fseg, 0);
    if c.state != TcpState::CloseWait || r == 0 {
        return false;
    }

    // 8) 主动关闭 → FIN|ACK，进入 FinWait1。
    let r = tcp_close(&mut c, io, 0);
    let cseg = match parse_seg_io(io, r) {
        Some(x) => x,
        None => return false,
    };
    if cseg.flags != (TCP_FIN | TCP_ACK) || c.state != TcpState::FinWait1 {
        return false;
    }

    // 9) 校验和拦截：篡改一个载荷字节 → 解析失败。
    let fl = tcp_build(
        io,
        c.mac,
        peer_mac,
        OUR_IP,
        GW_IP,
        c.local_port,
        12345,
        1000,
        2000,
        TCP_PSH | TCP_ACK,
        TCP_WINDOW,
        b"ABCDEF",
    );
    let tampered = match parse_seg_io(io, fl) {
        Some(x) => x,
        None => return false,
    };
    let p = tampered.payload_off;
    wr8(p, rd8(p) ^ 0xff);
    parse_seg_io(io, fl).is_none()
}

/// N18 确定性自证：用 v6 段驱动**同一套** TCP 状态机（连接 → 收发 → 重传 → 关闭），
/// 校验 v6 伪首部校验和与寻址。不依赖真实对端。
fn tcp6_selftest(io: u64, our: [u8; 16], peer: [u8; 16]) -> bool {
    let peer_mac = [0x02u8, 0, 0, 0, 0, 2];
    let mac = 0x5634_1200_5452u64;
    let isn = 0x4d4f_0600u32;
    let peer_isn = 0x1234_5606u32;
    let mut c = TcpConn::new();
    c.used = true;
    c.family = 6;
    c.mac = mac;
    c.dst_mac = peer_mac;
    c.local6 = our;
    c.remote6 = peer;
    c.local_port = 0x4d60;
    c.remote_port = 12346;

    // 1) 主动连接 → SYN。
    let fl = tcp_connect(&mut c, io, isn, 0);
    let s = match parse_seg_io6(io, fl) {
        Some(x) => x,
        None => return false,
    };
    if s.flags != TCP_SYN || s.seq != isn || s.dport != 12346 {
        return false;
    }

    // 2) 合成对端 SYN-ACK → ESTABLISHED 并回 ACK。
    let fl = tcp_build6(
        io,
        mac,
        peer_mac,
        &peer,
        &our,
        12346,
        c.local_port,
        peer_isn,
        isn.wrapping_add(1),
        TCP_SYN | TCP_ACK,
        TCP_WINDOW,
        &[],
    );
    let synack = match parse_seg_io6(io, fl) {
        Some(x) => x,
        None => return false,
    };
    let r = tcp_handle(&mut c, io, &synack, 0);
    if c.state != TcpState::Established || r == 0 {
        return false;
    }
    let a = match parse_seg_io6(io, r) {
        Some(x) => x,
        None => return false,
    };
    if a.flags & TCP_ACK == 0 || a.ack != peer_isn.wrapping_add(1) {
        return false;
    }

    // 3) 对端来数据 → 回 ACK，数据逐字节一致。
    let data = b"MORION-N18";
    let fl = tcp_build6(
        io,
        mac,
        peer_mac,
        &peer,
        &our,
        12346,
        c.local_port,
        peer_isn.wrapping_add(1),
        isn.wrapping_add(1),
        TCP_PSH | TCP_ACK,
        TCP_WINDOW,
        data,
    );
    let dseg = match parse_seg_io6(io, fl) {
        Some(x) => x,
        None => return false,
    };
    if tcp_handle(&mut c, io, &dseg, 0) == 0 || c.rx_len != data.len() as u64 {
        return false;
    }
    let mut i = 0usize;
    while i < data.len() {
        if c.rx[i] != data[i] {
            return false;
        }
        i += 1;
    }

    // 4) 主动发数据 → PSH|ACK，载荷一致。
    let out = b"MORION-N18-OUT";
    let fl = tcp_send(&mut c, io, out, 0);
    let oseg = match parse_seg_io6(io, fl) {
        Some(x) => x,
        None => return false,
    };
    if oseg.flags != (TCP_PSH | TCP_ACK) || oseg.payload_len != out.len() as u64 {
        return false;
    }
    let mut j = 0usize;
    while j < out.len() {
        if rd8(oseg.payload_off + j as u64) != out[j] {
            return false;
        }
        j += 1;
    }

    // 5) 未确认 → 到 RTO 重传同一段。
    let fl2 = tcp_tick(&mut c, io, TCP_RTO_MS);
    let rseg = match parse_seg_io6(io, fl2) {
        Some(x) => x,
        None => return false,
    };
    if rseg.flags != (TCP_PSH | TCP_ACK) || rseg.payload_len != out.len() as u64 {
        return false;
    }

    // 6) 对端 FIN → 回 ACK 并进入 CloseWait。
    let fl = tcp_build6(
        io,
        mac,
        peer_mac,
        &peer,
        &our,
        12346,
        c.local_port,
        c.rcv_nxt,
        c.snd_nxt,
        TCP_FIN | TCP_ACK,
        TCP_WINDOW,
        &[],
    );
    let fseg = match parse_seg_io6(io, fl) {
        Some(x) => x,
        None => return false,
    };
    if tcp_handle(&mut c, io, &fseg, 0) == 0 || c.state != TcpState::CloseWait {
        return false;
    }

    // 7) v6 伪首部校验和拦截：篡改一个载荷字节 → 解析失败。
    let fl = tcp_build6(
        io,
        mac,
        peer_mac,
        &our,
        &peer,
        c.local_port,
        12346,
        1000,
        2000,
        TCP_PSH | TCP_ACK,
        TCP_WINDOW,
        b"ABCDEF",
    );
    let tampered = match parse_seg_io6(io, fl) {
        Some(x) => x,
        None => return false,
    };
    let p = tampered.payload_off;
    wr8(p, rd8(p) ^ 0xff);
    parse_seg_io6(io, fl).is_none()
}

// ---------------------------------------------------------------------------
// TCP socket 操作（N7.2：应用经 libnetv 调用）
// ---------------------------------------------------------------------------

fn tcp_open(st: &mut Stack, nic: usize, from: u64) -> u64 {
    if nic >= st.nics {
        return 0;
    }
    let mut i = 0;
    while i < MAX_TCONS {
        if !st.tcons[i].used {
            st.tcons[i] = TcpConn::new();
            st.tcons[i].used = true;
            st.tcons[i].owner = from;
            st.tcons[i].nic = nic;
            return (i + 1) as u64;
        }
        i += 1;
    }
    0
}

/// 建一个监听者（被动打开）。返回连接 id（>0）/ 0。
///
/// 端口归属必须是发起域（内核已按其 `Net` 能力登记）—— 与 `tcp_bind` 同一道门禁，
/// 否则任何域都能监听任意端口。
fn tcp_listen_internal(st: &mut Stack, nic: usize, port: u16, from: u64) -> u64 {
    if nic >= st.nics || sys_net_owner(port) != from {
        return 0;
    }
    let mut i = 0;
    while i < MAX_TCONS {
        if !st.tcons[i].used {
            let mut c = TcpConn::new();
            c.used = true;
            c.owner = from;
            c.role = TCP_ROLE_LISTENER;
            c.state = TcpState::Listen;
            c.nic = nic;
            c.local_port = port;
            c.mac = st.links[nic].mac;
            st.tcons[i] = c;
            return (i + 1) as u64;
        }
        i += 1;
    }
    0
}

/// 接受一个属于监听者 `id` 的已建立服务端连接，返回其 id（>0）/ 0。
///
/// 只有**监听者的归属域**能 accept（`listener.owner == from`）；被接受的连接随后归该域
/// 所有（写 `owner`），故 `TSEND`/`TRECV`/`TCLOSE` 也须来自同一域。
fn tcp_accept_internal(st: &mut Stack, id: u64, from: u64) -> u64 {
    let (port, nic) = match tcp_slot(st, id) {
        Some(c) if c.role == TCP_ROLE_LISTENER && c.owner == from => (c.local_port, c.nic),
        _ => return 0,
    };
    let mut i = 0;
    while i < MAX_TCONS {
        let c = &st.tcons[i];
        if c.used
            && c.role == TCP_ROLE_SERVER
            && c.state == TcpState::Established
            && !c.accepted
            && c.local_port == port
            && c.nic == nic
        {
            st.tcons[i].accepted = true;
            st.tcons[i].owner = from;
            return (i + 1) as u64;
        }
        i += 1;
    }
    0
}

fn tcp_slot(st: &mut Stack, id: u64) -> Option<&mut TcpConn> {
    if id == 0 || id as usize > MAX_TCONS {
        return None;
    }
    let c = &mut st.tcons[id as usize - 1];
    if c.used {
        Some(c)
    } else {
        None
    }
}

/// 取连接槽并要求归属域为 `from`（越权 / 不存在返回 `None`）。
fn tcp_slot_owned(st: &mut Stack, id: u64, from: u64) -> Option<&mut TcpConn> {
    match tcp_slot(st, id) {
        Some(c) if c.owner == from => Some(c),
        _ => None,
    }
}

/// 绑本地端口（端口归属必须是发起域，内核已按其 `Net` 能力登记）。
fn tcp_bind(st: &mut Stack, id: u64, port: u16, from: u64) -> u64 {
    if sys_net_owner(port) != from {
        return 0;
    }
    match tcp_slot_owned(st, id, from) {
        Some(c) if c.local_port == 0 => {
            c.local_port = port;
            1
        }
        _ => 0,
    }
}

/// 主动连接：需已绑本地端口、网卡已学网关 MAC。发 SYN。
fn tcp_connect_op(st: &mut Stack, id: u64, addr: u64, port: u64, now: u64, from: u64) -> u64 {
    let nic = match tcp_slot_owned(st, id, from) {
        Some(c) => c.nic,
        None => return 0,
    };
    let dst_mac = match st.links[nic].gw_mac {
        Some(m) => m,
        None => return 0,
    };
    let remote = [
        ((addr >> 24) & 0xff) as u8,
        ((addr >> 16) & 0xff) as u8,
        ((addr >> 8) & 0xff) as u8,
        (addr & 0xff) as u8,
    ];
    let io = st.links[nic].io;
    let c = &mut st.tcons[id as usize - 1];
    if c.local_port == 0 || c.state != TcpState::Closed {
        return 0;
    }
    c.remote_ip = remote;
    c.remote_port = port as u16;
    c.mac = st.links[nic].mac;
    c.dst_mac = dst_mac;
    let isn = 0x4d4f_0000u32 ^ (id as u32).wrapping_mul(0x0101_0101);
    let len = tcp_connect(c, io, isn, now);
    tcp_emit_deliver(st, nic, len, now);
    if len > 0 {
        1
    } else {
        0
    }
}

fn tcp_send_op(st: &mut Stack, id: u64, buf: u64, len: u64, now: u64, from: u64) -> u64 {
    if buf == 0 || len == 0 || len > TCP_MSS as u64 || sys_virt_to_phys(buf) == 0 {
        return 0;
    }
    let nic = match tcp_slot_owned(st, id, from) {
        Some(c) => c.nic,
        None => return 0,
    };
    let io = st.links[nic].io;
    let payload = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize) };
    let fl = tcp_send(&mut st.tcons[id as usize - 1], io, payload, now);
    tcp_emit_deliver(st, nic, fl, now);
    if fl > 0 {
        1
    } else {
        0
    }
}

fn tcp_recv_op(st: &mut Stack, id: u64, buf: u64, from: u64) -> u64 {
    if buf == 0 || sys_virt_to_phys(buf) == 0 {
        return 0;
    }
    match tcp_slot_owned(st, id, from) {
        Some(c) if c.rx_len > 0 => {
            unsafe {
                core::ptr::copy_nonoverlapping(c.rx.as_ptr(), buf as *mut u8, c.rx_len as usize);
            }
            let n = c.rx_len;
            c.rx_len = 0;
            n
        }
        _ => 0,
    }
}

fn tcp_close_op(st: &mut Stack, id: u64, now: u64, from: u64) -> u64 {
    let nic = match tcp_slot_owned(st, id, from) {
        Some(c) => c.nic,
        None => return 0,
    };
    let io = st.links[nic].io;
    let fl = tcp_close(&mut st.tcons[id as usize - 1], io, now);
    tcp_emit_deliver(st, nic, fl, now);
    if fl == 0 {
        // 已经关闭 / 处于不可关状态：直接释放槽位。
        if id != 0 && (id as usize) <= MAX_TCONS {
            st.tcons[id as usize - 1] = TcpConn::new();
        }
    }
    1
}

/// 段是否属于连接 `c`（网卡 + 本地/远端端口 + L3 源地址，v4/v6 按 `family` 区分）。
fn tcp_match(c: &TcpConn, nic: usize, src: SrcId, seg: &TcpSeg) -> bool {
    if !c.used || c.nic != nic || c.local_port != seg.dport || c.remote_port != seg.sport {
        return false;
    }
    match (c.family, src) {
        (4, SrcId::V4(a)) => c.remote_ip == a,
        (6, SrcId::V6(a)) => v6_eq(&c.remote6, &a),
        _ => false,
    }
}

/// 处理某网卡收到的一个 TCP 段；若有回应段，构造在链路共享页并返回帧长。
fn tcp_input(st: &mut Stack, nic: usize, src: SrcId, seg: &TcpSeg, now: u64) -> u64 {
    let io = st.links[nic].io;
    let mut idx = usize::MAX;
    let mut i = 0;
    while i < MAX_TCONS {
        if tcp_match(&st.tcons[i], nic, src, seg) {
            idx = i;
            break;
        }
        i += 1;
    }
    if idx == usize::MAX {
        // 被动打开: 找在该端口上监听的监听者; 仅 SYN 建连。
        if seg.flags & TCP_SYN == 0 {
            return 0;
        }
        let mut li = usize::MAX;
        let mut k = 0;
        while k < MAX_TCONS {
            let c = &st.tcons[k];
            if c.used
                && c.role == TCP_ROLE_LISTENER
                && c.state == TcpState::Listen
                && c.nic == nic
                && c.local_port == seg.dport
            {
                li = k;
                break;
            }
            k += 1;
        }
        if li == usize::MAX {
            return 0;
        }
        let mut si = usize::MAX;
        let mut m = 0;
        while m < MAX_TCONS {
            if !st.tcons[m].used {
                si = m;
                break;
            }
            m += 1;
        }
        if si == usize::MAX {
            return 0;
        }
        // 新建服务端连接 (SynRcvd), 回 SYN-ACK。
        let mut c = TcpConn::new();
        c.used = true;
        c.role = TCP_ROLE_SERVER;
        c.state = TcpState::SynRcvd;
        c.nic = nic;
        c.mac = st.links[nic].mac;
        c.local_port = seg.dport;
        match src {
            SrcId::V4(a) => {
                c.family = 4;
                c.remote_ip = a;
                c.dst_mac = st.links[nic].gw_mac.unwrap_or([0; 6]);
            }
            SrcId::V6(a) => {
                c.family = 6;
                c.remote6 = a;
                c.local6 = link_v6_primary(st, nic);
                c.dst_mac = st.links[nic]
                    .gw6_mac
                    .or(st.links[nic].gw_mac)
                    .unwrap_or([0; 6]);
            }
        }
        c.remote_port = seg.sport;
        c.rcv_nxt = seg.seq.wrapping_add(1);
        c.snd_una = 0x4d4f_9000u32 ^ (si as u32).wrapping_mul(0x0101);
        c.snd_nxt = c.snd_una;
        c.rto_ms = TCP_RTO_MS;
        c.last_tx_ms = now;
        st.tcons[si] = c;
        let io = st.links[nic].io;
        let len = tcp_seg(
            &st.tcons[si],
            io,
            st.tcons[si].snd_una,
            st.tcons[si].rcv_nxt,
            TCP_SYN | TCP_ACK,
            &[],
        );
        st.tcons[si].snd_nxt = st.tcons[si].snd_una.wrapping_add(1);
        return len;
    }
    let was = st.tcons[idx].state;
    let len = tcp_handle(&mut st.tcons[idx], io, seg, now);
    if was == TcpState::SynSent
        && st.tcons[idx].state == TcpState::Closed
        && seg.flags & TCP_RST != 0
    {
        println("netstack: tcp peer refused (RST) OK");
    }
    len
}

/// 把 `io` 里已构造好的一个段送出：目的为本机 (`OUR_IP`) 走**回环**投递给栈内对端，
/// 否则经网卡发到链路上。
fn tcp_emit_deliver(st: &mut Stack, nic: usize, len: u64, now: u64) {
    if len == 0 {
        return;
    }
    let io = st.links[nic].io;
    let local = match ipv4_parse(io, len) {
        Some(info) => info.dst == OUR_IP,
        None => false,
    };
    if local {
        lo_pump(st, nic, len, now);
    } else {
        let _ = link_tx(&st.links[nic], len);
    }
}

/// 本机回环 pump：把 `io` 里的段当作"从 `OUR_IP` 收到"反复投递给栈，直到不再产生回应。
/// 每次回应覆盖 `io`；`guard` 防死循环。这样客户机内客户端↔服务端可完成完整握手/收发。
fn lo_pump(st: &mut Stack, nic: usize, mut len: u64, now: u64) {
    let mut guard = 0u32;
    while guard < 32 && len > 0 {
        guard += 1;
        let f = st.links[nic].io;
        let info = match ipv4_parse(f, len) {
            Some(i) => i,
            None => break,
        };
        let seg = match tcp_parse(&info) {
            Some(s) => s,
            None => break,
        };
        len = tcp_input(st, nic, SrcId::V4(info.src), &seg, now);
    }
}

/// 推进所有 TCP 连接的重传定时器；需重传则发帧。
fn tcp_tick_all(st: &mut Stack, now: u64) {
    let mut i = 0;
    while i < MAX_TCONS {
        if st.tcons[i].used && st.tcons[i].state != TcpState::Closed {
            let nic = st.tcons[i].nic;
            let io = st.links[nic].io;
            let len = tcp_tick(&mut st.tcons[i], io, now);
            tcp_emit_deliver(st, nic, len, now);
        }
        i += 1;
    }
}

// ---------------------------------------------------------------------------
// 套接字服务（应用侧 IPC）
// ---------------------------------------------------------------------------

fn sock_alloc(st: &mut Stack, nic: usize) -> u64 {
    for (i, s) in st.socks.iter_mut().enumerate() {
        if !s.used {
            *s = Sock::new();
            s.used = true;
            s.nic = nic;
            return (i + 1) as u64;
        }
    }
    0
}

fn sock_slot(st: &mut Stack, id: u64) -> Option<&mut Sock> {
    if id == 0 || id as usize > MAX_SOCKS {
        return None;
    }
    let s = &mut st.socks[id as usize - 1];
    if s.used {
        Some(s)
    } else {
        None
    }
}

/// `NETS_OP_BIND`: 端口归属必须是发起域（内核已按其 `Net` 能力登记）。
fn sock_bind(st: &mut Stack, id: u64, port: u16, from: u64) -> u64 {
    if sys_net_owner(port) != from {
        return 0; // 调用者没在内核登记过该端口 → 越权
    }
    match sock_slot(st, id) {
        Some(s) if s.port == 0 => {
            s.port = port;
            1
        }
        _ => 0,
    }
}

/// `NETS_OP_SENDTO`: 本机目标走回环投递；否则经 socket 所属网卡发（需已知网关 MAC）。
fn sock_sendto(st: &mut Stack, req: &NetSReq) -> u64 {
    if req.buf == 0 || req.len == 0 || req.len > PAY_MAX as u64 || sys_virt_to_phys(req.buf) == 0 {
        return 0;
    }
    let (sport, nic) = match sock_slot(st, req.sock) {
        Some(s) => (s.port, s.nic),
        None => return 0,
    };
    if nic >= st.nics || !st.links[nic].up {
        return 0;
    }
    let dst = [
        ((req.addr >> 24) & 0xff) as u8,
        ((req.addr >> 16) & 0xff) as u8,
        ((req.addr >> 8) & 0xff) as u8,
        (req.addr & 0xff) as u8,
    ];
    let dport = req.port as u16;
    if dst == OUR_IP {
        deliver(st, nic, dport, req.buf, req.len); // 回环（用途确定性自测）
        return 1;
    }
    match st.links[nic].gw_mac {
        Some(gm) => {
            let len = build_udp(&st.links[nic], gm, dst, sport, dport, req.buf, req.len);
            if link_tx(&st.links[nic], len) {
                1
            } else {
                0
            }
        }
        None => 0,
    }
}

/// 若 `a` 是 v4-mapped 地址 `::ffff:a.b.c.d`，取出其中的 IPv4。
fn v4_mapped(a: &[u8; 16]) -> Option<[u8; 4]> {
    let mut i = 0;
    while i < 10 {
        if a[i] != 0 {
            return None;
        }
        i += 1;
    }
    if a[10] != 0xff || a[11] != 0xff {
        return None;
    }
    Some([a[12], a[13], a[14], a[15]])
}

/// 环回地址 `::1`。
fn v6_is_loopback(a: &[u8; 16]) -> bool {
    let mut i = 0;
    while i < 15 {
        if a[i] != 0 {
            return false;
        }
        i += 1;
    }
    a[15] == 1
}

/// `NETS_OP_SENDTO6`（V6.4 双栈）：目的 IPv6 在 `buf[0..16]`，负载在 `buf[16..16+len]`。
/// `::1` / 本机 v6 地址 → 栈内回环投递；`::ffff:a.b.c.d`（v4-mapped）→ 按 IPv4 走；
/// 其余 → 经网卡发 UDPv6（需已学路由器 MAC）。上层 socket 因此**对 v4/v6 双栈**。
fn sock_sendto6(st: &mut Stack, req: &NetSReq) -> u64 {
    if req.buf == 0 || req.len > PAY_MAX as u64 || sys_virt_to_phys(req.buf) == 0 {
        return 0;
    }
    let (sport, nic) = match sock_slot(st, req.sock) {
        Some(s) => (s.port, s.nic),
        None => return 0,
    };
    if nic >= st.nics || !st.links[nic].up {
        return 0;
    }
    let dst6 = get_v6(req.buf);
    let dport = req.port as u16;
    let pl = req.buf + 16;
    if let Some(v4) = v4_mapped(&dst6) {
        if v4 == OUR_IP {
            deliver(st, nic, dport, pl, req.len);
            return 1;
        }
        return match st.links[nic].gw_mac {
            Some(gm) => {
                let len = build_udp(&st.links[nic], gm, v4, sport, dport, pl, req.len);
                if link_tx(&st.links[nic], len) {
                    1
                } else {
                    0
                }
            }
            None => 0,
        };
    }
    let ll = link_local_from_mac(mac_bytes(st.links[nic].mac));
    if v6_is_loopback(&dst6) || v6_eq(&dst6, &ll) || v6_eq(&dst6, &st.links[nic].v6_global) {
        deliver(st, nic, dport, pl, req.len);
        return 1;
    }
    match st.links[nic].gw6_mac {
        Some(gm) => {
            let src = link_v6_primary(st, nic);
            let io = st.links[nic].io;
            let mac = mac_bytes(st.links[nic].mac);
            let len = build_udp6(io, mac, gm, &src, &dst6, sport, dport, pl, req.len);
            if link_tx(&st.links[nic], len) {
                1
            } else {
                0
            }
        }
        None => 0,
    }
}

/// `NETS_OP_RECVFROM`: 有数据则拷进共享页 `buf`，回复长度。
fn sock_recvfrom(st: &mut Stack, req: &NetSReq) -> u64 {
    if req.buf == 0 || sys_virt_to_phys(req.buf) == 0 {
        return 0;
    }
    match sock_slot(st, req.sock) {
        Some(s) if s.len > 0 => {
            unsafe {
                core::ptr::copy_nonoverlapping(s.data.as_ptr(), req.buf as *mut u8, s.len as usize);
            }
            let n = s.len;
            s.len = 0;
            n
        }
        _ => 0,
    }
}

fn sock_close(st: &mut Stack, id: u64) -> u64 {
    if id == 0 || id as usize > MAX_SOCKS {
        return 0;
    }
    st.socks[id as usize - 1] = Sock::new();
    1
}

// ---------------------------------------------------------------------------
// Shell 支持 op：链路状态（`net`）+ ICMP echo（`ping` / `ping6`）
// ---------------------------------------------------------------------------

/// ping 的有界等待上限（毫秒）。
const PING_TIMEOUT_MS: u64 = 1000;

/// 请求里的网卡索引（越界钳到 0）。
fn req_nic(st: &Stack, req: &NetSReq) -> usize {
    let i = req.sock as usize;
    if i < st.nics {
        i
    } else {
        0
    }
}

fn pack_ip4(ip: [u8; 4]) -> u64 {
    ((ip[0] as u64) << 24) | ((ip[1] as u64) << 16) | ((ip[2] as u64) << 8) | ip[3] as u64
}
fn unpack_ip4(v: u64) -> [u8; 4] {
    [(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// 构造 IPv4 ICMP echo request（目的 `dst`，负载从 `payload_va` 拷入）；返回帧长。
#[allow(clippy::too_many_arguments)]
fn build_echo4(
    io: u64,
    mac: [u8; 6],
    dst_mac: [u8; 6],
    dst: [u8; 4],
    id: u16,
    seq: u16,
    payload_va: u64,
    plen: u64,
) -> u64 {
    put_mac(io, dst_mac);
    put_mac(io + 6, mac);
    wr16be(io + 12, ETH_IPV4);
    let ip = io + 14;
    let total = 20 + 8 + plen;
    wr8(ip, 0x45);
    wr8(ip + 1, 0);
    wr16be(ip + 2, total as u16);
    wr16be(ip + 4, 0);
    wr16be(ip + 6, 0x4000); // DF
    wr8(ip + 8, 64);
    wr8(ip + 9, IP_PROTO_ICMP);
    wr16be(ip + 10, 0);
    for (i, b) in OUR_IP.iter().enumerate() {
        wr8(ip + 12 + i as u64, *b);
        wr8(ip + 16 + i as u64, dst[i]);
    }
    wr16be(ip + 10, csum(ip, 20));
    let ic = ip + 20;
    wr8(ic, ICMP_ECHO_REQ);
    wr8(ic + 1, 0);
    wr16be(ic + 2, 0); // 算校验和前先清零
    wr16be(ic + 4, id);
    wr16be(ic + 6, seq);
    unsafe {
        core::ptr::copy_nonoverlapping(payload_va as *const u8, (ic + 8) as *mut u8, plen as usize);
    }
    wr16be(ic + 2, csum(ic, 8 + plen));
    14 + total
}

/// 判定共享页里的一帧是否为我们发出的 IPv4 ICMP echo 的应答（源 = `dst`，id/seq 匹配）。
fn is_echo_reply4(io: u64, n: u64, dst: [u8; 4], id: u16, seq: u16) -> bool {
    if n < 14 + 20 + 8 || rd16be(io + 12) != ETH_IPV4 {
        return false;
    }
    let ip = io + 14;
    let ihl = ((rd8(ip) & 0x0f) as u64) * 4;
    if ihl < 20 || n < 14 + ihl + 8 || rd8(ip + 9) != IP_PROTO_ICMP {
        return false;
    }
    let src = [rd8(ip + 12), rd8(ip + 13), rd8(ip + 14), rd8(ip + 15)];
    if src != dst {
        return false;
    }
    let ic = ip + ihl;
    rd8(ic) == ICMP_ECHO_REPLY && rd16be(ic + 4) == id && rd16be(ic + 6) == seq
}

/// `NETS_OP_NETINFO`：把链路状态写入共享页 `buf`（`sock` = nic 或 `u64::MAX` = 全部），返回条数。
fn sock_netinfo(st: &Stack, req: &NetSReq) -> u64 {
    let all = req.sock == u64::MAX;
    let ent = core::mem::size_of::<NetLinkInfo>() as u64;
    let mut count = 0u64;
    let mut i = 0usize;
    while i < st.nics {
        if all || i as u64 == req.sock {
            let l = &st.links[i];
            let info = NetLinkInfo {
                nic: i as u64,
                kind: l.kind,
                up: if l.up { 1 } else { 0 },
                v4: pack_ip4(OUR_IP),
                gw4: pack_ip4(GW_IP),
                mac: l.mac,
                v6_up: if l.v6_up { 1 } else { 0 },
                v6: l.v6_global,
                v6_gw: l.v6_gw,
            };
            unsafe {
                core::ptr::write_unaligned((req.buf + count * ent) as *mut NetLinkInfo, info);
            }
            count += 1;
        }
        i += 1;
    }
    count
}

/// `NETS_OP_PING4`：发 ICMP echo 到 `addr`，有界等待应答。回复 1（收到）/ 0。
fn sock_ping4(st: &mut Stack, req: &NetSReq, now: u64) -> u64 {
    let nic = req_nic(st, req);
    if !st.links[nic].up {
        return 0;
    }
    let dst = unpack_ip4(req.addr);
    if dst == OUR_IP {
        return 1; // ping 本机
    }
    if st.links[nic].gw_mac.is_none() {
        arp_learn_gw(st, nic);
    }
    let gw = match st.links[nic].gw_mac {
        Some(m) => m,
        None => return 0,
    };
    let id = 0x4d4fu16;
    let seq = 1u16;
    let payload = b"morion-ping";
    let len = build_echo4(
        st.links[nic].io,
        mac_bytes(st.links[nic].mac),
        gw,
        dst,
        id,
        seq,
        payload.as_ptr() as u64,
        payload.len() as u64,
    );
    if !link_tx(&st.links[nic], len) {
        return 0;
    }
    let mut waited = 0u64;
    while waited < PING_TIMEOUT_MS {
        while let Some(n) = link_rx(&st.links[nic]) {
            if is_echo_reply4(st.links[nic].io, n, dst, id, seq) {
                return 1;
            }
            handle_frame(st, nic, n, now);
        }
        sys_sleep(5);
        waited += 5;
    }
    0
}

/// `NETS_OP_PING6`：目的 IPv6 在共享页 `buf[0..16]`；有界等 ICMPv6 echo reply。回复 1/0。
fn sock_ping6(st: &mut Stack, req: &NetSReq, now: u64) -> u64 {
    let nic = req_nic(st, req);
    if !st.links[nic].up || !st.links[nic].v6_up {
        return 0;
    }
    let dst = get_v6(req.buf);
    let io = st.links[nic].io;
    let mac = mac_bytes(st.links[nic].mac);
    // 目的为链路本地（fe80::/10）时源也用链路本地，否则用 SLAAC 全局地址。
    let link_local = dst[0] == 0xfe && (dst[1] & 0xc0) == 0x80;
    let src = if link_local {
        link_local_from_mac(mac)
    } else {
        link_v6_primary(st, nic)
    };
    if v6_eq(&dst, &src) {
        return 1; // ping 本机
    }
    let gw_mac = match st.links[nic].gw6_mac {
        Some(m) => m,
        None => return 0, // 尚无路由器 MAC（未收到 RA/未解出）
    };
    // 链路本地目的只支持路由器本身（其余需 NDP 解析，暂不支持）。
    if link_local && !v6_eq(&dst, &st.links[nic].v6_gw) {
        return 0;
    }
    let id = 0x4d4fu16;
    let seq = 1u16;
    let payload = b"morion-ping6";
    let len = build_echo6(
        io,
        mac,
        gw_mac,
        &src,
        &dst,
        id,
        seq,
        payload.as_ptr() as u64,
        payload.len() as u64,
    );
    if !link_tx(&st.links[nic], len) {
        return 0;
    }
    let mut waited = 0u64;
    while waited < PING_TIMEOUT_MS {
        while let Some(n) = link_rx(&st.links[nic]) {
            if rd16be(io + 12) == ETH_IPV6 {
                if let Some(info) = ipv6_parse(io, n) {
                    if info.next == IP6_PROTO_ICMPV6
                        && rd8(info.payload_off) == ICMPV6_ECHO_REPLY
                        && rd16be(info.payload_off + 4) == id
                        && rd16be(info.payload_off + 6) == seq
                        && v6_eq(&info.src, &dst)
                    {
                        return 1;
                    }
                }
            }
            handle_frame(st, nic, n, now);
        }
        sys_sleep(5);
        waited += 5;
    }
    0
}

fn serve_app(st: &mut Stack, now: u64) {
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        if sys_try_recv(&mut msg as *mut Message as *mut u8) == u64::MAX {
            return;
        }
        if msg.tag != NETS_REQ_TAG {
            let _ = sys_reply(0);
            continue;
        }
        let req: NetSReq =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const NetSReq) };
        let reply = match req.op {
            // SOCKET: `sock` 字段 = 请求的网卡索引（0/1）。
            NETS_OP_SOCKET => {
                if (req.sock as usize) < st.nics {
                    sock_alloc(st, req.sock as usize)
                } else {
                    0
                }
            }
            NETS_OP_BIND => sock_bind(st, req.sock, req.port as u16, msg.from),
            NETS_OP_SENDTO => sock_sendto(st, &req),
            NETS_OP_SENDTO6 => sock_sendto6(st, &req),
            NETS_OP_RECVFROM => sock_recvfrom(st, &req),
            NETS_OP_CLOSE => sock_close(st, req.sock),
            // TCP（N7.2）：`sock` 字段对 TSOCKET/TLISTEN 是网卡索引, 其余是连接/监听者 id。
            NETS_OP_TSOCKET => tcp_open(st, req.sock as usize, msg.from),
            NETS_OP_TBIND => tcp_bind(st, req.sock, req.port as u16, msg.from),
            NETS_OP_TCONNECT => tcp_connect_op(st, req.sock, req.addr, req.port, now, msg.from),
            NETS_OP_TSEND => tcp_send_op(st, req.sock, req.buf, req.len, now, msg.from),
            NETS_OP_TRECV => tcp_recv_op(st, req.sock, req.buf, msg.from),
            NETS_OP_TCLOSE => tcp_close_op(st, req.sock, now, msg.from),
            NETS_OP_TLISTEN => {
                tcp_listen_internal(st, req.sock as usize, req.port as u16, msg.from)
            }
            NETS_OP_TACCEPT => tcp_accept_internal(st, req.sock, msg.from),
            // Shell 支持（网络完善）：链路状态查询 + ICMP echo（IPv4/IPv6）。
            NETS_OP_NETINFO => sock_netinfo(st, &req),
            NETS_OP_PING4 => sock_ping4(st, &req, now),
            NETS_OP_PING6 => sock_ping6(st, &req, now),
            _ => 0,
        };
        let _ = sys_reply(reply);
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 域 21 — netstack_srv 入口。
pub fn run() {
    // 状态在静态区（见 STACK 说明），不进栈帧。
    let st: &mut Stack = unsafe { &mut *core::ptr::addr_of_mut!(STACK) };

    // DRV-A: 读内核写入的只读 NIC 表（域号 + IO 页 VA + 型号）。
    let tbl = unsafe { &*(NIC_TABLE_VADDR as *const NicTable) };
    if tbl.magic != NIC_TABLE_MAGIC {
        println("netstack: no NIC table (kernel did not publish), idle");
        idle();
    }
    let count = (tbl.count as usize).min(NIC_MAX);
    if count == 0 {
        println("netstack: no NIC, idle");
        idle();
    }
    st.nics = count;

    // 按表逐条分配帧页并同址共享给对应驱动域。
    let mut i = 0usize;
    while i < count {
        let e = tbl.entries[i];
        st.links[i] = Link::down(e.domain, e.io_vaddr, e.kind);
        if sys_alloc_page(e.io_vaddr) != 1 || sys_share_page(e.io_vaddr, e.domain) != 1 {
            print("netstack: cannot share IO page (nic");
            print_u64(i as u64);
            println("), idle");
            idle();
        }
        i += 1;
    }

    // 逐个拉起网卡：NIC 0 等驱动自测完（重试），其余单次探测；取到 MAC 后学网关 MAC。
    let mut i = 0usize;
    while i < count {
        let mut mac = 0u64;
        let retries = if i == 0 { 200u64 } else { 1 };
        let mut t = 0u64;
        while t < retries {
            mac = link_call(&st.links[i], NET_OP_INFO, 0);
            if mac != 0 {
                break;
            }
            if retries > 1 {
                sys_sleep(50);
            }
            t += 1;
        }
        if mac == 0 {
            if i == 0 {
                // 第一台网卡缺失 → 协议栈无出口，退出到监督者重启（与旧行为一致）。
                println("netstack: no NIC (net_srv gave no MAC), idle");
                idle();
            }
            i += 1;
            continue;
        }
        st.links[i].mac = mac;
        st.links[i].up = true;
        arp_learn_gw(st, i);
        let svc = kind_svc_name(tbl.entries[i].kind);
        if i == 0 {
            print("netstack: up (frame link to ");
            print(svc);
            print(" OK), nic mac=0x");
        } else {
            print("netstack: nic");
            print_u64(i as u64);
            print(" up (");
            print(svc);
            print(" OK), mac=0x");
        }
        print_hex(mac);
        if st.links[i].gw_mac.is_some() {
            print(", gw mac learned");
        } else {
            print(", gw mac timeout");
        }
        println("");
        i += 1;
    }

    // DRV-A marker: 表驱动产出的网卡清单（加网卡只需内核往表里追加一条）。
    print("netstack: links=");
    print_u64(st.nics as u64);
    print(" (");
    let mut k = 0usize;
    while k < st.nics {
        if k != 0 {
            print("+");
        }
        print(kind_hw_name(tbl.entries[k].kind));
        k += 1;
    }
    println(")");

    // V6.1: NDP 确定性自证（构造 NS → 处理器产出 NA，校验类型与校验和）。
    if ndp_selftest(st, 0) {
        println("NET16 ndp self-test OK (ns->na)");
    } else {
        println("NET16 ndp self-test FAILED");
    }
    // V6.1: 发 RS 收 RA → SLAAC（前缀 fec0::/64 + EUI-64），并从 RA 的 SLLAO 记路由器 MAC。
    ipv6_probe(st, 0);

    // V6.2: ICMPv6 echo 确定性自证 + 真实链路 ping6 取证（best-effort）。
    if echo6_selftest(st, 0) {
        println("NET17 icmpv6 echo OK (req->reply)");
    } else {
        println("NET17 icmpv6 echo FAILED");
    }
    icmpv6_probe(st, 0);
    // V6.2: UDPv6（必需校验和 + 投递 + 端口不可达）确定性自证。
    if udp6_selftest(st, 0) {
        println("NET17 udp6 OK (checksum + deliver + port-unreach)");
    } else {
        println("NET17 udp6 FAILED");
    }
    // V6.2: TCPv6 复用同一套状态机 + v6 伪首部确定性自证。
    let our6 = link_local_from_mac(mac_bytes(st.links[0].mac));
    let peer6 = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
    if tcp6_selftest(st.links[0].io, our6, peer6) {
        println("NET18 tcp6 OK (state machine + v6 pseudo-header)");
    } else {
        println("NET18 tcp6 FAILED");
    }

    // N7: TCP 连接状态机 + 重传的确定性自证（不依赖真实对端）。
    if tcp_selftest(st.links[0].io) {
        println("NET6 tcp conn OK (state machine + retransmit + checksum)");
    } else {
        println("NET6 tcp conn FAILED");
    }

    // 内建 HTTP 已拆成独立服务 httpd_srv（域 23）：它自己 `tcp_listen(80)` + `tcp_accept`，
    // 协议栈只提供 TCP 原语，不再内含任何应用层服务。

    // 主循环: 服务应用请求 + 排空各网卡 RX + 推进 TCP 重传定时器。
    let mut now = 0u64;
    loop {
        now = now.wrapping_add(10);
        serve_app(st, now);
        let mut nic = 0;
        while nic < st.nics {
            if st.links[nic].up {
                let mut n = 0;
                while n < 8 {
                    let got = link_rx(&st.links[nic]);
                    match got {
                        Some(len) => {
                            handle_frame(st, nic, len, now);
                            n += 1;
                        }
                        None => break,
                    }
                }
            }
            nic += 1;
        }
        tcp_tick_all(st, now);
        sys_sleep(10);
    }
}
