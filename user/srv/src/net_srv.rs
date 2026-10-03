//! 域 16 — 网络驱动服务（virtio-net，驱动路线 **N0–N3**；**D2b** 起传输层与 vring 用 `libdevice::virtio`）。
//!
//! **N0**：只起域 + 报到 + 保持存活。
//! **N1**：内核按类找到 virtio-net、用通用 `device::grant` 交出 `DeviceGrant`。
//! **N2**：用户态 virtio-net modern 驱动 —— 通过 `SYS_DEVICE_CONFIG_READ` 自行解析
//! virtio PCI 能力（common/notify/ISR/device 四个区域都在内核交给的 BAR 里）→ 复位 → 协商
//! 特性 → 读 MAC → 建 RX/TX virtqueue（环落在内核交出的**连续 DMA 块**里）→ 投 RX 缓冲 →
//! `DRIVER_OK` → 收帧；MSI-X 表在 BAR1，内核把它另映射给本域（`msix_table_vaddr`），
//! 驱动写表项 + 注册向量，**中断驱动**收 RX（无中断则回落轮询）。
//! **N3**：发广播 ARP 请求问网关 MAC → 收应答（`NET1 virtio-net up, MAC=…, ARP reply OK`，
//! 端到端取证，同时验证 TX/RX 与中断链路）。
//! **N3b**：网卡之上的**最小 IPv4 栈** —— IPv4 头构造/解析（总长/协议/头校验和、拒分片）、
//! ICMP echo（发 request 收 reply 端到端 + 收 request 回 reply，后者以无对端合成请求自证）、
//! UDP 构造/发送（slirp 对未监听端口回 ICMP 端口不可达作副证据）。自测标记 `NET2 ipv4/icmp …`。
//!
//! **D2b**：把「所有 virtio 设备都一样」的传输层（能力解析 / common cfg / 复位协商 / 队列
//! 配置 / avail·used 环）搬进 [`libdevice::virtio`]，与 `virtio_blk_srv` 共用 —— 本文件因此
//! 只剩**网卡语义**（包头长度、ARP 报文、收帧回收策略）。
//!
//! 内核侧只交出"BAR + DMA 块 + 配置空间只读通道"，设备协议全在本域 —— 这正是 D1/N 的目的。

use libdevice::grant::DeviceGrant;
use libdevice::mmio::{rd16, rd32, rd8, wr8};
use libdevice::msix;
use libdevice::virtio::{self, Vq};
use morion::syscall::*;

/// virtio-net 特性位：`VIRTIO_NET_F_MAC`（feature word 0 bit 5）。
const FEAT_NET_MAC: u32 = 1 << 5;

// ===========================================================================
// 环与缓冲布局（**本驱动自己**决定，内核不参与）
// ===========================================================================

const PAGE: u64 = virtio::PAGE;
/// virtqueue 深度（取 2 的幂；RX/TX 各一个队列）。
const Q_SIZE: u16 = 8;
/// RX 队列页：desc@+0 / avail@+0x100 / used@+0x200（深度 8 时都在一页内）。
const RX_RING_PAGE: u64 = 0;
/// TX 队列页（发包用）。
const TX_RING_PAGE: u64 = 1;
/// RX 缓冲起点（页号）；每格 `BUF_SZ` 字节。
const RX_BUF_PAGE: u64 = 2;
const BUF_SZ: u64 = 2048;
/// 本驱动需要的 DMA 页数（与 `main.rs` 里给 net 声明的 `dma_pages: 8` 一致）。
const DMA_PAGES: u64 = 8;
/// 中断模式下 `SYS_IRQ_WAIT` 的超时（毫秒）：超时即回落重扫 used 环，兼顾延迟与兜底。
const IRQ_WAIT_MS: u64 = 200;
/// TX 缓冲页（发 ARP 用；排在 RX 缓冲之后的空闲页）。
const TX_BUF_PAGE: u64 = 6;
/// virtio-net 包头长度：**modern（`VIRTIO_F_VERSION_1`）恒为 12 字节**
/// （`num_buffers` 字段总是存在；只有 legacy 且未协商 `MRG_RXBUF` 时才是 10）。
const VNET_HDR_LEN: u64 = 12;
/// 自测地址（QEMU user-net 约定：`10.0.2.0/24`，guest `10.0.2.15`，网关 `10.0.2.2`）。
const OUR_IP: [u8; 4] = [10, 0, 2, 15];
const GW_IP: [u8; 4] = [10, 0, 2, 2];

// ---------------------------------------------------------------------------
// N3b — 最小 IPv4 栈（仅网卡之上的协议语义）
// ---------------------------------------------------------------------------

/// IP 帧（ICMP/UDP）专用 TX 缓冲页：与 ARP 的 [`TX_BUF_PAGE`]（页 6）错开。
const IP_TX_BUF_PAGE: u64 = 7;
/// 以太类型。
const ETH_IPV4: u16 = 0x0800;
const ETH_ARP: u16 = 0x0806;
/// IPv4 协议号。
const IP_PROTO_ICMP: u8 = 1;
const IP_PROTO_UDP: u8 = 17;
/// IPv4 默认 TTL。
const IP_TTL: u8 = 64;
/// ICMP 类型。
const ICMP_ECHO_REPLY: u8 = 0;
const ICMP_ECHO_REQ: u8 = 8;
const ICMP_UNREACH: u8 = 3;
/// 自测用 ICMP id/seq 与 UDP 端口。
const TEST_ICMP_ID: u16 = 0x4d4f;
const TEST_ICMP_SEQ: u16 = 1;
const TEST_UDP_SPORT: u16 = 0x4d4f;
const TEST_UDP_DPORT: u16 = 9999;
/// N3b 自测的有界等待（毫秒）：到点无论结果如何都打 `NET2`，不阻塞保活。
const NET2_TIMEOUT_MS: u64 = 2000;

/// 网络序（大端）读 16 位。
fn be16(a: u64) -> u16 {
    ((rd8(a) as u16) << 8) | rd8(a + 1) as u16
}

/// 网络序（大端）写 16 位。
fn put_be16(a: u64, v: u16) {
    wr8(a, (v >> 8) as u8);
    wr8(a + 1, (v & 0xff) as u8);
}

/// Internet 校验和（RFC 1071）：对 `[base, base+len)` 按 16 位字求反码和。
/// 奇数字节按高位补 0；返回可直接写入头部的值。校验一段已含校验和的区域时，
/// 结果应为 0（见 [`inet_checksum_valid`]）。
fn inet_checksum(base: u64, len: u64) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0u64;
    while i + 1 < len {
        sum += be16(base + i) as u32;
        i += 2;
    }
    if i < len {
        sum += (rd8(base + i) as u32) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// 校验一段**已含校验和字段**的区域是否合法（反码和为 0xFFFF）。
fn inet_checksum_valid(base: u64, len: u64) -> bool {
    let mut sum: u32 = 0;
    let mut i = 0u64;
    while i + 1 < len {
        sum += be16(base + i) as u32;
        i += 2;
    }
    if i < len {
        sum += (rd8(base + i) as u32) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16 == 0xffff
}

/// 把 device cfg 读出的 MAC（低字节 = 首字节）拆成网络序字节数组。
fn mac_bytes(mac: u64) -> [u8; 6] {
    let mut m = [0u8; 6];
    let mut i = 0u64;
    while i < 6 {
        m[i as usize] = ((mac >> (8 * i)) & 0xff) as u8;
        i += 1;
    }
    m
}

/// 解析出的 IPv4 报文信息；`payload_off` 是 payload 在帧内的地址。
struct Ipv4Info {
    src: [u8; 4],
    dst: [u8; 4],
    proto: u8,
    payload_off: u64,
    payload_len: u64,
}

/// 解析以太帧里的 IPv4 报文。`eth_va` 指向以太头（已跳过 12 字节 virtio 包头），
/// `eth_len` 是以太帧长度。任一校验不过返回 `None`：非 IPv4 / 版本非 4 / IHL 越界 /
/// 总长越界 / 头校验和非法 / **任何分片**（不实现重组，故 MF 与 offset≠0 一律拒）。
fn ipv4_parse(eth_va: u64, eth_len: u64) -> Option<Ipv4Info> {
    if eth_len < 14 + 20 || be16(eth_va + 12) != ETH_IPV4 {
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
    let total = be16(ip + 2) as u64;
    if total < ihl || total > ip_len {
        return None;
    }
    let frag = be16(ip + 6);
    if frag & 0x2000 != 0 || frag & 0x1fff != 0 {
        return None;
    }
    if !inet_checksum_valid(ip, ihl) {
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

/// 在 `buf_va` 写以太头 + 20 字节 IPv4 头（无选项、DF 置位），返回 **payload 起始地址**；
/// 调用方写完 payload 后再调 [`ipv4_finish`] 回填总长与头校验和。
fn ipv4_build(buf_va: u64, src_mac: u64, dst_mac: [u8; 6], proto: u8, dst_ip: [u8; 4]) -> u64 {
    let f = buf_va + VNET_HDR_LEN;
    let src = mac_bytes(src_mac);
    let mut i = 0u64;
    while i < 6 {
        wr8(f + i, dst_mac[i as usize]);
        wr8(f + 6 + i, src[i as usize]);
        i += 1;
    }
    put_be16(f + 12, ETH_IPV4);
    let ip = f + 14;
    wr8(ip, 0x45); // version 4, IHL 5
    wr8(ip + 1, 0); // DSCP/ECN
    put_be16(ip + 2, 0); // total length（finish 回填）
    put_be16(ip + 4, 0); // identification
    put_be16(ip + 6, 0x4000); // flags = DF, fragment offset = 0
    wr8(ip + 8, IP_TTL);
    wr8(ip + 9, proto);
    put_be16(ip + 10, 0); // header checksum（finish 回填）
    let mut k = 0u64;
    while k < 4 {
        wr8(ip + 12 + k, OUR_IP[k as usize]);
        wr8(ip + 16 + k, dst_ip[k as usize]);
        k += 1;
    }
    ip + 20
}

/// 回填 IPv4 总长与头校验和，返回整帧长度（含 12 字节 virtio 包头）。
fn ipv4_finish(buf_va: u64, payload_len: u64) -> u64 {
    let ip = buf_va + VNET_HDR_LEN + 14;
    put_be16(ip + 2, (20 + payload_len) as u16);
    put_be16(ip + 10, 0);
    put_be16(ip + 10, inet_checksum(ip, 20));
    VNET_HDR_LEN + 14 + 20 + payload_len
}

/// 写一条 ICMP echo（`kind` = 8 request / 0 reply）到 `ip_payload_va`，返回 ICMP 报文长度。
fn icmp_echo_write(ip_payload_va: u64, kind: u8, id: u16, seq: u16, payload: &[u8]) -> u64 {
    wr8(ip_payload_va, kind);
    wr8(ip_payload_va + 1, 0);
    put_be16(ip_payload_va + 2, 0);
    put_be16(ip_payload_va + 4, id);
    put_be16(ip_payload_va + 6, seq);
    let mut i = 0u64;
    while i < payload.len() as u64 {
        wr8(ip_payload_va + 8 + i, payload[i as usize]);
        i += 1;
    }
    let len = 8 + payload.len() as u64;
    put_be16(ip_payload_va + 2, inet_checksum(ip_payload_va, len));
    len
}

/// 写一条 UDP 报头到 `ip_payload_va`（校验和置 0 = 不校验，IPv4 允许），返回 UDP 长度。
fn udp_write(ip_payload_va: u64, sport: u16, dport: u16, payload: &[u8]) -> u64 {
    let len = 8 + payload.len() as u64;
    put_be16(ip_payload_va, sport);
    put_be16(ip_payload_va + 2, dport);
    put_be16(ip_payload_va + 4, len as u16);
    put_be16(ip_payload_va + 6, 0);
    let mut i = 0u64;
    while i < payload.len() as u64 {
        wr8(ip_payload_va + 8 + i, payload[i as usize]);
        i += 1;
    }
    len
}

/// 收到的是否为对我们 echo request（`id`/`seq`）的 echo reply；是则返回发送方 IP。
fn icmp_echo_reply_from(eth_va: u64, eth_len: u64, id: u16, seq: u16) -> Option<[u8; 4]> {
    let info = ipv4_parse(eth_va, eth_len)?;
    if info.proto != IP_PROTO_ICMP || info.payload_len < 8 {
        return None;
    }
    let icmp = info.payload_off;
    if rd8(icmp) != ICMP_ECHO_REPLY || rd8(icmp + 1) != 0 {
        return None;
    }
    if be16(icmp + 4) != id || be16(icmp + 6) != seq {
        return None;
    }
    if !inet_checksum_valid(icmp, info.payload_len) {
        return None;
    }
    Some(info.src)
}

/// 收到的是否为引用我们 UDP 报文的 ICMP 目的不可达（type 3 code 3）。
fn icmp_unreachable_for_udp(eth_va: u64, eth_len: u64) -> bool {
    let info = match ipv4_parse(eth_va, eth_len) {
        Some(v) => v,
        None => return false,
    };
    if info.proto != IP_PROTO_ICMP || info.payload_len < 8 + 20 {
        return false;
    }
    let icmp = info.payload_off;
    if rd8(icmp) != ICMP_UNREACH || rd8(icmp + 1) != 3 {
        return false;
    }
    // 内嵌的原始 IP 头：确认源是我们、协议是 UDP。
    let inner = icmp + 8;
    if rd8(inner) >> 4 != 4 || (rd8(inner) & 0x0f) < 5 {
        return false;
    }
    if rd8(inner + 9) != IP_PROTO_UDP {
        return false;
    }
    [
        rd8(inner + 12),
        rd8(inner + 13),
        rd8(inner + 14),
        rd8(inner + 15),
    ] == OUR_IP
}

/// 若 `eth_va` 是发往 `OUR_IP` 的 ICMP echo request，则在 `tx_va` 构造 echo reply，
/// 返回整帧长度（含 12 字节 virtio 包头）；否则 `None`。目的 MAC 取请求的源 MAC。
fn icmp_echo_reply_build(eth_va: u64, eth_len: u64, our_mac: u64, tx_va: u64) -> Option<u64> {
    let info = ipv4_parse(eth_va, eth_len)?;
    if info.proto != IP_PROTO_ICMP || info.payload_len < 8 || info.dst != OUR_IP {
        return None;
    }
    let icmp = info.payload_off;
    if rd8(icmp) != ICMP_ECHO_REQ || rd8(icmp + 1) != 0 {
        return None;
    }
    if !inet_checksum_valid(icmp, info.payload_len) {
        return None;
    }
    let dst_mac = [
        rd8(eth_va + 6),
        rd8(eth_va + 7),
        rd8(eth_va + 8),
        rd8(eth_va + 9),
        rd8(eth_va + 10),
        rd8(eth_va + 11),
    ];
    let id = be16(icmp + 4);
    let seq = be16(icmp + 6);
    let pay_len = info.payload_len - 8;
    let payload = ipv4_build(tx_va, our_mac, dst_mac, IP_PROTO_ICMP, info.src);
    wr8(payload, ICMP_ECHO_REPLY);
    wr8(payload + 1, 0);
    put_be16(payload + 2, 0);
    put_be16(payload + 4, id);
    put_be16(payload + 6, seq);
    let mut i = 0u64;
    while i < pay_len {
        wr8(payload + 8 + i, rd8(icmp + 8 + i));
        i += 1;
    }
    put_be16(payload + 2, inet_checksum(payload, 8 + pay_len));
    Some(ipv4_finish(tx_va, 8 + pay_len))
}

/// 无对端自证「收 echo request → 回 echo reply」：合成一条发往 `OUR_IP` 的 echo request，
/// 交给 [`icmp_echo_reply_build`] 生成回包，再解析回包断言字段与双校验和。
fn icmp_responder_selftest(our_mac: u64, tx_va: u64) -> bool {
    let mut req = [0u8; 64];
    let rva = req.as_mut_ptr() as u64;
    let our = mac_bytes(our_mac);
    let peer = [0x02u8, 0x00, 0x00, 0x00, 0x00, 0x01];
    let mut i = 0u64;
    while i < 6 {
        wr8(rva + i, our[i as usize]);
        wr8(rva + 6 + i, peer[i as usize]);
        i += 1;
    }
    put_be16(rva + 12, ETH_IPV4);
    let ip = rva + 14;
    wr8(ip, 0x45);
    wr8(ip + 1, 0);
    put_be16(ip + 4, 0);
    put_be16(ip + 6, 0x4000);
    wr8(ip + 8, IP_TTL);
    wr8(ip + 9, IP_PROTO_ICMP);
    let mut k = 0u64;
    while k < 4 {
        wr8(ip + 12 + k, GW_IP[k as usize]); // 模拟对端 = 网关
        wr8(ip + 16 + k, OUR_IP[k as usize]);
        k += 1;
    }
    let icmp_len = icmp_echo_write(ip + 20, ICMP_ECHO_REQ, TEST_ICMP_ID, TEST_ICMP_SEQ, b"loop");
    put_be16(ip + 2, (20 + icmp_len) as u16);
    put_be16(ip + 10, 0);
    put_be16(ip + 10, inet_checksum(ip, 20));
    let req_len = 14 + 20 + icmp_len;

    let reply_len = match icmp_echo_reply_build(rva, req_len, our_mac, tx_va) {
        Some(l) => l,
        None => return false,
    };
    // 回包（跳过 12 字节 virtio 包头）再解析校验一次。
    let info = match ipv4_parse(tx_va + VNET_HDR_LEN, reply_len - VNET_HDR_LEN) {
        Some(v) => v,
        None => return false,
    };
    if info.proto != IP_PROTO_ICMP || info.src != OUR_IP || info.dst != GW_IP {
        return false;
    }
    let icmp = info.payload_off;
    if rd8(icmp) != ICMP_ECHO_REPLY || rd8(icmp + 1) != 0 {
        return false;
    }
    if be16(icmp + 4) != TEST_ICMP_ID || be16(icmp + 6) != TEST_ICMP_SEQ {
        return false;
    }
    inet_checksum_valid(icmp, info.payload_len)
}

/// 发一帧并等到设备消费完（TX used 环前进），保证 TX 缓冲可安全复用。
fn tx_send(caps: &virtio::Caps, tx: &Vq, buf_pa: u64, len: u64) {
    let before = tx.used_idx();
    tx.set_desc(0, buf_pa, len as u32, 0, 0);
    tx.avail_push(0);
    tx.kick(caps, 1);
    let mut spins = 0u32;
    while tx.used_idx() == before && spins < 1_000_000 {
        spins += 1;
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";
fn put_byte_hex(b: u8) {
    let d = [HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]];
    print(unsafe { core::str::from_utf8_unchecked(&d) });
}
/// 按 `aa:bb:cc:dd:ee:ff` 打印 MAC（`mac` 低字节 = 首字节，与 device cfg 读出的一致）。
fn print_mac(mac: u64) {
    let mut i = 0;
    while i < 6 {
        if i > 0 {
            print(":");
        }
        put_byte_hex(((mac >> (8 * i)) & 0xff) as u8);
        i += 1;
    }
}

/// 在 TX 缓冲里拼一个**广播 ARP 请求**（问 `GW_IP` 的 MAC），返回整帧长度（含 12 字节包头）。
fn arp_build(buf_va: u64, our_mac: u64) -> u64 {
    let mut mac = [0u8; 6];
    let mut i = 0u64;
    while i < 6 {
        mac[i as usize] = ((our_mac >> (8 * i)) & 0xff) as u8;
        i += 1;
    }
    // 包头 12 字节清零（无 offload：flags/gso 全 0）。
    let mut k = 0u64;
    while k < VNET_HDR_LEN {
        wr8(buf_va + k, 0);
        k += 1;
    }
    // 以太头：目的 = 广播，源 = 本机 MAC，类型 = 0x0806 (ARP)。
    let f = buf_va + VNET_HDR_LEN;
    let mut j = 0u64;
    while j < 6 {
        wr8(f + j, 0xff);
        wr8(f + 6 + j, mac[j as usize]);
        j += 1;
    }
    put_be16(f + 12, ETH_ARP);
    // ARP 报文（28 字节）：Ethernet/IPv4，oper=1 (request)，sha/spa = 本机，tpa = 网关。
    let a = f + 14;
    let arp: [u8; 28] = [
        0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x01, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
        OUR_IP[0], OUR_IP[1], OUR_IP[2], OUR_IP[3], 0, 0, 0, 0, 0, 0, GW_IP[0], GW_IP[1], GW_IP[2],
        GW_IP[3],
    ];
    let mut m = 0u64;
    while m < 28 {
        wr8(a + m, arp[m as usize]);
        m += 1;
    }
    VNET_HDR_LEN + 42
}

/// 若帧是网关对 `GW_IP` 的 ARP **应答**，返回其发送方 MAC（网络序）。
///
/// 不写死网关 MAC —— 从应答的 `sha` 字段取，供后续 IP 帧做单播目的地址。
fn gw_arp_reply_mac(buf_va: u64, len: u64) -> Option<[u8; 6]> {
    if len < VNET_HDR_LEN + 42 {
        return None;
    }
    let eth = buf_va + VNET_HDR_LEN;
    if be16(eth + 12) != ETH_ARP {
        return None;
    }
    let arp = eth + 14;
    if be16(arp + 6) != 0x0002 {
        return None; // oper = reply
    }
    if [rd8(arp + 14), rd8(arp + 15), rd8(arp + 16), rd8(arp + 17)] != GW_IP {
        return None;
    }
    Some([
        rd8(arp + 8),
        rd8(arp + 9),
        rd8(arp + 10),
        rd8(arp + 11),
        rd8(arp + 12),
        rd8(arp + 13),
    ])
}

/// 永不返回的保活循环（无设备 / 初始化失败时用）。
fn idle() -> ! {
    loop {
        sys_sleep(500);
    }
}

/// 域 16 — net_srv：virtio-net modern 驱动（N2）。
pub fn run() {
    let g = DeviceGrant::load();
    if !g.is_valid() {
        println("net: no device grant (no virtio-net), idle");
        idle();
    }
    if g.dma_bytes < DMA_PAGES * PAGE {
        println("net: device grant DMA too small, aborting");
        idle();
    }

    let caps =
        match virtio::discover_caps(g.bar_vaddr, |off| sys_device_config_read(off as u64) as u32) {
            Some(c) => c,
            None => {
                println("net: no virtio PCI capabilities, aborting");
                idle();
            }
        };
    print("net: virtio caps common=0x");
    print_hex(caps.common);
    print(" notify=0x");
    print_hex(caps.notify);
    print(" device=0x");
    print_hex(caps.device);
    print(" mult=");
    print_u64(caps.notify_mult as u64);
    println("");

    // 环的初始状态：清零 RX/TX ring 页（分配器不保证新帧为 0，`avail.idx` 必须是 0）。
    virtio::zero_page(g.dma_vaddr + RX_RING_PAGE * PAGE);
    virtio::zero_page(g.dma_vaddr + TX_RING_PAGE * PAGE);

    // 复位 + 特性协商：只接 `VERSION_1`（传输层保证）+ `NET_F_MAC`。
    match virtio::negotiate(&caps, |dev0| dev0 & FEAT_NET_MAC) {
        Ok(_) => {}
        Err(virtio::NegError::NoVersion1) => {
            println("net: device lacks VIRTIO_F_VERSION_1 (legacy), aborting");
            caps.failed();
            idle();
        }
        Err(virtio::NegError::FeaturesRejected) => {
            println("net: device rejected FEATURES_OK, aborting");
            caps.failed();
            idle();
        }
    }

    // 读设备配置：队列数 + MAC。
    let num_queues = caps.num_queues();
    let mac_lo = rd32(caps.device);
    let mac_hi = rd16(caps.device + 4);
    let mac = ((mac_hi as u64) << 32) | mac_lo as u64;

    // MSI-X 判定：内核给了向量段 + 表窗口（N2b：表在 BAR1，内核已另映射给本域）才走中断。
    let want_irq = g.msix_vector_base != 0 && g.msix_table_vaddr != 0 && g.msix_vector_count >= 2;
    let irq_base = g.msix_vector_base as u16;
    let rx_msix = if want_irq { 0 } else { virtio::NO_VECTOR };
    let tx_msix = if want_irq { 1 } else { virtio::NO_VECTOR };
    // 不做配置变更中断（本驱动不需要）。
    caps.set_config_msix(virtio::NO_VECTOR);

    // 建 RX(0) / TX(1) 两个 virtqueue（各自绑一条 MSI-X 表项）。
    let (rx_size, rx_noff) = virtio::setup_queue(
        &caps,
        0,
        Q_SIZE,
        rx_msix,
        g.dma_paddr + RX_RING_PAGE * PAGE,
        g.dma_paddr + RX_RING_PAGE * PAGE + virtio::OFF_AVAIL,
        g.dma_paddr + RX_RING_PAGE * PAGE + virtio::OFF_USED,
    );
    let (tx_size, tx_noff) = virtio::setup_queue(
        &caps,
        1,
        Q_SIZE,
        tx_msix,
        g.dma_paddr + TX_RING_PAGE * PAGE,
        g.dma_paddr + TX_RING_PAGE * PAGE + virtio::OFF_AVAIL,
        g.dma_paddr + TX_RING_PAGE * PAGE + virtio::OFF_USED,
    );
    if rx_size == 0 || tx_size == 0 {
        println("net: virtqueue setup failed, aborting");
        caps.failed();
        idle();
    }
    let rx = Vq::new(g.dma_vaddr, RX_RING_PAGE, rx_size, rx_noff);
    let tx = Vq::new(g.dma_vaddr, TX_RING_PAGE, tx_size, tx_noff);

    print("net: virtio-net up MAC=");
    print_hex(mac);
    print(" num_queues=");
    print_u64(num_queues as u64);
    print(" rx=");
    print_u64(rx_size as u64);
    print(" tx=");
    print_u64(tx_size as u64);
    println("");

    // MSI-X：写表项（表在内核另映射的窗口里）→ 注册向量 → 请内核打开 MSI-X。
    // 任一环节不成就退回轮询（等待期用 sys_sleep）。
    let mut irq_vectors: u64 = 0;
    let mut irq_mask: u64 = 0;
    if want_irq {
        msix::write_table_entry(&g, 0, irq_base as u64);
        msix::write_table_entry(&g, 1, (irq_base + 1) as u64);
        let r0 = sys_register_irq(irq_base as u64);
        let r1 = sys_register_irq((irq_base + 1) as u64);
        if sys_msix_enable() == 1 && r0 == 1 && r1 == 1 {
            irq_vectors = 2;
            // 掩码位 `i` ↔ 向量 `MSI_VECTOR_BASE + i`：本设备向量段不从段首开始，整体左移。
            let shift = (irq_base as u64).wrapping_sub(MSI_VECTOR_BASE);
            irq_mask = ((1u64 << irq_vectors) - 1) << shift;
            print("net: MSI-X enabled vectors=0x");
            print_hex(irq_base as u64);
            print("..0x");
            print_hex((irq_base + 1) as u64);
            println("");
        } else {
            println("net: MSI-X enable/register failed, polling");
        }
    } else {
        println("net: no MSI-X (kernel gave no vectors/table window), polling");
    }

    // 投满 RX 缓冲（每个描述符一格缓冲；设备收包时写进对应格）。批量投完再敲一次门铃。
    let mut i = 0u16;
    while i < rx_size {
        let buf_pa = g.dma_paddr + RX_BUF_PAGE * PAGE + (i as u64) * BUF_SZ;
        rx.set_desc(i, buf_pa, BUF_SZ as u32, virtio::DESC_F_WRITE, 0);
        rx.avail_push(i);
        i += 1;
    }
    rx.kick(&caps, 0);

    // DRIVER_OK：驱动就绪，设备开始收包。
    caps.driver_ok();
    println("net: DRIVER_OK, RX buffers posted");

    // N3 自测：发一个广播 ARP 请求问网关 MAC（QEMU user-net 会应答）——
    // 应答回来即证明「TX 通路 + RX 通路 + 中断/轮询」整条链路通。
    let tx_buf_va = g.dma_vaddr + TX_BUF_PAGE * PAGE;
    let tx_buf_pa = g.dma_paddr + TX_BUF_PAGE * PAGE;
    // N3b：IP 帧（ICMP/UDP）用独立的一页 TX 缓冲，与 ARP 的页 6 错开、串行复用。
    let ip_buf_va = g.dma_vaddr + IP_TX_BUF_PAGE * PAGE;
    let ip_buf_pa = g.dma_paddr + IP_TX_BUF_PAGE * PAGE;

    let flen = arp_build(tx_buf_va, mac);
    tx.set_desc(0, tx_buf_pa, flen as u32, 0, 0);
    tx.avail_push(0);
    tx.kick(&caps, 1);
    print("net: ARP request sent for 10.0.2.2 (gateway), frame len=");
    print_u64(flen);
    println("");

    // N3b：无对端自证「收 echo request 回 echo reply」路径（合成请求 → responder → 解析回包）。
    let responder_ok = icmp_responder_selftest(mac, ip_buf_va);
    if responder_ok {
        println(
            "net: ipv4/icmp echo-reply path OK (synthetic request answered, checksums verified)",
        );
    } else {
        println("net: ipv4/icmp echo-reply path FAILED (synthetic request not answered)");
    }

    // 收帧：有中断走中断（快路径 poll + 阻塞 wait，超时回落重扫），否则轮询。
    let mut last_used: u16 = 0;
    let mut rx_frames: u64 = 0;
    let mut irq_hits: u64 = 0;
    // N3/N3b 自测状态。
    let mut arp_ok = false;
    let mut gw_mac = [0u8; 6];
    let mut icmp_sent = false;
    let mut icmp_ok = false;
    let mut udp_sent = false;
    let mut udp_ok = false;
    let mut net2_done = false;
    let mut probe_ms: u64 = 0;
    loop {
        let used_idx = rx.used_idx();
        let mut drained = 0u32;
        while last_used != used_idx {
            let slot = (last_used as u64) % (rx_size as u64);
            let (id, len) = rx.used_elem(slot as u16);
            rx_frames += 1;
            drained += 1;
            let len = len as u64;
            let buf_va = g.dma_vaddr + RX_BUF_PAGE * PAGE + (id as u64) * BUF_SZ;
            let eth_va = buf_va + VNET_HDR_LEN;
            let eth_len = len.saturating_sub(VNET_HDR_LEN);
            // NET1：命中网关 ARP 应答即记下其 MAC 并打自测标记。
            if !arp_ok {
                if let Some(m) = gw_arp_reply_mac(buf_va, len) {
                    arp_ok = true;
                    gw_mac = m;
                    print("NET1 virtio-net up, MAC=");
                    print_mac(mac);
                    println(", ARP reply OK");
                }
            }
            // NET2：收到对我们的 ICMP echo 应答 / 引用我们 UDP 的 ICMP 端口不可达。
            if icmp_sent && !icmp_ok {
                if let Some(src) =
                    icmp_echo_reply_from(eth_va, eth_len, TEST_ICMP_ID, TEST_ICMP_SEQ)
                {
                    if src == GW_IP {
                        icmp_ok = true;
                        println("net: icmp echo reply from 10.0.2.2");
                    }
                }
            }
            if udp_sent && !udp_ok && icmp_unreachable_for_udp(eth_va, eth_len) {
                udp_ok = true;
                println("net: udp 10.0.2.2:9999 -> icmp port unreachable");
            }
            // 收到发往本机的 echo request → 回 echo reply（真实入站路径）。
            if let Some(rlen) = icmp_echo_reply_build(eth_va, eth_len, mac, ip_buf_va) {
                tx_send(&caps, &tx, ip_buf_pa, rlen);
                println("net: icmp echo request answered");
            }
            // 把同一个描述符补投回 avail 环，缓冲可被复用。
            rx.avail_push(id);
            last_used = last_used.wrapping_add(1);
        }
        if drained > 0 {
            rx.kick(&caps, 0);
            print("net: rx frames=");
            print_u64(rx_frames);
            print(" irq_hits=");
            print_u64(irq_hits);
            println("");
        }
        // 拿到网关 MAC 后各发一次 ICMP echo request 与 UDP（串行复用页 7）。
        if arp_ok && !icmp_sent {
            let pay = ipv4_build(ip_buf_va, mac, gw_mac, IP_PROTO_ICMP, GW_IP);
            let ilen = icmp_echo_write(
                pay,
                ICMP_ECHO_REQ,
                TEST_ICMP_ID,
                TEST_ICMP_SEQ,
                b"MORION-N3B",
            );
            let ifl = ipv4_finish(ip_buf_va, ilen);
            tx_send(&caps, &tx, ip_buf_pa, ifl);
            icmp_sent = true;
            println("net: icmp echo request sent to 10.0.2.2");

            let upay = ipv4_build(ip_buf_va, mac, gw_mac, IP_PROTO_UDP, GW_IP);
            let ulen = udp_write(upay, TEST_UDP_SPORT, TEST_UDP_DPORT, b"MORION-UDP");
            let ufl = ipv4_finish(ip_buf_va, ulen);
            tx_send(&caps, &tx, ip_buf_pa, ufl);
            udp_sent = true;
            println("net: udp sent to 10.0.2.2:9999");
        }
        // NET2 判据：ICMP+UDP 都有结果，或到点收手（绝不阻塞后续保活）。
        if !net2_done && icmp_sent && ((icmp_ok && udp_ok) || probe_ms >= NET2_TIMEOUT_MS) {
            print("NET2 ipv4/icmp ");
            print(if responder_ok { "OK" } else { "FAILED" });
            print(", echo reply from ");
            print(if icmp_ok { "10.0.2.2" } else { "timeout" });
            print(", udp TX 10.0.2.2:9999 -> ");
            print(if udp_ok {
                "icmp unreachable"
            } else {
                "timeout"
            });
            print(", echo-reply path ");
            println(if responder_ok { "OK" } else { "FAILED" });
            net2_done = true;
        }
        if irq_vectors != 0 {
            // 快路径 poll 命中就不睡；否则阻塞等下一次中断，超时回落重扫。
            let hit = sys_irq_poll(irq_mask) != 0 || sys_irq_wait(irq_mask, IRQ_WAIT_MS) != 0;
            if hit {
                irq_hits += 1;
            } else if !net2_done {
                probe_ms += IRQ_WAIT_MS;
            }
        } else {
            sys_sleep(20);
            if !net2_done {
                probe_ms += 20;
            }
        }
    }
}
