//! 域 17 — 第二个真实驱动：**virtio-blk**（驱动路线 **D3**）。
//!
//! 目的：拿 D1 的**通用设备授权**（`DeviceGrant`）去驱动一台**新类型**的设备，全程**不改
//! 内核设备逻辑** —— 内核只按类找到 virtio-blk、交出 BAR + 连续 DMA 块 + MSI-X 参数，设备
//! 协议（virtio-blk 的请求链）完全在本域。
//!
//! 与 [`crate::net_srv`]（virtio-net）同源：同为 virtio-modern（PCI transport），配置结构
//! 都在 BAR4、MSI-X 表在 BAR1，握手流程（复位 → 协商特性 → 建队列 → DRIVER_OK）一致。区别
//! 只在设备语义：virtio-blk 只有**一个** virtqueue，每个请求是一条**三段式描述符链**
//! （header 16B → data 512B → status 1B）。
//!
//! 自测：读扇区 0 校验签名（宿主造盘时写入 `MORION-VBLK-TEST!`）→ 写扇区 1 再读回校验 →
//! 串口打 marker。这一步同时验证 **DMA 描述符链 + avail/used 环 + MSI-X 中断** 整条链路。

use libdevice::grant::DeviceGrant;
use libdevice::mmio::{fence, rd16, rd32, rd64, rd8, wr16, wr32, wr64, wr8};
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

/// 特性位：`VIRTIO_F_VERSION_1` 在 feature **word 1** 的 bit 0（必须有，否则是 legacy）。
const FEAT_VERSION_1: u32 = 1 << 0;

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

/// `VIRTIO_MSI_NO_VECTOR`：不给该队列分配中断向量。
const NO_VECTOR: u16 = 0xFFFF;

/// 描述符标志。
const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

/// virtio-blk 请求类型（spec：`VIRTIO_BLK_T_*`）。
const BLK_T_IN: u32 = 0; // 读（设备 → 内存）
const BLK_T_OUT: u32 = 1; // 写（内存 → 设备）

/// 请求完成状态字节：0 = OK。
const BLK_S_OK: u8 = 0;

// ===========================================================================
// 环与缓冲布局（本驱动自己决定，内核不参与）
// ===========================================================================

const PAGE: u64 = 4096;
/// 请求队列深度（2 的幂）。
const Q_SIZE: u16 = 8;
/// 请求队列页：desc@+0 / avail@+0x100 / used@+0x200。
const REQ_RING_PAGE: u64 = 0;
/// 请求头（16B：type u32 / reserved u32 / sector u64）。
const REQ_HDR_PAGE: u64 = 1;
/// 数据缓冲（一个扇区 512B）。
const REQ_DATA_PAGE: u64 = 2;
/// 状态字节（1B）。
const REQ_STATUS_PAGE: u64 = 3;
/// 扇区大小。
const SECTOR: u64 = 512;
/// 本驱动需要的 DMA 页数（与 `main.rs` 里给 virtio-blk 声明的 `dma_pages: 8` 一致）。
const DMA_PAGES: u64 = 8;
/// 请求等待中断的超时（毫秒）；超时即重扫 used 环兜底。
const IRQ_WAIT_MS: u64 = 100;

/// 一个 ring 页内的子偏移。
const OFF_AVAIL: u64 = 0x100;
const OFF_USED: u64 = 0x200;

/// 宿主造盘时写进扇区 0 的签名（自测据此确认"读到的确实是那块盘"）。
const SIG: [u8; 16] = *b"MORION-VBLK-TST!";

/// 请求队列在本域里的可写坐标。
struct Vq {
    size: u16,
    desc: u64,
    avail: u64,
    used: u64,
    notify_off: u16,
}

/// 解析出来的 virtio PCI 能力：各区域在**内核交给的本域 BAR**内的虚拟地址。
struct Caps {
    common: u64,
    notify: u64,
    isr: u64,
    device: u64,
    notify_mult: u32,
}

/// 驱动运行时状态：一个请求队列 + 复用的请求缓冲 + 中断掩码。
struct Blk {
    q: Vq,
    c: Caps,
    hdr_pa: u64,
    hdr_va: u64,
    data_pa: u64,
    data_va: u64,
    status_pa: u64,
    status_va: u64,
    /// 本设备向量对应的中断掩码（0 = 走轮询）。
    irq_mask: u64,
}

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
    (cfg_dword(off & !0x2) >> ((off & 0x2) * 8)) as u16
}

/// 遍历能力链表，收集 virtio 的四个 region 偏移（都换算成本域虚拟地址）。
fn discover_caps(bar_vaddr: u64) -> Option<Caps> {
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
    wr8(c.common + off, v)
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

/// 通知设备"队列 0 有新请求"。地址 = notify 基址 + `notify_off * multiplier`。
fn notify(c: &Caps, qindex: u16, notify_off: u16) {
    let addr = c.notify + (notify_off as u64) * (c.notify_mult as u64);
    wr16(addr, qindex);
}

/// 把一页 DMA 内存清零（环的初始状态：`avail.idx = 0` 等）。
fn zero_page(va: u64) {
    let mut off = 0;
    while off < PAGE {
        wr8(va + off, 0);
        off += 1;
    }
}

/// 按 ASCII 打印一段定长字节（用于打签名）。
fn print_sig(sig: &[u8; 16]) {
    print(unsafe { core::str::from_utf8_unchecked(&sig[..]) });
}

/// 配置一个 virtqueue：设置大小、MSI-X 向量下标与三个环的**物理**地址并使之生效。
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

/// 建请求队列的坐标（环都排在 `dma_vaddr` 的 `ring_page` 那页）。
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

/// 永不返回的保活循环（无设备 / 初始化失败时用）。
fn idle() -> ! {
    loop {
        sys_sleep(500);
    }
}

impl Blk {
    /// 提交一个单扇区请求并**同步等待**完成，返回状态字节（0 = OK）。
    ///
    /// 三段式描述符链：`[0]` header（设备只读）→ `[1]` data（读时设备可写）→ `[2]` status（设备可写）。
    fn request(&self, req_type: u32, sector: u64) -> u8 {
        // 请求头。
        wr32(self.hdr_va, req_type);
        wr32(self.hdr_va + 4, 0);
        wr64(self.hdr_va + 8, sector);
        // 状态先置非 0：设备写 0 表示成功，若链路没跑起来也能被识别为"没完成"。
        wr8(self.status_va, 0xFF);

        // 描述符链 0 → 1 → 2。
        let d = self.q.desc;
        wr64(d, self.hdr_pa);
        wr32(d + 8, 16);
        wr16(d + 12, DESC_F_NEXT);
        wr16(d + 14, 1);

        let dd = d + 16;
        let data_flags = if req_type == BLK_T_IN {
            DESC_F_NEXT | DESC_F_WRITE
        } else {
            DESC_F_NEXT
        };
        wr64(dd, self.data_pa);
        wr32(dd + 8, SECTOR as u32);
        wr16(dd + 12, data_flags);
        wr16(dd + 14, 2);

        let ds = d + 32;
        wr64(ds, self.status_pa);
        wr32(ds + 8, 1);
        wr16(ds + 12, DESC_F_WRITE);
        wr16(ds + 14, 0);

        // 挂上 avail 环（desc 头 = 0）→ 门铃。
        let before = rd16(self.q.used + 2);
        let a = rd16(self.q.avail + 2);
        let slot = (a as u64) % (self.q.size as u64);
        wr16(self.q.avail + 4 + slot * 2, 0);
        fence();
        wr16(self.q.avail + 2, a.wrapping_add(1));
        fence();
        notify(&self.c, 0, self.q.notify_off);

        // 等完成：有中断走中断（快路径 poll + 阻塞 wait），否则让出 CPU 轮询。
        let mut tries = 0u32;
        while rd16(self.q.used + 2) == before && tries < 100 {
            if self.irq_mask != 0 {
                if sys_irq_poll(self.irq_mask) == 0 {
                    sys_irq_wait(self.irq_mask, IRQ_WAIT_MS);
                }
                // 读 ISR 以应答并清除设备侧中断（virtio 规范要求）。
                let _ = rd8(self.c.isr);
            } else {
                sys_sleep(1);
            }
            tries += 1;
        }
        // 回收 used 环：used 元素下标 = (used.idx - 1) % size。
        let used_idx = rd16(self.q.used + 2);
        let elem = self.q.used + 4 + (((used_idx.wrapping_sub(1)) as u64) % (self.q.size as u64)) * 8;
        let _id = rd32(elem);
        let _len = rd32(elem + 4);
        rd8(self.status_va)
    }
}

/// 域 17 — virtio_blk_srv：virtio-blk modern 驱动 + 自测（D3）。
pub fn run() {
    let g = DeviceGrant::load();
    if !g.is_valid() {
        println("vblk: no device grant (no virtio-blk), idle");
        idle();
    }
    if g.dma_bytes < DMA_PAGES * PAGE {
        println("vblk: device grant DMA too small, aborting");
        idle();
    }

    let caps = match discover_caps(g.bar_vaddr) {
        Some(c) => c,
        None => {
            println("vblk: no virtio PCI capabilities, aborting");
            idle();
        }
    };
    print("vblk: virtio caps common=0x");
    print_hex(caps.common);
    print(" notify=0x");
    print_hex(caps.notify);
    print(" device=0x");
    print_hex(caps.device);
    print(" mult=");
    print_u64(caps.notify_mult as u64);
    println("");

    // 环的初始状态：清零请求队列页（`avail.idx` / `used.idx` 必须是 0）。
    zero_page(g.dma_vaddr + REQ_RING_PAGE * PAGE);

    // 1. 复位：写 0 到 device_status，等设备确认。
    set_status(&caps, 0);
    let mut spins = 0u32;
    while c_r8(&caps, C_STATUS) != 0 && spins < 1000 {
        spins += 1;
    }

    // 2. ACKNOWLEDGE | DRIVER。
    set_status(&caps, ST_ACKNOWLEDGE | ST_DRIVER);

    // 3. 特性协商：只要有 VERSION_1；virtio-blk 的设备特性（RO / BLK_SIZE / FLUSH…）全不要。
    c_w32(&caps, C_DEV_FEAT_SEL, 0);
    let _dev_feat0 = c_r32(&caps, C_DEV_FEAT);
    c_w32(&caps, C_DEV_FEAT_SEL, 1);
    let dev_feat1 = c_r32(&caps, C_DEV_FEAT);
    if dev_feat1 & FEAT_VERSION_1 == 0 {
        println("vblk: device lacks VIRTIO_F_VERSION_1 (legacy), aborting");
        set_status(&caps, ST_FAILED);
        idle();
    }
    c_w32(&caps, C_DRV_FEAT_SEL, 0);
    c_w32(&caps, C_DRV_FEAT, 0);
    c_w32(&caps, C_DRV_FEAT_SEL, 1);
    c_w32(&caps, C_DRV_FEAT, FEAT_VERSION_1);

    // 4. FEATURES_OK：设备必须回读置位，否则特性不被接受。
    set_status(&caps, ST_ACKNOWLEDGE | ST_DRIVER | ST_FEATURES_OK);
    if c_r8(&caps, C_STATUS) & ST_FEATURES_OK == 0 {
        println("vblk: device rejected FEATURES_OK, aborting");
        set_status(&caps, ST_FAILED);
        idle();
    }

    // 5. 读设备配置：队列数 + 容量（单位 = 512B 扇区）。
    let num_queues = c_r16(&caps, C_NUM_QUEUES);
    let capacity = rd64(caps.device);

    // 6. MSI-X 判定：内核给了向量段 + 表窗口才走中断（单队列 → 1 条向量）。
    let want_irq = g.msix_vector_base != 0 && g.msix_table_vaddr != 0 && g.msix_vector_count >= 1;
    let irq_base = g.msix_vector_base as u16;
    let q_msix = if want_irq { 0 } else { NO_VECTOR };
    c_w16(&caps, C_MSIX_CONFIG, NO_VECTOR);

    // 7. 建请求队列（队列 0）。
    let (qsize, noff) = setup_queue(
        &caps,
        0,
        Q_SIZE,
        q_msix,
        g.dma_paddr + REQ_RING_PAGE * PAGE,
        g.dma_paddr + REQ_RING_PAGE * PAGE + OFF_AVAIL,
        g.dma_paddr + REQ_RING_PAGE * PAGE + OFF_USED,
    );
    if qsize == 0 {
        println("vblk: virtqueue setup failed, aborting");
        set_status(&caps, ST_FAILED);
        idle();
    }
    let q = make_vq(&g, REQ_RING_PAGE, qsize, noff);

    print("vblk: virtio-blk up num_queues=");
    print_u64(num_queues as u64);
    print(" qsize=");
    print_u64(qsize as u64);
    print(" cap=");
    print_u64(capacity);
    println(" sectors");

    // 8. MSI-X：写表项 → 注册向量 → 请内核打开 MSI-X。任一环节不成就退回轮询。
    let mut irq_mask: u64 = 0;
    if want_irq {
        msix::write_table_entry(&g, 0, irq_base as u64);
        let r0 = sys_register_irq(irq_base as u64);
        if sys_msix_enable() == 1 && r0 == 1 {
            // 掩码位 `i` ↔ 向量 `MSI_VECTOR_BASE + i`：本设备向量段不从段首开始，整体左移。
            let shift = (irq_base as u64).wrapping_sub(MSI_VECTOR_BASE);
            irq_mask = 1u64 << shift;
            print("vblk: MSI-X enabled vector=0x");
            print_hex(irq_base as u64);
            println("");
        } else {
            println("vblk: MSI-X enable/register failed, polling");
        }
    } else {
        println("vblk: no MSI-X (kernel gave no vector/table window), polling");
    }

    // 9. DRIVER_OK：驱动就绪，设备开始处理请求。
    set_status(
        &caps,
        ST_ACKNOWLEDGE | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK,
    );

    let blk = Blk {
        q,
        c: caps,
        hdr_pa: g.dma_paddr + REQ_HDR_PAGE * PAGE,
        hdr_va: g.dma_vaddr + REQ_HDR_PAGE * PAGE,
        data_pa: g.dma_paddr + REQ_DATA_PAGE * PAGE,
        data_va: g.dma_vaddr + REQ_DATA_PAGE * PAGE,
        status_pa: g.dma_paddr + REQ_STATUS_PAGE * PAGE,
        status_va: g.dma_vaddr + REQ_STATUS_PAGE * PAGE,
        irq_mask,
    };

    // 10. 自测 A：读扇区 0，校验签名。
    let st0 = blk.request(BLK_T_IN, 0);
    if st0 != BLK_S_OK {
        print("vblk: read sector 0 failed, status=");
        print_u64(st0 as u64);
        println("");
        idle();
    }
    let mut sig = [0u8; 16];
    let mut sig_ok = true;
    let mut i = 0u64;
    while i < SIG.len() as u64 {
        sig[i as usize] = rd8(blk.data_va + i);
        if sig[i as usize] != SIG[i as usize] {
            sig_ok = false;
        }
        i += 1;
    }

    // 11. 自测 B：往扇区 1 写一个可复算的花纹，再读回校验。
    let mut k = 0u64;
    while k < SECTOR {
        wr8(blk.data_va + k, (k as u8).wrapping_mul(7).wrapping_add(0x33));
        k += 1;
    }
    let st_w = blk.request(BLK_T_OUT, 1);
    // 读回前先把缓冲涂成别的值，确保校验的是"从盘读回的内容"。
    let mut m = 0u64;
    while m < SECTOR {
        wr8(blk.data_va + m, 0);
        m += 1;
    }
    let st_r = blk.request(BLK_T_IN, 1);
    let mut rw_ok = st_w == BLK_S_OK && st_r == BLK_S_OK;
    let mut j = 0u64;
    while j < SECTOR {
        let want = (j as u8).wrapping_mul(7).wrapping_add(0x33);
        if rd8(blk.data_va + j) != want {
            rw_ok = false;
        }
        j += 1;
    }

    // 12. marker：整条链路（描述符链 + 环 + 中断/轮询）的端到端取证。
    print("VBLK1 virtio-blk OK, cap=");
    print_u64(capacity);
    print(", sector0 sig=");
    print_sig(&sig);
    print(", sig=");
    print(if sig_ok { "ok" } else { "BAD" });
    print(", rw=");
    print(if rw_ok { "ok" } else { "BAD" });
    println("");

    // 驱动就绪后长驻：与其它常驻服务一致，保持域存活。
    loop {
        sys_sleep(1000);
    }
}
