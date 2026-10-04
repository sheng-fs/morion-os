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

/// NIC 0：net_srv（virtio-net，域 16）。
const NET_DOMAIN: u64 = 16;
/// NIC 1：e1000e_srv（e1000e，域 22，N9）。
const E1000E_DOMAIN: u64 = 22;
/// 各网卡收发帧的共享页（同址共享）。
const IO0_VADDR: u64 = 0x0000_0080_001A_0000;
const IO1_VADDR: u64 = 0x0000_0080_001A_1000;
/// 网卡数量。
const NIC_COUNT: usize = 2;

/// 本机静态配置（与网卡驱动的 DHCP 回落值一致）。
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
// 网卡链路（帧级通道）
// ---------------------------------------------------------------------------

/// 一条发送/接收裸以太帧的链路（对应内核交出的一台网卡驱动服务）。
#[derive(Clone, Copy)]
struct Link {
    domain: u64,
    io: u64,
    mac: u64,
    gw_mac: Option<[u8; 6]>,
    up: bool,
}

impl Link {
    const fn down(domain: u64, io: u64) -> Link {
        Link {
            domain,
            io,
            mac: 0,
            gw_mac: None,
            up: false,
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
    links: [Link; NIC_COUNT],
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

/// 学某网卡的网关 MAC（发 ARP 请求 + 有界收包）。取不到则保持 `None`。
fn arp_learn_gw(st: &mut Stack, nic: usize) {
    let mut tries = 0u64;
    while st.links[nic].gw_mac.is_none() && tries < 40 {
        let len = build_arp_request(&st.links[nic], GW_IP);
        let _ = link_tx(&st.links[nic], len);
        for _ in 0..5 {
            if let Some(n) = link_rx(&st.links[nic]) {
                handle_frame(st, nic, n);
            }
            sys_sleep(10);
        }
        tries += 1;
    }
}

/// 处理某网卡收到的一帧（帧在链路共享页，长 `n`）。
fn handle_frame(st: &mut Stack, nic: usize, n: u64) {
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

/// 把负载投递给**绑在该网卡**上、绑定 `dport` 的 socket。
fn deliver(st: &mut Stack, nic: usize, dport: u16, src_va: u64, len: u64) {
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
            return;
        }
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
    if nic >= NIC_COUNT || !st.links[nic].up {
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
            // SOCKET: `sock` 字段 = 请求的网卡索引（0/1）。
            NETS_OP_SOCKET => {
                if (req.sock as usize) < NIC_COUNT {
                    sock_alloc(st, req.sock as usize)
                } else {
                    0
                }
            }
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
    let mut st = Stack {
        links: [
            Link::down(NET_DOMAIN, IO0_VADDR),
            Link::down(E1000E_DOMAIN, IO1_VADDR),
        ],
        socks: [Sock::new(); MAX_SOCKS],
    };

    // NIC 0：virtio-net（必需）。分配帧页并共享给 net_srv。
    if sys_alloc_page(IO0_VADDR) != 1 || sys_share_page(IO0_VADDR, NET_DOMAIN) != 1 {
        println("netstack: cannot share IO page with net_srv, idle");
        idle();
    }
    // NIC 1：e1000e（可选）。页面照分配+共享；驱动给的 MAC 为 0 即视为不在。
    let _ = sys_alloc_page(IO1_VADDR);
    let _ = sys_share_page(IO1_VADDR, E1000E_DOMAIN);

    // 取 NIC 0 的 MAC（net_srv 可能还在跑自测，重试）。
    let mut mac0 = 0u64;
    let mut i = 0u64;
    while i < 200 {
        mac0 = link_call(&st.links[0], NET_OP_INFO, 0);
        if mac0 != 0 {
            break;
        }
        sys_sleep(50);
        i += 1;
    }
    if mac0 == 0 {
        println("netstack: no NIC (net_srv gave no MAC), idle");
        idle();
    }
    st.links[0].mac = mac0;
    st.links[0].up = true;
    arp_learn_gw(&mut st, 0);

    print("netstack: up (frame link to net_srv OK), nic mac=0x");
    print_hex(mac0);
    if st.links[0].gw_mac.is_some() {
        print(", gw mac learned");
    } else {
        print(", gw mac timeout");
    }
    println("");

    // NIC 1：e1000e（best-effort）。学网关 MAC 同时验证该网卡的 TX + RX 路径。
    let mac1 = link_call(&st.links[1], NET_OP_INFO, 0);
    if mac1 != 0 {
        st.links[1].mac = mac1;
        st.links[1].up = true;
        arp_learn_gw(&mut st, 1);
        print("netstack: nic1 up (e1000e OK), mac=0x");
        print_hex(mac1);
        if st.links[1].gw_mac.is_some() {
            print(", gw mac learned");
        } else {
            print(", gw mac timeout");
        }
        println("");
    } else {
        println("netstack: nic1 absent (no e1000e)");
    }

    // 主循环: 服务应用请求 + 排空各网卡 RX。
    loop {
        serve_app(&mut st);
        let mut nic = 0;
        while nic < NIC_COUNT {
            if st.links[nic].up {
                let mut n = 0;
                while n < 8 {
                    let got = link_rx(&st.links[nic]);
                    match got {
                        Some(len) => {
                            handle_frame(&mut st, nic, len);
                            n += 1;
                        }
                        None => break,
                    }
                }
            }
            nic += 1;
        }
        sys_sleep(10);
    }
}
