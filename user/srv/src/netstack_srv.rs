//! 域 21 — netstack_srv：用户态网络协议栈（N6）。
//!
//! 架构（见 `docs/plan-network.md`）：
//!
//! ```text
//! 应用 ── libnetv ──▶ netstack_srv(本服务: ARP/IPv4/UDP + socket + 端口能力门禁)
//!                          │  帧级 IPC (NetReq, 帧走共享页 IO_VADDR)
//!                          ▼
//!                     net_srv(域 16: 纯 NIC 驱动)
//! ```
//!
//! **N6.5/N6.6**：实现 IPv4/UDP 与 UDP socket 服务；`bind` 时用 `SYS_NET_OWNER` 核对
//! 发起域在内核登记的端口归属（不可伪造）；支持**本机自投递**（loopback，用途确定性自测）
//! 与经真实网卡的收发（先 ARP 学网关 MAC，再发帧；收到的帧解析后投递给 socket）。

use crate::common::*;
use morion::syscall::*;

/// net_srv 域号（帧级 NIC 驱动）。
const NET_DOMAIN: u64 = 16;

/// net_srv 收发帧的共享页（同址共享）。
const IO_VADDR: u64 = 0x0000_0080_001A_0000;

/// 本机静态配置（与 net_srv 的 DHCP 回落值一致）。
const OUR_IP: [u8; 4] = [10, 0, 2, 15];
const GW_IP: [u8; 4] = [10, 0, 2, 2];

const MAX_SOCKS: usize = 4;
const PAY_MAX: usize = NETS_PAYLOAD_MAX as usize;

const ETH_IPV4: u16 = 0x0800;
const ETH_ARP: u16 = 0x0806;
const IP_PROTO_ICMP: u8 = 1;
const IP_PROTO_UDP: u8 = 17;
const ICMP_UNREACH: u8 = 3;

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
// 与 net_srv 的帧级通道
// ---------------------------------------------------------------------------

fn link_call(op: u64, len: u64) -> u64 {
    let req = NetReq {
        op,
        len,
        buf: IO_VADDR,
    };
    let b = unsafe {
        core::slice::from_raw_parts(
            &req as *const NetReq as *const u8,
            core::mem::size_of::<NetReq>(),
        )
    };
    sys_call_payload(NET_DOMAIN, NET_REQ_TAG, b)
}
/// 发一帧（帧已构造在 `IO_VADDR` 页里）。
fn link_tx(len: u64) -> bool {
    link_call(NET_OP_TX, len) == 1
}
/// 收一帧到 `IO_VADDR`；返回帧长（无帧 `None`）。
fn link_rx() -> Option<u64> {
    let r = link_call(NET_OP_RX, 0);
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
    len: u64,
    data: [u8; PAY_MAX],
}

impl Sock {
    const fn new() -> Sock {
        Sock {
            used: false,
            port: 0,
            len: 0,
            data: [0; PAY_MAX],
        }
    }
}

struct Stack {
    mac: u64,
    gw_mac: Option<[u8; 6]>,
    socks: [Sock; MAX_SOCKS],
}

fn idle() -> ! {
    loop {
        sys_sleep(500);
    }
}

// ---------------------------------------------------------------------------
// 帧构造 / 解析
// ---------------------------------------------------------------------------

/// 广播一个 ARP 请求问 `target` 的 MAC；写进 `IO_VADDR`，返回帧长。
fn build_arp_request(our_mac: u64, target: [u8; 4]) -> u64 {
    let f = IO_VADDR;
    put_mac(f, [0xff; 6]);
    put_mac(f + 6, mac_bytes(our_mac));
    wr16be(f + 12, ETH_ARP);
    wr16be(f + 14, 0x0001); // htype = Ethernet
    wr16be(f + 16, ETH_IPV4); // ptype = IPv4
    wr8(f + 18, 6);
    wr8(f + 19, 4);
    wr16be(f + 20, 0x0001); // oper = request
    put_mac(f + 22, mac_bytes(our_mac));
    for (i, b) in OUR_IP.iter().enumerate() {
        wr8(f + 28 + i as u64, *b);
    }
    put_mac(f + 32, [0; 6]);
    for (i, b) in target.iter().enumerate() {
        wr8(f + 38 + i as u64, *b);
    }
    42
}

/// 把 UDP 数据报构造进 `IO_VADDR`（负载从 `payload_va` 拷入），返回帧长。
fn build_udp(
    st: &Stack,
    dst_mac: [u8; 6],
    dst_ip: [u8; 4],
    sport: u16,
    dport: u16,
    payload_va: u64,
    plen: u64,
) -> u64 {
    let f = IO_VADDR;
    put_mac(f, dst_mac);
    put_mac(f + 6, mac_bytes(st.mac));
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

/// 学网关 MAC（发 ARP 请求 + 有界收包）。取不到则保持在 `None`。
fn arp_learn_gw(st: &mut Stack) {
    let mut tries = 0u64;
    while st.gw_mac.is_none() && tries < 40 {
        let len = build_arp_request(st.mac, GW_IP);
        let _ = link_tx(len);
        for _ in 0..5 {
            if let Some(n) = link_rx() {
                handle_frame(st, n);
            }
            sys_sleep(10);
        }
        tries += 1;
    }
}

/// 处理一帧（帧在 `IO_VADDR`，长 `n`）。
fn handle_frame(st: &mut Stack, n: u64) {
    if n < 14 {
        return;
    }
    let f = IO_VADDR;
    let et = rd16be(f + 12);
    if et == ETH_ARP {
        if n >= 42 && rd16be(f + 20) == 0x0002 {
            let sip = [rd8(f + 28), rd8(f + 29), rd8(f + 30), rd8(f + 31)];
            if sip == GW_IP {
                st.gw_mac = Some([
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
    if proto == IP_PROTO_ICMP {
        let ic = ip + ihl;
        if n > 14 + ihl && rd8(ic) == ICMP_UNREACH {
            println("netstack: nic rx (icmp unreachable) OK");
        }
        return;
    }
    if proto == IP_PROTO_UDP && dst_ip == OUR_IP {
        let udp = ip + ihl;
        let dport = rd16be(udp + 2);
        let ulen = rd16be(udp + 4) as u64;
        if ulen >= 8 && n >= 14 + ihl + ulen {
            deliver(st, dport, udp + 8, ulen - 8);
        }
    }
}

/// 把负载投递给绑定 `dport` 的 socket。
fn deliver(st: &mut Stack, dport: u16, src_va: u64, len: u64) {
    for s in st.socks.iter_mut() {
        if s.used && s.port == dport && len <= PAY_MAX as u64 {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src_va as *const u8,
                    s.data.as_mut_ptr(),
                    len as usize,
                );
            }
            s.len = len;
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// 套接字服务（应用侧 IPC）
// ---------------------------------------------------------------------------

fn sock_alloc(st: &mut Stack) -> u64 {
    for (i, s) in st.socks.iter_mut().enumerate() {
        if !s.used {
            *s = Sock::new();
            s.used = true;
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

/// `NETS_OP_SENDTO`: 本机目标走回环投递；否则经真实网卡发（需已知网关 MAC）。
fn sock_sendto(st: &mut Stack, req: &NetSReq) -> u64 {
    if req.buf == 0 || req.len == 0 || req.len > PAY_MAX as u64 || sys_virt_to_phys(req.buf) == 0 {
        return 0;
    }
    let sport = match sock_slot(st, req.sock) {
        Some(s) => s.port,
        None => return 0,
    };
    let dst = [
        ((req.addr >> 24) & 0xff) as u8,
        ((req.addr >> 16) & 0xff) as u8,
        ((req.addr >> 8) & 0xff) as u8,
        (req.addr & 0xff) as u8,
    ];
    let dport = req.port as u16;
    if dst == OUR_IP {
        deliver(st, dport, req.buf, req.len); // 回环（用途确定性自测）
        return 1;
    }
    match st.gw_mac {
        Some(gm) => {
            let len = build_udp(st, gm, dst, sport, dport, req.buf, req.len);
            if link_tx(len) {
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

fn serve_app(st: &mut Stack) {
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
            NETS_OP_SOCKET => sock_alloc(st),
            NETS_OP_BIND => sock_bind(st, req.sock, req.port as u16, msg.from),
            NETS_OP_SENDTO => sock_sendto(st, &req),
            NETS_OP_RECVFROM => sock_recvfrom(st, &req),
            NETS_OP_CLOSE => sock_close(st, req.sock),
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
    if sys_alloc_page(IO_VADDR) != 1 || sys_share_page(IO_VADDR, NET_DOMAIN) != 1 {
        println("netstack: cannot share IO page with net_srv, idle");
        idle();
    }

    // 取网卡 MAC（net_srv 可能还在跑自测，重试）。
    let mut mac = 0u64;
    let mut i = 0u64;
    while i < 200 {
        mac = link_call(NET_OP_INFO, 0);
        if mac != 0 {
            break;
        }
        sys_sleep(50);
        i += 1;
    }
    if mac == 0 {
        println("netstack: no NIC (net_srv gave no MAC), idle");
        idle();
    }

    let mut st = Stack {
        mac,
        gw_mac: None,
        socks: [Sock::new(); MAX_SOCKS],
    };
    // 学网关 MAC（同时验证 netstack 的 TX + RX 路径）。
    arp_learn_gw(&mut st);

    print("netstack: up (frame link to net_srv OK), nic mac=0x");
    print_hex(mac);
    if st.gw_mac.is_some() {
        print(", gw mac learned");
    } else {
        print(", gw mac timeout");
    }
    println("");

    // 主循环: 服务应用请求 + 排空 NIC RX。
    loop {
        serve_app(&mut st);
        let mut n = 0;
        while n < 8 {
            match link_rx() {
                Some(len) => {
                    handle_frame(&mut st, len);
                    n += 1;
                }
                None => break,
            }
        }
        sys_sleep(10);
    }
}
