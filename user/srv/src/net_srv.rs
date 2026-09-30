//! 域 16 — 网络驱动服务（virtio-net，驱动路线 **N0–N3**）。
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
//! 内核侧只交出"BAR + DMA 块 + 配置空间只读通道"，设备协议全在本域 —— 这正是 D1/N 的目的。

use libdevice::grant::DeviceGrant;
use libdevice::mmio::{fence, rd16, rd32, rd8, wr16, wr32, wr64, wr8};
use libdevice::msix;
use morion::syscall::*;

// ===========================================================================
// virtio-modern（PCI transport）常量
// ===========================================================================

/// PCI 能力 ID：厂商自定义（virtio 的 modern 结构都挂在这个能力下）。
const CAP_ID_VENDOR: u8 = 0x09;
/// virtio 能力里的 `cfg_type`。
const VIRTIO_CAP_COMMON: u8 = 1;
const VIRTIO_CAP_NOTIFY: u8 = 2;
const VIRTIO_CAP_ISR: u8 = 3;
const VIRTIO_CAP_DEVICE: u8 = 4;

/// `device_status` 位。
const ST_ACKNOWLEDGE: u8 = 1;
const ST_DRIVER: u8 = 2;
const ST_DRIVER_OK: u8 = 4;
const ST_FEATURES_OK: u8 = 8;
const ST_FAILED: u8 = 0x80;

/// 特性位：`VIRTIO_F_VERSION_1` 在 feature **word 1** 的 bit 0；`VIRTIO_NET_F_MAC` 在 word 0 bit 5。
const FEAT_VERSION_1: u32 = 1 << 0;
const FEAT_NET_MAC: u32 = 1 << 5;

/// common cfg 各字段偏移（virtio 1.x 规范）。
const C_DEV_FEAT_SEL: u64 = 0x00;
const C_DEV_FEAT: u64 = 0x04;
const C_DRV_FEAT_SEL: u64 = 0x08;
const C_DRV_FEAT: u64 = 0x0c;
const C_MSIX_CONFIG: u64 = 0x10;
const C_NUM_QUEUES: u64 = 0x12;
const C_STATUS: u64 = 0x14;
const C_Q_SELECT: u64 = 0x16;
const C_Q_SIZE: u64 = 0x18;
const C_Q_MSIX: u64 = 0x1a;
const C_Q_ENABLE: u64 = 0x1c;
const C_Q_NOTIFY_OFF: u64 = 0x1e;
const C_Q_DESC: u64 = 0x20;
const C_Q_DRIVER: u64 = 0x28;
const C_Q_DEVICE: u64 = 0x30;

/// `VIRTIO_MSI_NO_VECTOR`：不给该队列/配置分配中断向量（本步走轮询）。
const NO_VECTOR: u16 = 0xFFFF;

/// 描述符标志：设备可写（RX 缓冲）。
const DESC_F_WRITE: u16 = 2;

// ===========================================================================
// 环与缓冲布局（**本驱动自己**决定，内核不参与）
// ===========================================================================

const PAGE: u64 = 4096;
/// virtqueue 深度（取 2 的幂；RX/TX 各一个队列）。
const Q_SIZE: u16 = 8;
/// RX 队列页：desc@+0 / avail@+0x100 / used@+0x200（深度 8 时都在一页内）。
const RX_RING_PAGE: u64 = 0;
/// TX 队列页（本步只建环，不发包）。
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

/// 一个 ring 页内的子偏移。
const OFF_AVAIL: u64 = 0x100;
const OFF_USED: u64 = 0x200;

/// 一个 virtqueue 在本域里的可写坐标（环都排在内核 DMA 块的同一页内）。
struct Vq {
    size: u16,
    /// 本域可写的虚拟地址。
    desc: u64,
    avail: u64,
    used: u64,
    /// 通知区偏移（驱动写 `notify` 时用）。
    notify_off: u16,
}

/// 解析出来的 virtio PCI 能力：四个区域在**内核交给的本域 BAR** 内的虚拟地址。
struct Caps {
    common: u64,
    notify: u64,
    #[allow(dead_code)]
    isr: u64,
    device: u64,
    notify_mult: u32,
}

// ===========================================================================
// 易失 MMIO 读写：统一来自 libdevice（D2），本驱动不再自带一份
// ===========================================================================

// ===========================================================================
// PCI 配置空间（只读，经过内核窄接口 `SYS_DEVICE_CONFIG_READ`）
// ===========================================================================

fn cfg_dword(off: u32) -> u32 {
    sys_device_config_read(off as u64) as u32
}
fn cfg_u8(off: u32) -> u8 {
    (cfg_dword(off & !0x3) >> ((off & 0x3) * 8)) as u8
}
fn cfg_u16(off: u32) -> u16 {
    (cfg_dword(off & !0x3) >> ((off & 0x2) * 8)) as u16
}

/// 遍历能力链表，收集 virtio 的四个 region 偏移（都换算成本域虚拟地址）。
///
/// virtio-modern 的 common/notify/ISR/device 四个区域都在同一个 BAR（内核交给我们的那根），
/// 故直接用 `bar_vaddr + cap.offset`；notify 还要读它自己的 `notify_off_multiplier`。
fn discover_caps(bar_vaddr: u64) -> Option<Caps> {
    // 配置空间状态寄存器 bit4 = 支持能力链表。
    if cfg_u16(0x06) & (1u16 << 4) == 0 {
        return None;
    }
    let mut ptr = (cfg_u8(0x34) & 0xFC) as u32;
    let mut caps = Caps {
        common: 0,
        notify: 0,
        isr: 0,
        device: 0,
        notify_mult: 0,
    };
    let mut guard = 0;
    // 能力链表节点数有限；`guard` 同时挡住固件给出的环。
    while ptr >= 0x40 && guard < 48 {
        let id = cfg_u8(ptr);
        let next = (cfg_u8(ptr + 1) & 0xFC) as u32;
        if id == CAP_ID_VENDOR {
            let cfg_type = cfg_u8(ptr + 3);
            let off = cfg_dword(ptr + 8) as u64;
            let va = bar_vaddr + off;
            match cfg_type {
                VIRTIO_CAP_COMMON => caps.common = va,
                VIRTIO_CAP_NOTIFY => {
                    caps.notify = va;
                    caps.notify_mult = cfg_dword(ptr + 16);
                }
                VIRTIO_CAP_ISR => caps.isr = va,
                VIRTIO_CAP_DEVICE => caps.device = va,
                _ => {}
            }
        }
        if next == 0 {
            break;
        }
        ptr = next;
        guard += 1;
    }
    if caps.common == 0 || caps.notify == 0 || caps.device == 0 {
        None
    } else {
        Some(caps)
    }
}

// ===========================================================================
// common cfg 访问
// ===========================================================================

fn c_r8(c: &Caps, off: u64) -> u8 {
    rd8(c.common + off)
}
fn c_r16(c: &Caps, off: u64) -> u16 {
    rd16(c.common + off)
}
fn c_r32(c: &Caps, off: u64) -> u32 {
    rd32(c.common + off)
}
fn c_w8(c: &Caps, off: u64, v: u8) {
    wr8(c.common + off, v);
}
fn c_w16(c: &Caps, off: u64, v: u16) {
    wr16(c.common + off, v)
}
fn c_w32(c: &Caps, off: u64, v: u32) {
    wr32(c.common + off, v)
}
fn c_w64(c: &Caps, off: u64, v: u64) {
    wr64(c.common + off, v)
}

/// 设置 `device_status`（覆盖写）。
fn set_status(c: &Caps, v: u8) {
    c_w8(c, C_STATUS, v);
}

/// 通知设备"队列 `qindex` 有新缓冲"。地址 = notify 基址 + `notify_off * multiplier`。
fn notify(c: &Caps, qindex: u16, notify_off: u16) {
    let addr = c.notify + (notify_off as u64) * (c.notify_mult as u64);
    wr16(addr, qindex);
}

/// 网络序（大端）读 16 位。
fn be16(a: u64) -> u16 {
    ((rd8(a) as u16) << 8) | rd8(a + 1) as u16
}

/// 把一页 DMA 内存清零（环的初始状态：`avail.idx = 0` 等；分配器不保证新帧为 0）。
fn zero_page(va: u64) {
    let mut off = 0;
    while off < PAGE {
        wr8(va + off, 0);
        off += 1;
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

/// 把描述符 0 指向 TX 缓冲并挂上 TX avail 环、敲门铃（本驱动只发单描述符包）。
fn tx_submit(tx: &Vq, c: &Caps, buf_pa: u64, len: u64) {
    wr64(tx.desc, buf_pa);
    wr32(tx.desc + 8, len as u32);
    wr16(tx.desc + 12, 0); // 设备只读，无 NEXT
    wr16(tx.desc + 14, 0);
    let idx = rd16(tx.avail + 2);
    let slot = (idx as u64) % (tx.size as u64);
    wr16(tx.avail + 4 + slot * 2, 0);
    fence();
    wr16(tx.avail + 2, idx.wrapping_add(1));
    fence();
    notify(c, 1, tx.notify_off);
}

/// 收到的帧是否是网关对 `GW_IP` 的 ARP **应答**（跳过 10 字节 virtio 包头后看以太头/ARP）。
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

/// 配置一个 virtqueue：设置大小、MSI-X 向量下标与三个环的**物理**地址并使之生效。
///
/// `msix_index` 是 MSI-X **表项下标**（`VIRTIO_MSI_NO_VECTOR` = 不给这个队列中断）。
/// 返回 `(实际深度, notify_off)`；深度 0 表示失败。
fn setup_queue(
    c: &Caps,
    idx: u16,
    want: u16,
    msix_index: u16,
    desc_pa: u64,
    avail_pa: u64,
    used_pa: u64,
) -> (u16, u16) {
    c_w16(c, C_Q_SELECT, idx);
    let max = c_r16(c, C_Q_SIZE);
    if max == 0 {
        return (0, 0);
    }
    let size = if max < want { max } else { want };
    c_w16(c, C_Q_SIZE, size);
    c_w64(c, C_Q_DESC, desc_pa);
    c_w64(c, C_Q_DRIVER, avail_pa);
    c_w64(c, C_Q_DEVICE, used_pa);
    c_w16(c, C_Q_MSIX, msix_index);
    c_w16(c, C_Q_ENABLE, 1);
    (size, c_r16(c, C_Q_NOTIFY_OFF))
}

/// 建一个 virtqueue 的坐标（环都排在 `dma_vaddr` 的 `ring_page` 那页）。
fn make_vq(g: &DeviceGrant, ring_page: u64, size: u16, notify_off: u16) -> Vq {
    let page_va = g.dma_vaddr + ring_page * PAGE;
    Vq {
        size,
        desc: page_va,
        avail: page_va + OFF_AVAIL,
        used: page_va + OFF_USED,
        notify_off,
    }
}

/// 往 RX 队列投一个"设备可写"描述符并把它的下标挂上 avail 环。
fn post_rx(vq: &Vq, desc_idx: u16, avail_idx: u16, buf_pa: u64) {
    let d = vq.desc + (desc_idx as u64) * 16;
    wr64(d, buf_pa);
    wr32(d + 8, BUF_SZ as u32);
    wr16(d + 12, DESC_F_WRITE);
    wr16(d + 14, 0); // next（不用链式描述符）
    let ring_slot = (avail_idx as u64) % (vq.size as u64);
    wr16(vq.avail + 4 + ring_slot * 2, desc_idx);
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

    let caps = match discover_caps(g.bar_vaddr) {
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
    zero_page(g.dma_vaddr + RX_RING_PAGE * PAGE);
    zero_page(g.dma_vaddr + TX_RING_PAGE * PAGE);

    // 1. 复位：写 0 到 device_status，等设备确认（modern 规定 0 = 复位）。
    set_status(&caps, 0);
    let mut spins = 0u32;
    while c_r8(&caps, C_STATUS) != 0 && spins < 1000 {
        spins += 1;
    }

    // 2. ACKNOWLEDGE | DRIVER。
    set_status(&caps, ST_ACKNOWLEDGE | ST_DRIVER);

    // 3. 特性协商：读设备特性，只接受我们要的子集。必须有 VERSION_1（否则是 legacy）。
    c_w32(&caps, C_DEV_FEAT_SEL, 0);
    let dev_feat0 = c_r32(&caps, C_DEV_FEAT);
    c_w32(&caps, C_DEV_FEAT_SEL, 1);
    let dev_feat1 = c_r32(&caps, C_DEV_FEAT);
    if dev_feat1 & FEAT_VERSION_1 == 0 {
        println("net: device lacks VIRTIO_F_VERSION_1 (legacy), aborting");
        set_status(&caps, ST_FAILED);
        idle();
    }
    let drv_feat0 = dev_feat0 & FEAT_NET_MAC;
    let drv_feat1 = dev_feat1 & FEAT_VERSION_1;
    c_w32(&caps, C_DRV_FEAT_SEL, 0);
    c_w32(&caps, C_DRV_FEAT, drv_feat0);
    c_w32(&caps, C_DRV_FEAT_SEL, 1);
    c_w32(&caps, C_DRV_FEAT, drv_feat1);

    // 4. FEATURES_OK：设备必须回读置位，否则特性不被接受。
    set_status(&caps, ST_ACKNOWLEDGE | ST_DRIVER | ST_FEATURES_OK);
    if c_r8(&caps, C_STATUS) & ST_FEATURES_OK == 0 {
        println("net: device rejected FEATURES_OK, aborting");
        set_status(&caps, ST_FAILED);
        idle();
    }

    // 5. 读设备配置：队列数 + MAC。
    let num_queues = c_r16(&caps, C_NUM_QUEUES);
    let mac_lo = rd32(caps.device);
    let mac_hi = rd16(caps.device + 4);
    let mac = ((mac_hi as u64) << 32) | mac_lo as u64;

    // 6. MSI-X 判定：内核给了向量段 + 表窗口（N2b：表在 BAR1，内核已另映射给本域）才走中断。
    let want_irq = g.msix_vector_base != 0 && g.msix_table_vaddr != 0 && g.msix_vector_count >= 2;
    let irq_base = g.msix_vector_base as u16;
    let rx_msix = if want_irq { 0 } else { NO_VECTOR };
    let tx_msix = if want_irq { 1 } else { NO_VECTOR };
    // 不做配置变更中断（本驱动不需要）。
    c_w16(&caps, C_MSIX_CONFIG, NO_VECTOR);

    // 7. 建 RX(0) / TX(1) 两个 virtqueue（各自绑一条 MSI-X 表项）。
    let (rx_size, rx_noff) = setup_queue(
        &caps,
        0,
        Q_SIZE,
        rx_msix,
        g.dma_paddr + RX_RING_PAGE * PAGE,
        g.dma_paddr + RX_RING_PAGE * PAGE + OFF_AVAIL,
        g.dma_paddr + RX_RING_PAGE * PAGE + OFF_USED,
    );
    let (tx_size, tx_noff) = setup_queue(
        &caps,
        1,
        Q_SIZE,
        tx_msix,
        g.dma_paddr + TX_RING_PAGE * PAGE,
        g.dma_paddr + TX_RING_PAGE * PAGE + OFF_AVAIL,
        g.dma_paddr + TX_RING_PAGE * PAGE + OFF_USED,
    );
    if rx_size == 0 || tx_size == 0 {
        println("net: virtqueue setup failed, aborting");
        set_status(&caps, ST_FAILED);
        idle();
    }
    let rx = make_vq(&g, RX_RING_PAGE, rx_size, rx_noff);
    let tx = make_vq(&g, TX_RING_PAGE, tx_size, tx_noff);

    print("net: virtio-net up MAC=");
    print_hex(mac);
    print(" num_queues=");
    print_u64(num_queues as u64);
    print(" rx=");
    print_u64(rx_size as u64);
    print(" tx=");
    print_u64(tx_size as u64);
    println("");

    // 8. MSI-X：写表项（表在内核另映射的窗口里）→ 注册向量 → 请内核打开 MSI-X。
    //    任一环节不成就退回轮询（等待期用 sys_sleep）。
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

    // 9. 投满 RX 缓冲（每个描述符一格缓冲；设备收包时写进对应格）。
    let mut i = 0u16;
    while i < rx_size {
        let buf_pa = g.dma_paddr + RX_BUF_PAGE * PAGE + (i as u64) * BUF_SZ;
        post_rx(&rx, i, i, buf_pa);
        i += 1;
    }
    // avail.idx = 已投递数量；之后每次回收再补投时递增。
    let mut avail_idx = rx_size;
    wr16(rx.avail + 2, avail_idx);
    fence();
    notify(&caps, 0, rx.notify_off);

    // 10. DRIVER_OK：驱动就绪，设备开始收包。
    set_status(
        &caps,
        ST_ACKNOWLEDGE | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK,
    );
    println("net: DRIVER_OK, RX buffers posted");

    // 10.5 N3 自测：发一个广播 ARP 请求问网关 MAC（QEMU user-net 会应答）——
    //      应答回来即证明「TX 通路 + RX 通路 + 中断/轮询」整条链路通。
    let tx_buf_va = g.dma_vaddr + TX_BUF_PAGE * PAGE;
    let tx_buf_pa = g.dma_paddr + TX_BUF_PAGE * PAGE;
    let flen = arp_build(tx_buf_va, mac);
    tx_submit(&tx, &caps, tx_buf_pa, flen);
    print("net: ARP request sent for 10.0.2.2 (gateway), frame len=");
    print_u64(flen);
    println("");

    // 11. 收帧：有中断走中断（快路径 poll + 阻塞 wait，超时回落重扫），否则轮询。
    let mut last_used: u16 = 0;
    let mut rx_frames: u64 = 0;
    let mut irq_hits: u64 = 0;
    let mut arp_ok = false;
    loop {
        let used_idx = rd16(rx.used + 2);
        let mut drained = 0u32;
        while last_used != used_idx {
            let slot = (last_used as u64) % (rx_size as u64);
            let e = rx.used + 4 + slot * 8;
            let id = rd32(e) as u16;
            let len = rd32(e + 4) as u64;
            rx_frames += 1;
            drained += 1;
            // 先看内容（补投前），命中网关 ARP 应答即打自测标记。
            if !arp_ok {
                let buf_va = g.dma_vaddr + RX_BUF_PAGE * PAGE + (id as u64) * BUF_SZ;
                if is_gw_arp_reply(buf_va, len) {
                    arp_ok = true;
                    print("NET1 virtio-net up, MAC=");
                    print_mac(mac);
                    println(", ARP reply OK");
                }
            }
            // 把同一个描述符补投回 avail 环，缓冲可被复用。
            let ring_slot = (avail_idx as u64) % (rx_size as u64);
            wr16(rx.avail + 4 + ring_slot * 2, id);
            avail_idx = avail_idx.wrapping_add(1);
            last_used = last_used.wrapping_add(1);
        }
        if drained > 0 {
            wr16(rx.avail + 2, avail_idx);
            fence();
            notify(&caps, 0, rx.notify_off);
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
