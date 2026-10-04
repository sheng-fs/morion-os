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
//! **N3c**：DHCP 客户端 —— 广播 DHCPDISCOVER → OFFER → REQUEST → ACK，取得 IP/掩码/网关/DNS，
//! 替换写死的 `10.0.2.15`/`10.0.2.2`（取不到则回落默认值）。自测标记 `NET3 dhcp …`。
//! **N4**：最小 TCP —— 头构造/解析（校验和含 12 字节伪首部）、三次握手（SYN → SYN-ACK → ACK）
//! 与最小 PSH/ACK 数据段；先向 slirp 网关试发 SYN 取真实回应，无稳定对端则用确定性自证
//! （合成 SYN-ACK → 解析断言 → 生成 ACK/数据 → 复校验和）。自测标记 `NET4 tcp …`。
//! **ARP 老化**：网关 MAC 进带时间戳的缓存（TTL 约 30s），过期即重新广播 ARP 刷新。
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
/// QEMU user-net 默认地址（DHCP 未取得租约时的回落值：guest `10.0.2.15`、网关 `10.0.2.2`）。
const DEFAULT_OUR_IP: [u8; 4] = [10, 0, 2, 15];
const DEFAULT_GW_IP: [u8; 4] = [10, 0, 2, 2];
/// 运行期本机 / 网关地址：N3c 的 DHCP 取得租约后写入，缺省为上面的回落值。
static mut OUR_IP: [u8; 4] = DEFAULT_OUR_IP;
static mut GW_IP: [u8; 4] = DEFAULT_GW_IP;

/// 读运行期本机地址（按值拷贝，避免 `static_mut_refs`）。
fn our_ip() -> [u8; 4] {
    unsafe { OUR_IP }
}
/// 读运行期网关地址。
fn gw_ip() -> [u8; 4] {
    unsafe { GW_IP }
}

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
const IP_PROTO_TCP: u8 = 6;
const IP_PROTO_UDP: u8 = 17;
/// IPv4 默认 TTL。
const IP_TTL: u8 = 64;
/// ICMP 类型。
const ICMP_ECHO_REPLY: u8 = 0;
const ICMP_ECHO_REQ: u8 = 8;
const ICMP_UNREACH: u8 = 3;
/// TCP 标志位（本驱动只用到这四个）。
const TCP_SYN: u8 = 0x02;
const TCP_RST: u8 = 0x04;
const TCP_PSH: u8 = 0x08;
const TCP_ACK: u8 = 0x10;
/// 自测用 ICMP id/seq 与 UDP 端口。
const TEST_ICMP_ID: u16 = 0x4d4f;
const TEST_ICMP_SEQ: u16 = 1;
const TEST_UDP_SPORT: u16 = 0x4d4f;
const TEST_UDP_DPORT: u16 = 9999;
/// N4 自测用 TCP 端口（目的取 slirp 上大概率无监听的端口）与客户端初始序号（ISN）。
const TEST_TCP_SPORT: u16 = 0x4d50;
const TEST_TCP_DPORT: u16 = 12345;
const TEST_TCP_ISN: u32 = 0x4d4f_0004;
/// TCP 通告窗口（任意值，取常见 29200）。
const TCP_WINDOW: u16 = 0x7210;
/// N4 对真实对端发起握手后的有界等待（毫秒）。
const TCP_PEER_TIMEOUT_MS: u64 = 1000;
/// ARP 缓存 TTL（约 30 秒；无墙钟 syscall，用 RX 循环累计毫秒近似）。
const ARP_CACHE_TTL_MS: u64 = 30_000;
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

/// 网络序（大端）写 32 位。
fn put_be32(a: u64, v: u32) {
    put_be16(a, (v >> 16) as u16);
    put_be16(a + 2, (v & 0xffff) as u16);
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

/// 把 `[base, base+len)` 按 16 位字累加进 `sum`（供带伪首部的传输层校验和复用）。
fn csum_acc(base: u64, len: u64, sum: u32) -> u32 {
    let mut s = sum;
    let mut i = 0u64;
    while i + 1 < len {
        s += be16(base + i) as u32;
        i += 2;
    }
    if i < len {
        s += (rd8(base + i) as u32) << 8;
    }
    s
}

/// 反码和折叠到 16 位（不取反）。
fn csum_fold(mut s: u32) -> u16 {
    while (s >> 16) != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    s as u16
}

/// 伪首部（12 字节：源 IP / 目的 IP / 0 / 协议 / TCP 长度）+ TCP 段的 16 位反码和
/// （折叠后、未取反）。TCP 校验和 = `!` 它；校验时该和应为 `0xffff`。
fn tcp_pseudo_sum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp: u64, seg_len: u64) -> u16 {
    let mut ph = [0u8; 12];
    let mut i = 0u64;
    while i < 4 {
        ph[i as usize] = src_ip[i as usize];
        ph[4 + i as usize] = dst_ip[i as usize];
        i += 1;
    }
    ph[8] = 0;
    ph[9] = IP_PROTO_TCP;
    ph[10] = (seg_len >> 8) as u8;
    ph[11] = seg_len as u8;
    let pva = ph.as_ptr() as u64;
    csum_fold(csum_acc(tcp, seg_len, csum_acc(pva, 12, 0)))
}

/// 计算写入 TCP 头的校验和（调用前须已把校验和字段置 0）。
fn tcp_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp: u64, seg_len: u64) -> u16 {
    !tcp_pseudo_sum(src_ip, dst_ip, tcp, seg_len)
}

/// 校验一段**已含校验和字段**的 TCP 段（伪首部 + 段之和折叠应为 `0xffff`）。
fn tcp_checksum_valid(src_ip: [u8; 4], dst_ip: [u8; 4], tcp: u64, seg_len: u64) -> bool {
    tcp_pseudo_sum(src_ip, dst_ip, tcp, seg_len) == 0xffff
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
    ipv4_build_ex(buf_va, src_mac, dst_mac, proto, our_ip(), dst_ip)
}

/// 同 [`ipv4_build`]，但显式给出源 IP（DHCP 在拿到租约前用 `0.0.0.0`）。
fn ipv4_build_ex(
    buf_va: u64,
    src_mac: u64,
    dst_mac: [u8; 6],
    proto: u8,
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
) -> u64 {
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
        wr8(ip + 12 + k, src_ip[k as usize]);
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

// ---------------------------------------------------------------------------
// N4 — 最小 TCP（头构造/解析 + 伪首部校验和 + 握手/数据段语义）
// ---------------------------------------------------------------------------

/// 解析出的 TCP 段信息；`payload_off` 是数据在帧内的地址。
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

/// 解析 IPv4 报文里的 TCP 段（含 12 字节伪首部校验和校验、data offset 越界拒绝）。
fn tcp_parse(info: &Ipv4Info) -> Option<TcpSeg> {
    if info.proto != IP_PROTO_TCP || info.payload_len < 20 {
        return None;
    }
    let tcp = info.payload_off;
    let doff = (rd8(tcp + 12) >> 4) as u64 * 4;
    if doff < 20 {
        return None;
    }
    if doff > info.payload_len {
        return None;
    }
    if !tcp_checksum_valid(info.src, info.dst, tcp, info.payload_len) {
        return None;
    }
    Some(TcpSeg {
        sport: be16(tcp),
        dport: be16(tcp + 2),
        seq: be32(tcp + 4),
        ack: be32(tcp + 8),
        flags: rd8(tcp + 13),
        window: be16(tcp + 14),
        payload_off: tcp + doff,
        payload_len: info.payload_len - doff,
    })
}

/// 便捷：直接解析以太帧（`eth_va` 指向以太头，已跳过 12 字节 virtio 包头）。
fn tcp_parse_eth(eth_va: u64, eth_len: u64) -> Option<TcpSeg> {
    let info = ipv4_parse(eth_va, eth_len)?;
    tcp_parse(&info)
}

/// 组装一个 TCP 段（以太 + IPv4 + TCP，无选项），返回整帧长度（含 12 字节 virtio 包头）。
#[allow(clippy::too_many_arguments)]
fn tcp_build(
    buf_va: u64,
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
    let tcp = ipv4_build_ex(buf_va, src_mac, dst_mac, IP_PROTO_TCP, src_ip, dst_ip);
    put_be16(tcp, sport);
    put_be16(tcp + 2, dport);
    put_be32(tcp + 4, seq);
    put_be32(tcp + 8, ack);
    wr8(tcp + 12, 5 << 4); // data offset = 5（20 字节头），reserved/NS = 0
    wr8(tcp + 13, flags);
    put_be16(tcp + 14, window);
    put_be16(tcp + 16, 0); // checksum（回填）
    put_be16(tcp + 18, 0); // urgent pointer
    let mut i = 0u64;
    while i < payload.len() as u64 {
        wr8(tcp + 20 + i, payload[i as usize]);
        i += 1;
    }
    let seg_len = 20 + payload.len() as u64;
    put_be16(tcp + 16, tcp_checksum(src_ip, dst_ip, tcp, seg_len));
    ipv4_finish(buf_va, seg_len)
}

/// 无对端自证 TCP：合成对端 SYN-ACK → 解析断言字段与校验和 → 生成握手 ACK → 生成最小
/// PSH/ACK 数据段 → 解析断言载荷逐字节一致；最后篡改一个载荷字节，确认校验和拦截。
fn tcp_selftest(our_mac: u64, tx_va: u64) -> bool {
    let peer_mac = [0x02u8, 0x00, 0x00, 0x00, 0x00, 0x02];
    let us = our_ip();
    let peer = gw_ip();
    let isn = TEST_TCP_ISN;
    let peer_isn: u32 = 0x1234_5678;
    let eth = tx_va + VNET_HDR_LEN;

    // 1) 合成对端 SYN-ACK（peer → us）：SYN|ACK，ack 应等于 ISN+1。
    let fl = tcp_build(
        tx_va,
        our_mac,
        peer_mac,
        peer,
        us,
        TEST_TCP_DPORT,
        TEST_TCP_SPORT,
        peer_isn,
        isn.wrapping_add(1),
        TCP_SYN | TCP_ACK,
        TCP_WINDOW,
        b"",
    );
    let synack = match tcp_parse_eth(eth, fl - VNET_HDR_LEN) {
        Some(s) => s,
        None => return false,
    };
    if synack.flags != (TCP_SYN | TCP_ACK) {
        return false;
    }
    if synack.seq != peer_isn || synack.ack != isn.wrapping_add(1) {
        return false;
    }
    let peer_next = synack.seq.wrapping_add(1);
    let our_next = isn.wrapping_add(1);

    // 2) 握手第三个 ACK（us → peer）。
    let fl = tcp_build(
        tx_va,
        our_mac,
        peer_mac,
        us,
        peer,
        TEST_TCP_SPORT,
        TEST_TCP_DPORT,
        our_next,
        peer_next,
        TCP_ACK,
        TCP_WINDOW,
        b"",
    );
    match tcp_parse_eth(eth, fl - VNET_HDR_LEN) {
        Some(s) => {
            if s.flags != TCP_ACK || s.seq != our_next || s.ack != peer_next {
                return false;
            }
        }
        None => return false,
    }

    // 3) 最小数据段（PSH|ACK），载荷逐字节核对。
    let data = b"MORION-N4";
    let fl = tcp_build(
        tx_va,
        our_mac,
        peer_mac,
        us,
        peer,
        TEST_TCP_SPORT,
        TEST_TCP_DPORT,
        our_next,
        peer_next,
        TCP_PSH | TCP_ACK,
        TCP_WINDOW,
        data,
    );
    let seg = match tcp_parse_eth(eth, fl - VNET_HDR_LEN) {
        Some(s) => s,
        None => return false,
    };
    if seg.flags != (TCP_PSH | TCP_ACK) || seg.payload_len != data.len() as u64 {
        return false;
    }
    let mut i = 0u64;
    while i < data.len() as u64 {
        if rd8(seg.payload_off + i) != data[i as usize] {
            return false;
        }
        i += 1;
    }

    // 4) 篡改一个载荷字节 → 伪首部 + 段校验和应拦截（证明校验和真的覆盖了段）。
    wr8(
        seg.payload_off + data.len() as u64 - 1,
        data[data.len() - 1] ^ 0xff,
    );
    tcp_parse_eth(eth, fl - VNET_HDR_LEN).is_none()
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
    ] == our_ip()
}

/// 若 `eth_va` 是发往 `OUR_IP` 的 ICMP echo request，则在 `tx_va` 构造 echo reply，
/// 返回整帧长度（含 12 字节 virtio 包头）；否则 `None`。目的 MAC 取请求的源 MAC。
fn icmp_echo_reply_build(eth_va: u64, eth_len: u64, our_mac: u64, tx_va: u64) -> Option<u64> {
    let info = ipv4_parse(eth_va, eth_len)?;
    if info.proto != IP_PROTO_ICMP || info.payload_len < 8 || info.dst != our_ip() {
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
    let gw = gw_ip();
    let us = our_ip();
    let mut k = 0u64;
    while k < 4 {
        wr8(ip + 12 + k, gw[k as usize]); // 模拟对端 = 网关
        wr8(ip + 16 + k, us[k as usize]);
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
    if info.proto != IP_PROTO_ICMP || info.src != our_ip() || info.dst != gw_ip() {
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

/// 收到的一帧: 在 RX DMA 缓冲里的坐标（已剥掉 virtio-net 头）。用完须 `recycle`。
struct RxFrame {
    /// 描述符号（补投回 avail 环用）。
    id: u16,
    /// RX 缓冲起始（含 virtio-net 头），供按需再解析头部的调用方。
    buf_va: u64,
    /// 完整长度（含 virtio-net 头）。
    raw_len: u64,
    /// 以太帧起始（去头之后）。
    eth_va: u64,
    /// 以太帧长度。
    len: u64,
}

/// **帧级网卡抽象**（N5）：只收发以太帧 + 暴露 MAC，不含任何 IP/TCP 语义。
///
/// 把驱动细节（virtqueue / virtio-net 头 / RX 补投 / MSI-X 等待）收进这里，协议栈只经
/// [`Nic::send`] / [`Nic::poll_rx`] / [`Nic::recycle`] / [`Nic::wait`] 交互 —— 为 N6 把协议栈
/// 抽到独立服务（`netstack_srv`）铺路。**行为与重构前逐字一致**（纯等价重构）。
struct Nic {
    caps: virtio::Caps,
    rx: Vq,
    tx: Vq,
    rx_size: u16,
    dma_vaddr: u64,
    dma_paddr: u64,
    irq_mask: u64,
    irq_vectors: u64,
    /// RX used 环的消费游标（跨 DHCP 与主循环共用）。
    last_used: u16,
}

impl Nic {
    /// 建驱动态：投满 RX 缓冲 → `DRIVER_OK`（设备开始收包）。行为与 N2 一致。
    #[allow(clippy::too_many_arguments)]
    fn new(
        caps: virtio::Caps,
        rx: Vq,
        tx: Vq,
        rx_size: u16,
        dma_vaddr: u64,
        dma_paddr: u64,
        irq_mask: u64,
        irq_vectors: u64,
    ) -> Nic {
        let mut nic = Nic {
            caps,
            rx,
            tx,
            rx_size,
            dma_vaddr,
            dma_paddr,
            irq_mask,
            irq_vectors,
            last_used: 0,
        };
        nic.post_rx_buffers();
        nic.caps.driver_ok();
        nic
    }

    /// 投满 RX 缓冲（每个描述符一格缓冲；设备收包时写进对应格）。批量投完再敲一次门铃。
    fn post_rx_buffers(&mut self) {
        let mut i = 0u16;
        while i < self.rx_size {
            let buf_pa = self.dma_paddr + RX_BUF_PAGE * PAGE + (i as u64) * BUF_SZ;
            self.rx
                .set_desc(i, buf_pa, BUF_SZ as u32, virtio::DESC_F_WRITE, 0);
            self.rx.avail_push(i);
            i += 1;
        }
        self.rx.kick(&self.caps, 0);
    }

    /// 发一帧（帧已构造在 `buf_pa` 起始的 TX 缓冲里）并**等设备消费完**（TX used 环前进），
    /// 保证 TX 缓冲可安全复用。
    fn send(&self, buf_pa: u64, len: u64) {
        let before = self.tx.used_idx();
        self.tx.set_desc(0, buf_pa, len as u32, 0, 0);
        self.tx.avail_push(0);
        self.tx.kick(&self.caps, 1);
        let mut spins = 0u32;
        while self.tx.used_idx() == before && spins < 1_000_000 {
            spins += 1;
        }
    }

    /// 排空 RX used 环，返回下一帧（无则 `None`）。含 virtio-net 头剥离与游标推进。
    fn poll_rx(&mut self) -> Option<RxFrame> {
        let used = self.rx.used_idx();
        if self.last_used == used {
            return None;
        }
        let slot = (self.last_used as u64) % (self.rx_size as u64);
        let (id, rlen) = self.rx.used_elem(slot as u16);
        self.last_used = self.last_used.wrapping_add(1);
        let buf_va = self.dma_vaddr + RX_BUF_PAGE * PAGE + (id as u64) * BUF_SZ;
        Some(RxFrame {
            id,
            buf_va,
            raw_len: rlen as u64,
            eth_va: buf_va + VNET_HDR_LEN,
            len: (rlen as u64).saturating_sub(VNET_HDR_LEN),
        })
    }

    /// 把刚消费过的 RX 描述符补投回 avail 环（缓冲可复用）。
    fn recycle(&self, id: u16) {
        self.rx.avail_push(id);
    }

    /// 补投后敲一次 RX 门铃。
    fn kick_rx(&self) {
        self.rx.kick(&self.caps, 0);
    }

    /// 等下一次 RX 事件。有中断：快路径 `poll` 命中即返回，否则阻塞至多 `irq_poll_ms`；
    /// 无中断：睡固定 20ms。返回 `(本次耗时 ms, 是否命中中断)`。
    fn wait(&self, irq_poll_ms: u64) -> (u64, bool) {
        if self.irq_vectors != 0 {
            if sys_irq_poll(self.irq_mask) != 0 || sys_irq_wait(self.irq_mask, irq_poll_ms) != 0 {
                (0, true)
            } else {
                (irq_poll_ms, false)
            }
        } else {
            sys_sleep(20);
            (20, false)
        }
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
    let ip = our_ip();
    let gw = gw_ip();
    let arp: [u8; 28] = [
        0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x01, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
        ip[0], ip[1], ip[2], ip[3], 0, 0, 0, 0, 0, 0, gw[0], gw[1], gw[2], gw[3],
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
    if [rd8(arp + 14), rd8(arp + 15), rd8(arp + 16), rd8(arp + 17)] != gw_ip() {
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

// ---------------------------------------------------------------------------
// ARP 缓存老化（网关 MAC + 时间戳，TTL 过期后重新广播 ARP 刷新）
// ---------------------------------------------------------------------------

/// 网关 ARP 缓存条目：MAC + 老化计时（无墙钟 syscall，用 RX 循环累计毫秒近似）。
struct ArpCache {
    mac: [u8; 6],
    valid: bool,
    age_ms: u64,
    hits: u64,
}

impl ArpCache {
    const fn new() -> Self {
        ArpCache {
            mac: [0u8; 6],
            valid: false,
            age_ms: 0,
            hits: 0,
        }
    }

    /// 写入/刷新缓存（重置老化计时）。
    fn insert(&mut self, mac: [u8; 6]) {
        self.mac = mac;
        self.valid = true;
        self.age_ms = 0;
    }

    /// 推进老化计时。
    fn advance(&mut self, ms: u64) {
        self.age_ms = self.age_ms.saturating_add(ms);
    }

    /// 未过期则返回 MAC 并记一次命中；无效或已过期返回 `None`。
    fn lookup(&mut self) -> Option<[u8; 6]> {
        if self.valid && self.age_ms < ARP_CACHE_TTL_MS {
            self.hits += 1;
            Some(self.mac)
        } else {
            None
        }
    }

    /// 是否已过期（需重新广播 ARP 刷新）。
    fn is_stale(&self) -> bool {
        self.valid && self.age_ms >= ARP_CACHE_TTL_MS
    }

    /// 重新发过 ARP 后重置计时，避免连续重发。
    fn mark_refreshed(&mut self) {
        self.age_ms = 0;
    }
}

/// 有界自证 ARP 缓存老化：空缓存不命中 → TTL 内命中 → 边界前仍命中 → 到 TTL 变陈旧
/// → 重插后再次命中。返回 `(是否通过, 命中计数)`。
fn arp_cache_selftest() -> (bool, u64) {
    let gw = [0x52u8, 0x54, 0x00, 0x12, 0x34, 0x56];
    let mut c = ArpCache::new();
    if c.lookup().is_some() {
        return (false, c.hits); // 空缓存不应命中
    }
    c.insert(gw);
    if c.lookup() != Some(gw) {
        return (false, c.hits); // TTL 内应命中
    }
    c.advance(ARP_CACHE_TTL_MS - 1);
    if c.is_stale() {
        return (false, c.hits); // 未到 TTL 不应陈旧
    }
    if c.lookup() != Some(gw) {
        return (false, c.hits); // 边界前仍应命中
    }
    c.advance(1);
    if !c.is_stale() {
        return (false, c.hits); // 恰好到 TTL 应变陈旧
    }
    if c.lookup().is_some() {
        return (false, c.hits); // 陈旧不应命中
    }
    c.insert(gw);
    if c.lookup() != Some(gw) {
        return (false, c.hits); // 刷新后应再次命中
    }
    (true, c.hits) // hits == 3
}

// ---------------------------------------------------------------------------
// N3c — DHCP 客户端（BOOTP/DHCP over UDP；复用 ARP 的 TX 页 6，二者串行）
// ---------------------------------------------------------------------------

/// DHCP 服务端 / 客户端端口。
const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
/// DHCP 事务 id（单客户端固定值即可）。
const DHCP_XID: u32 = 0x4d4f_4e31; // "MON1"
/// magic cookie（RFC 2131）。
const DHCP_MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
/// DHCP 消息类型（option 53）。
const DHCP_DISCOVER: u8 = 1;
const DHCP_REQUEST: u8 = 3;
const DHCP_ACK: u8 = 5;
/// BOOTP 固定区（op..file）= 236 字节；其后是 4 字节 magic。
const DHCP_BOOTP_FIXED: u64 = 236;
/// DHCP 报文最小长度（BOOTP 经典要求；短包部分服务端会忽略）。
const DHCP_MIN_MSG: u64 = 300;
/// N3c 有界等待（毫秒）：到点无论结果如何都收手，不阻塞系统。
const DHCP_TIMEOUT_MS: u64 = 3000;

/// DHCP 租约。
struct DhcpLease {
    ip: [u8; 4],
    mask: [u8; 4],
    gw: [u8; 4],
    dns: [u8; 4],
}

/// 网络序（大端）读 32 位。
fn be32(a: u64) -> u32 {
    ((rd8(a) as u32) << 24)
        | ((rd8(a + 1) as u32) << 16)
        | ((rd8(a + 2) as u32) << 8)
        | rd8(a + 3) as u32
}

/// 以 `a.b.c.d` 打印 IPv4 地址（十进制点分，供自测取证）。
fn print_ip(ip: [u8; 4]) {
    print_u64(ip[0] as u64);
    print(".");
    print_u64(ip[1] as u64);
    print(".");
    print_u64(ip[2] as u64);
    print(".");
    print_u64(ip[3] as u64);
}

/// 从以太帧里取满足 `dport` 的 UDP 载荷，返回 (载荷地址, 载荷长度, 源 IP, 目的 IP)。
fn udp_payload(eth_va: u64, eth_len: u64, dport: u16) -> Option<(u64, u64, [u8; 4], [u8; 4])> {
    let info = ipv4_parse(eth_va, eth_len)?;
    if info.proto != IP_PROTO_UDP || info.payload_len < 8 {
        return None;
    }
    let udp = info.payload_off;
    if be16(udp + 2) != dport {
        return None;
    }
    let ulen = be16(udp + 4) as u64;
    if ulen < 8 || ulen > info.payload_len {
        return None;
    }
    Some((udp + 8, ulen - 8, info.src, info.dst))
}

/// 解析 DHCP 回包：BOOTREPLY + xid 匹配 + magic cookie，返回
/// (消息类型, yiaddr, 掩码, 路由器, DNS, 服务端标识)；缺失的 option 返回 `0.0.0.0`。
#[allow(clippy::type_complexity)]
fn dhcp_parse(
    payload: u64,
    len: u64,
    xid: u32,
) -> Option<(u8, [u8; 4], [u8; 4], [u8; 4], [u8; 4], [u8; 4])> {
    if len < DHCP_BOOTP_FIXED + 4 || rd8(payload) != 2 || be32(payload + 4) != xid {
        return None;
    }
    let yiaddr = [
        rd8(payload + 16),
        rd8(payload + 17),
        rd8(payload + 18),
        rd8(payload + 19),
    ];
    let ck = payload + DHCP_BOOTP_FIXED;
    if [rd8(ck), rd8(ck + 1), rd8(ck + 2), rd8(ck + 3)] != DHCP_MAGIC {
        return None;
    }
    let mut msg = 0u8;
    let mut mask = [0u8; 4];
    let mut gw = [0u8; 4];
    let mut dns = [0u8; 4];
    let mut server = [0u8; 4];
    let end = payload + len;
    let mut o = ck + 4;
    while o < end {
        let code = rd8(o);
        if code == 255 {
            break;
        }
        if code == 0 {
            o += 1;
            continue;
        }
        if o + 2 > end {
            break;
        }
        let olen = rd8(o + 1) as u64;
        if o + 2 + olen > end {
            break;
        }
        let d = o + 2;
        if olen >= 4 {
            let v = [rd8(d), rd8(d + 1), rd8(d + 2), rd8(d + 3)];
            match code {
                1 => mask = v,
                3 => gw = v,
                6 => dns = v,
                54 => server = v,
                _ => {}
            }
        }
        if code == 53 && olen >= 1 {
            msg = rd8(d);
        }
        o += 2 + olen;
    }
    if msg == 0 {
        return None;
    }
    Some((msg, yiaddr, mask, gw, dns, server))
}

/// 在 `buf_va`（TX 页）拼一条 DHCP 报文：以太广播 + IPv4(`0.0.0.0`→`255.255.255.255`) +
/// UDP `68→67` + BOOTP + magic + options，返回整帧长度。
fn dhcp_build(buf_va: u64, mac: u64, msg_type: u8, req_ip: [u8; 4], server_id: [u8; 4]) -> u64 {
    let macb = mac_bytes(mac);
    // 包头 12 字节清零（无 offload）。
    let mut k = 0u64;
    while k < VNET_HDR_LEN {
        wr8(buf_va + k, 0);
        k += 1;
    }
    // 以太 / IPv4（源 IP 0.0.0.0）/ UDP 头起始地址。
    let udp = ipv4_build_ex(
        buf_va,
        mac,
        [0xff; 6],
        IP_PROTO_UDP,
        [0, 0, 0, 0],
        [255, 255, 255, 255],
    );
    let bootp = udp + 8;
    // BOOTP 固定区清零后填字段。
    let mut i = 0u64;
    while i < DHCP_BOOTP_FIXED {
        wr8(bootp + i, 0);
        i += 1;
    }
    wr8(bootp, 1); // op = BOOTREQUEST
    wr8(bootp + 1, 1); // htype = Ethernet
    wr8(bootp + 2, 6); // hlen
    wr8(bootp + 3, 0); // hops
    wr8(bootp + 4, (DHCP_XID >> 24) as u8);
    wr8(bootp + 5, (DHCP_XID >> 16) as u8);
    wr8(bootp + 6, (DHCP_XID >> 8) as u8);
    wr8(bootp + 7, DHCP_XID as u8);
    put_be16(bootp + 10, 0x8000); // flags = broadcast（尚未取得 IP，避免服务端 ARP）
    let mut j = 0u64;
    while j < 6 {
        wr8(bootp + 28 + j, macb[j as usize]); // chaddr
        j += 1;
    }
    // magic cookie + options。
    let ck = bootp + DHCP_BOOTP_FIXED;
    let mut m = 0u64;
    while m < 4 {
        wr8(ck + m, DHCP_MAGIC[m as usize]);
        m += 1;
    }
    let mut o = ck + 4;
    wr8(o, 53);
    wr8(o + 1, 1);
    wr8(o + 2, msg_type);
    o += 3;
    if msg_type == DHCP_REQUEST {
        // option 50 = requested IP，option 54 = server identifier。
        wr8(o, 50);
        wr8(o + 1, 4);
        let mut r = 0u64;
        while r < 4 {
            wr8(o + 2 + r, req_ip[r as usize]);
            r += 1;
        }
        o += 6;
        wr8(o, 54);
        wr8(o + 1, 4);
        let mut s = 0u64;
        while s < 4 {
            wr8(o + 2 + s, server_id[s as usize]);
            s += 1;
        }
        o += 6;
    }
    // option 55 = 参数请求列表（掩码 / 路由器 / DNS / 租期 / 服务端标识）。
    wr8(o, 55);
    wr8(o + 1, 5);
    wr8(o + 2, 1);
    wr8(o + 3, 3);
    wr8(o + 4, 6);
    wr8(o + 5, 51);
    wr8(o + 6, 54);
    o += 7;
    wr8(o, 255); // end
    o += 1;
    // 补零到 DHCP 最小报文长度。
    let mut pad = o - bootp;
    while pad < DHCP_MIN_MSG {
        wr8(bootp + pad, 0);
        pad += 1;
    }
    // UDP 头。
    let ulen = 8 + DHCP_MIN_MSG;
    put_be16(udp, DHCP_CLIENT_PORT);
    put_be16(udp + 2, DHCP_SERVER_PORT);
    put_be16(udp + 4, ulen as u16);
    put_be16(udp + 6, 0); // 校验和 0 = 不校验（IPv4 允许）
    ipv4_finish(buf_va, ulen)
}

/// N3c：DHCP 客户端。经帧级 [`Nic`] 收发；成功返回租约并推进 RX 游标，超时返回 `None`。
fn dhcp_acquire(nic: &mut Nic, g: &DeviceGrant, mac: u64) -> Option<DhcpLease> {
    // DHCP 与随后的 ARP 串行，复用 ARP 的 TX 页（页 6），不额外占用 DMA 页。
    let tx_va = g.dma_vaddr + TX_BUF_PAGE * PAGE;
    let tx_pa = g.dma_paddr + TX_BUF_PAGE * PAGE;

    let mut offered = [0u8; 4];
    let mut server_id = [0u8; 4];
    let mut mask = [0u8; 4];
    let mut gw = [0u8; 4];
    let mut dns = [0u8; 4];
    let mut got_offer = false;
    let mut request_sent = false;
    let mut got_ack = false;

    let len = dhcp_build(tx_va, mac, DHCP_DISCOVER, [0; 4], [0; 4]);
    nic.send(tx_pa, len);
    println("net: DHCPDISCOVER sent (broadcast)");

    let mut ms: u64 = 0;
    while ms < DHCP_TIMEOUT_MS && !got_ack {
        // OFFER 到手就立刻发 REQUEST（并把 offered / server id 回填进去）。
        if got_offer && !request_sent {
            let len = dhcp_build(tx_va, mac, DHCP_REQUEST, offered, server_id);
            nic.send(tx_pa, len);
            request_sent = true;
            print("net: DHCPREQUEST sent for ");
            print_ip(offered);
            println("");
        }
        // 排干 RX（把每个描述符补投回 avail 环）。
        let mut drained = 0u32;
        while let Some(f) = nic.poll_rx() {
            if let Some((p, l, _s, _d)) = udp_payload(f.eth_va, f.len, DHCP_CLIENT_PORT) {
                if let Some((msg, yi, m, r, dn, sid)) = dhcp_parse(p, l, DHCP_XID) {
                    if msg == DHCP_ACK {
                        offered = yi;
                        mask = m;
                        gw = r;
                        dns = dn;
                        got_ack = true;
                    } else if msg != 0 && !got_offer {
                        // OFFER（type 2）或其它中间类型：记下 offered / server id。
                        offered = yi;
                        server_id = sid;
                        got_offer = true;
                        print("net: DHCPOFFER ");
                        print_ip(yi);
                        print(" from ");
                        print_ip(sid);
                        println("");
                    }
                }
            }
            nic.recycle(f.id);
            drained += 1;
        }
        if drained > 0 {
            nic.kick_rx();
        }
        if got_ack {
            break;
        }
        // 有界等待（有中断优先，否则睡一小段）。
        ms += nic.wait(IRQ_WAIT_MS).0;
    }
    if !got_ack {
        return None;
    }
    if mask == [0u8; 4] {
        mask = [255, 255, 255, 0];
    }
    Some(DhcpLease {
        ip: offered,
        mask,
        gw,
        dns,
    })
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

    // 建驱动态：投满 RX 缓冲 → `DRIVER_OK`（设备开始收包）。此后一切收发都经帧级 `Nic`。
    let mut nic = Nic::new(
        caps,
        rx,
        tx,
        rx_size,
        g.dma_vaddr,
        g.dma_paddr,
        irq_mask,
        irq_vectors,
    );
    println("net: DRIVER_OK, RX buffers posted");

    // N3c：先走 DHCP 取租约（失败回落默认地址）。DHCP 与随后的 ARP 串行复用 TX 页 6；
    // RX used 环消费游标现由 `Nic` 内部维护，DHCP 消费后主循环接着往后走。
    match dhcp_acquire(&mut nic, &g, mac) {
        Some(l) => {
            let gw = if l.gw == [0u8; 4] {
                DEFAULT_GW_IP
            } else {
                l.gw
            };
            unsafe {
                OUR_IP = l.ip;
                GW_IP = gw;
            }
            print("NET3 dhcp OK, ip=");
            print_ip(l.ip);
            print(" mask=");
            print_ip(l.mask);
            print(" gw=");
            print_ip(gw);
            if l.dns != [0u8; 4] {
                print(" dns=");
                print_ip(l.dns);
            }
            println("");
        }
        None => println("net: DHCP timeout, using default 10.0.2.15/10.0.2.2"),
    }

    // N3 自测：发一个广播 ARP 请求问网关 MAC（QEMU user-net 会应答）——
    // 应答回来即证明「TX 通路 + RX 通路 + 中断/轮询」整条链路通。
    let tx_buf_va = g.dma_vaddr + TX_BUF_PAGE * PAGE;
    let tx_buf_pa = g.dma_paddr + TX_BUF_PAGE * PAGE;
    // N3b：IP 帧（ICMP/UDP）用独立的一页 TX 缓冲，与 ARP 的页 6 错开、串行复用。
    let ip_buf_va = g.dma_vaddr + IP_TX_BUF_PAGE * PAGE;
    let ip_buf_pa = g.dma_paddr + IP_TX_BUF_PAGE * PAGE;

    let flen = arp_build(tx_buf_va, mac);
    nic.send(tx_buf_pa, flen);
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

    // N4 前置：TCP 头构造/解析 + 伪首部校验和 + 握手/数据段的确定性自证（不需要对端）。
    let tcp_selftest_ok = tcp_selftest(mac, ip_buf_va);

    // ARP 缓存老化自证（有界）：空缓存不命中 / TTL 内命中 / 到 TTL 过期 / 重插后命中。
    let (arp_cache_ok, arp_cache_hits) = arp_cache_selftest();
    if arp_cache_ok {
        print("net: arp cache aging OK, hits=");
        print_u64(arp_cache_hits);
        print(" ttl_ms=");
        print_u64(ARP_CACHE_TTL_MS);
        println("");
    } else {
        println("net: arp cache aging FAILED");
    }

    // 收帧：有中断走中断（快路径 poll + 阻塞 wait，超时回落重扫），否则轮询。
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
    // N4 / ARP 老化状态。
    let mut arp_cache = ArpCache::new();
    let mut tcp_syn_sent = false;
    let mut tcp_peer: u8 = 0; // 0=未知/超时, 1=握手完成, 2=被拒(RST)
    let mut tcp_ms: u64 = 0;
    let mut tcp_done = false;
    loop {
        let mut drained = 0u32;
        while let Some(f) = nic.poll_rx() {
            rx_frames += 1;
            drained += 1;
            let buf_va = f.buf_va;
            let eth_va = f.eth_va;
            let eth_len = f.len;
            // NET1：命中网关 ARP 应答即记下其 MAC 并打自测标记；同时写入/刷新 ARP 缓存。
            if let Some(m) = gw_arp_reply_mac(buf_va, f.raw_len) {
                gw_mac = m;
                arp_cache.insert(m);
                if !arp_ok {
                    arp_ok = true;
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
                    if src == gw_ip() {
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
                nic.send(ip_buf_pa, rlen);
                println("net: icmp echo request answered");
            }
            // NET4：观察对真实对端 SYN 的回应（RST = 被拒；SYN-ACK = 完成握手并发数据）。
            if tcp_syn_sent && tcp_peer == 0 {
                if let Some(seg) = tcp_parse_eth(eth_va, eth_len) {
                    if seg.sport == TEST_TCP_DPORT && seg.dport == TEST_TCP_SPORT {
                        if (seg.flags & TCP_RST) != 0 {
                            tcp_peer = 2;
                            println("net: tcp peer refused (RST)");
                        } else if (seg.flags & (TCP_SYN | TCP_ACK)) == (TCP_SYN | TCP_ACK)
                            && seg.ack == TEST_TCP_ISN.wrapping_add(1)
                        {
                            // 三次握手收尾（ACK）+ 一个最小 PSH/ACK 数据段。
                            let peer_next = seg.seq.wrapping_add(1);
                            let our_next = TEST_TCP_ISN.wrapping_add(1);
                            let dst_mac = [
                                rd8(eth_va + 6),
                                rd8(eth_va + 7),
                                rd8(eth_va + 8),
                                rd8(eth_va + 9),
                                rd8(eth_va + 10),
                                rd8(eth_va + 11),
                            ];
                            let ack = tcp_build(
                                ip_buf_va,
                                mac,
                                dst_mac,
                                our_ip(),
                                gw_ip(),
                                TEST_TCP_SPORT,
                                TEST_TCP_DPORT,
                                our_next,
                                peer_next,
                                TCP_ACK,
                                TCP_WINDOW,
                                b"",
                            );
                            nic.send(ip_buf_pa, ack);
                            let data = tcp_build(
                                ip_buf_va,
                                mac,
                                dst_mac,
                                our_ip(),
                                gw_ip(),
                                TEST_TCP_SPORT,
                                TEST_TCP_DPORT,
                                our_next,
                                peer_next,
                                TCP_PSH | TCP_ACK,
                                TCP_WINDOW,
                                b"MORION-N4",
                            );
                            nic.send(ip_buf_pa, data);
                            tcp_peer = 1;
                            println("net: tcp handshake + data sent (SYN-ACK received)");
                        }
                    }
                }
            }
            // 把同一个描述符补投回 avail 环，缓冲可被复用。
            nic.recycle(f.id);
        }
        if drained > 0 {
            nic.kick_rx();
            print("net: rx frames=");
            print_u64(rx_frames);
            print(" irq_hits=");
            print_u64(irq_hits);
            println("");
        }
        // 拿到网关 MAC 后各发一次 ICMP echo request / UDP / TCP SYN（串行复用页 7）。
        if arp_ok && !icmp_sent {
            // 从 ARP 缓存取网关 MAC（记一次命中，未命中回落到最近一次应答值）。
            let dst_mac = arp_cache.lookup().unwrap_or(gw_mac);
            let pay = ipv4_build(ip_buf_va, mac, dst_mac, IP_PROTO_ICMP, gw_ip());
            let ilen = icmp_echo_write(
                pay,
                ICMP_ECHO_REQ,
                TEST_ICMP_ID,
                TEST_ICMP_SEQ,
                b"MORION-N3B",
            );
            let ifl = ipv4_finish(ip_buf_va, ilen);
            nic.send(ip_buf_pa, ifl);
            icmp_sent = true;
            println("net: icmp echo request sent to 10.0.2.2");

            let upay = ipv4_build(ip_buf_va, mac, dst_mac, IP_PROTO_UDP, gw_ip());
            let ulen = udp_write(upay, TEST_UDP_SPORT, TEST_UDP_DPORT, b"MORION-UDP");
            let ufl = ipv4_finish(ip_buf_va, ulen);
            nic.send(ip_buf_pa, ufl);
            udp_sent = true;
            println("net: udp sent to 10.0.2.2:9999");

            // N4：向 slirp 网关试发一个 SYN（无监听端口多半回 RST/超时；有对端则完成握手）。
            let syn = tcp_build(
                ip_buf_va,
                mac,
                dst_mac,
                our_ip(),
                gw_ip(),
                TEST_TCP_SPORT,
                TEST_TCP_DPORT,
                TEST_TCP_ISN,
                0,
                TCP_SYN,
                TCP_WINDOW,
                b"",
            );
            nic.send(ip_buf_pa, syn);
            tcp_syn_sent = true;
            println("net: tcp SYN sent to 10.0.2.2:12345");
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
        // NET4 判据：确定性自证结果 + 对真实对端的有界尝试（有结论或到点即打，绝不阻塞）。
        if !tcp_done && tcp_syn_sent && (tcp_peer != 0 || tcp_ms >= TCP_PEER_TIMEOUT_MS) {
            print("NET4 tcp ");
            print(if tcp_selftest_ok { "OK" } else { "FAILED" });
            print(", handshake+data selftest ");
            print(if tcp_selftest_ok { "OK" } else { "FAILED" });
            print(", peer ");
            print(match tcp_peer {
                1 => "handshake OK",
                2 => "refused(RST)",
                _ => "timeout",
            });
            println("");
            tcp_done = true;
        }
        // 快路径 poll 命中就不睡；否则阻塞等下一次中断，超时回落重扫（无中断则睡）。
        let (elapsed_ms, hit) = nic.wait(IRQ_WAIT_MS);
        if hit {
            irq_hits += 1;
        }
        if !net2_done {
            probe_ms += elapsed_ms;
        }
        if tcp_syn_sent && !tcp_done {
            tcp_ms += elapsed_ms;
        }
        // ARP 缓存老化：推进计时；过期即重新广播 ARP 刷新（约每 TTL 一次，有界）。
        arp_cache.advance(elapsed_ms);
        if arp_cache.is_stale() {
            let flen = arp_build(tx_buf_va, mac);
            nic.send(tx_buf_pa, flen);
            arp_cache.mark_refreshed();
            print("net: arp cache expired, re-ARP sent (hits=");
            print_u64(arp_cache.hits);
            println(")");
        }
    }
}
