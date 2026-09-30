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

/// 网络序（大端）读 16 位。
fn be16(a: u64) -> u16 {
    ((rd8(a) as u16) << 8) | rd8(a + 1) as u16
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
    wr8(f + 12, 0x08);
    wr8(f + 13, 0x06);
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

/// 收到的帧是否是网关对 `GW_IP` 的 ARP **应答**（跳过 virtio 包头后看以太头/ARP）。
fn is_gw_arp_reply(buf_va: u64, len: u64) -> bool {
    if len < VNET_HDR_LEN + 42 {
        return false;
    }
    let eth = buf_va + VNET_HDR_LEN;
    if be16(eth + 12) != 0x0806 {
        return false;
    }
    let arp = eth + 14;
    if be16(arp + 6) != 0x0002 {
        return false; // oper = reply
    }
    [rd8(arp + 14), rd8(arp + 15), rd8(arp + 16), rd8(arp + 17)] == GW_IP
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
    let flen = arp_build(tx_buf_va, mac);
    tx.set_desc(0, tx_buf_pa, flen as u32, 0, 0);
    tx.avail_push(0);
    tx.kick(&caps, 1);
    print("net: ARP request sent for 10.0.2.2 (gateway), frame len=");
    print_u64(flen);
    println("");

    // 收帧：有中断走中断（快路径 poll + 阻塞 wait，超时回落重扫），否则轮询。
    let mut last_used: u16 = 0;
    let mut rx_frames: u64 = 0;
    let mut irq_hits: u64 = 0;
    let mut arp_ok = false;
    loop {
        let used_idx = rx.used_idx();
        let mut drained = 0u32;
        while last_used != used_idx {
            let slot = (last_used as u64) % (rx_size as u64);
            let (id, len) = rx.used_elem(slot as u16);
            rx_frames += 1;
            drained += 1;
            // 先看内容（补投前），命中网关 ARP 应答即打自测标记。
            if !arp_ok {
                let buf_va = g.dma_vaddr + RX_BUF_PAGE * PAGE + (id as u64) * BUF_SZ;
                if is_gw_arp_reply(buf_va, len as u64) {
                    arp_ok = true;
                    print("NET1 virtio-net up, MAC=");
                    print_mac(mac);
                    println(", ARP reply OK");
                }
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
        if irq_vectors != 0 {
            // 快路径 poll 命中就不睡；否则阻塞等下一次中断，超时回落重扫。
            let hit = sys_irq_poll(irq_mask) != 0 || sys_irq_wait(irq_mask, IRQ_WAIT_MS) != 0;
            if hit {
                irq_hits += 1;
            }
        } else {
            sys_sleep(20);
        }
    }
}
