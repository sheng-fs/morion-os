//! virtio **modern（PCI transport）传输层 + vring 原语**（驱动路线 **D2b**）。
//!
//! 这里只放「**所有 virtio 设备都一样**」的那一层：PCI 能力解析（common / notify / ISR /
//! device 四个区域）、common cfg 寄存器读写、复位与特性协商、virtqueue 的配置以及
//! avail / used 环操作。**具体设备语义**（网卡的包头与 ARP、块设备的请求链）仍在各自驱动里
//! —— 它们才是真正不同的部分。
//!
//! 典型用法（见 `net_srv` / `virtio_blk_srv`）：
//!
//! ```text
//! let caps = virtio::discover_caps(g.bar_vaddr, |off| sys_device_config_read(off as u64) as u32)?;
//! virtio::zero_page(g.dma_vaddr + RING_PAGE * virtio::PAGE);
//! let dev_feat0 = virtio::negotiate(&caps, |f0| f0 & MY_FEAT_BIT)?;   // 复位 + 协商
//! let (size, noff) = virtio::setup_queue(&caps, 0, Q_SIZE, virtio::NO_VECTOR, desc_pa, avail_pa, used_pa);
//! let q = virtio::Vq::new(g.dma_vaddr, RING_PAGE, size, noff);
//! q.set_desc(0, buf_pa, len, virtio::DESC_F_WRITE, 0);
//! q.avail_push(0);
//! q.kick(&caps, 0);
//! ```
//!
//! `discover_caps` 需要一个「读 PCI 配置空间 dword」的函数（内核窄接口 `SYS_DEVICE_CONFIG_READ`）。
//! 它做成**参数**而不是让本库直接依赖 `morion` —— `libdevice` 因此保持**零依赖**，飞地的
//! 直通形态（E3）也能原样复用同一份传输层。

use crate::mmio::{fence, rd16, rd32, rd8, wr16, wr32, wr64, wr8};

/// 页大小（环与缓冲都按页排布）。
pub const PAGE: u64 = 4096;

// ---------------------------------------------------------------------------
// PCI 能力
// ---------------------------------------------------------------------------

/// PCI 能力 ID：厂商自定义（virtio 的 modern 结构都挂在这个能力下）。
pub const CAP_ID_VENDOR: u8 = 0x09;
/// virtio 能力里的 `cfg_type`。
pub const CAP_COMMON: u8 = 1;
pub const CAP_NOTIFY: u8 = 2;
pub const CAP_ISR: u8 = 3;
pub const CAP_DEVICE: u8 = 4;

// ---------------------------------------------------------------------------
// device_status 位与特性位
// ---------------------------------------------------------------------------

pub const ST_ACKNOWLEDGE: u8 = 1;
pub const ST_DRIVER: u8 = 2;
pub const ST_DRIVER_OK: u8 = 4;
pub const ST_FEATURES_OK: u8 = 8;
pub const ST_FAILED: u8 = 0x80;

/// `VIRTIO_F_VERSION_1`（feature **word 1** 的 bit 0）：缺它说明设备是 legacy，本库只支持 modern。
pub const FEAT_VERSION_1: u32 = 1 << 0;

// ---------------------------------------------------------------------------
// common cfg 字段偏移（virtio 1.x 规范）
// ---------------------------------------------------------------------------

pub const C_DEV_FEAT_SEL: u64 = 0x00;
pub const C_DEV_FEAT: u64 = 0x04;
pub const C_DRV_FEAT_SEL: u64 = 0x08;
pub const C_DRV_FEAT: u64 = 0x0c;
pub const C_MSIX_CONFIG: u64 = 0x10;
pub const C_NUM_QUEUES: u64 = 0x12;
pub const C_STATUS: u64 = 0x14;
pub const C_Q_SELECT: u64 = 0x16;
pub const C_Q_SIZE: u64 = 0x18;
pub const C_Q_MSIX: u64 = 0x1a;
pub const C_Q_ENABLE: u64 = 0x1c;
pub const C_Q_NOTIFY_OFF: u64 = 0x1e;
pub const C_Q_DESC: u64 = 0x20;
pub const C_Q_DRIVER: u64 = 0x28;
pub const C_Q_DEVICE: u64 = 0x30;

/// `VIRTIO_MSI_NO_VECTOR`：不给该队列 / 配置分配中断向量。
pub const NO_VECTOR: u16 = 0xFFFF;

// ---------------------------------------------------------------------------
// 描述符标志
// ---------------------------------------------------------------------------

pub const DESC_F_NEXT: u16 = 1;
pub const DESC_F_WRITE: u16 = 2;

/// 一个 ring 页内的子偏移（desc@+0 / avail@+0x100 / used@+0x200）。
pub const OFF_AVAIL: u64 = 0x100;
pub const OFF_USED: u64 = 0x200;

/// 解析出来的 virtio PCI 能力：四个区域在**本域 BAR** 内的虚拟地址。
pub struct Caps {
    pub common: u64,
    pub notify: u64,
    pub isr: u64,
    pub device: u64,
    pub notify_mult: u32,
}

/// 特性协商失败原因（调用方据此打印自己的日志）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NegError {
    /// 设备缺 `VIRTIO_F_VERSION_1`（是 legacy 设备）。
    NoVersion1,
    /// 设备回读 `FEATURES_OK` 未置位 = 拒绝这组特性。
    FeaturesRejected,
}

// ---------------------------------------------------------------------------
// PCI 配置空间（读取函数由调用方提供，本库不依赖 syscall 封装）
// ---------------------------------------------------------------------------

fn cfg_u32<F: Fn(u32) -> u32>(cfg: &F, off: u32) -> u32 {
    cfg(off & !0x3)
}
fn cfg_u16<F: Fn(u32) -> u32>(cfg: &F, off: u32) -> u16 {
    (cfg(off & !0x3) >> ((off & 0x2) * 8)) as u16
}
fn cfg_u8<F: Fn(u32) -> u32>(cfg: &F, off: u32) -> u8 {
    (cfg(off & !0x3) >> ((off & 0x3) * 8)) as u8
}

/// 遍历 PCI 能力链表，收集 virtio 的四个 region（都换算成本域虚拟地址）。
///
/// `cfg` = 「读配置空间 dword」（通常是 `|off| sys_device_config_read(off as u64) as u32`）。
/// 四个区域都在**同一个 BAR**（内核交给驱动的那根），故直接用 `bar_vaddr + cap.offset`；
/// notify 还要读它自己的 `notify_off_multiplier`。
pub fn discover_caps<F: Fn(u32) -> u32>(bar_vaddr: u64, cfg: F) -> Option<Caps> {
    // 配置空间状态寄存器 bit4 = 支持能力链表。
    if cfg_u16(&cfg, 0x06) & (1u16 << 4) == 0 {
        return None;
    }
    let mut ptr = (cfg_u8(&cfg, 0x34) & 0xFC) as u32;
    let mut caps = Caps {
        common: 0,
        notify: 0,
        isr: 0,
        device: 0,
        notify_mult: 0,
    };
    let mut guard = 0;
    // 能力链表节点数有限（PCI 规范上限 48）；`guard` 同时挡住固件给出的环。
    while ptr >= 0x40 && guard < 48 {
        let id = cfg_u8(&cfg, ptr);
        let next = (cfg_u8(&cfg, ptr + 1) & 0xFC) as u32;
        if id == CAP_ID_VENDOR {
            let cfg_type = cfg_u8(&cfg, ptr + 3);
            let off = cfg_u32(&cfg, ptr + 8) as u64;
            let va = bar_vaddr + off;
            match cfg_type {
                CAP_COMMON => caps.common = va,
                CAP_NOTIFY => {
                    caps.notify = va;
                    caps.notify_mult = cfg_u32(&cfg, ptr + 16);
                }
                CAP_ISR => caps.isr = va,
                CAP_DEVICE => caps.device = va,
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

// ---------------------------------------------------------------------------
// common cfg 访问 + 复位 / 特性协商
// ---------------------------------------------------------------------------

impl Caps {
    pub fn r8(&self, off: u64) -> u8 {
        rd8(self.common + off)
    }
    pub fn r16(&self, off: u64) -> u16 {
        rd16(self.common + off)
    }
    pub fn r32(&self, off: u64) -> u32 {
        rd32(self.common + off)
    }
    pub fn w8(&self, off: u64, v: u8) {
        wr8(self.common + off, v)
    }
    pub fn w16(&self, off: u64, v: u16) {
        wr16(self.common + off, v)
    }
    pub fn w32(&self, off: u64, v: u32) {
        wr32(self.common + off, v)
    }
    pub fn w64(&self, off: u64, v: u64) {
        wr64(self.common + off, v)
    }

    /// 当前 `device_status`。
    pub fn status(&self) -> u8 {
        self.r8(C_STATUS)
    }
    /// 覆盖写 `device_status`。
    pub fn set_status(&self, v: u8) {
        self.w8(C_STATUS, v);
    }
    /// 设备支持的队列数。
    pub fn num_queues(&self) -> u16 {
        self.r16(C_NUM_QUEUES)
    }
    /// 读 ISR 状态寄存器（读即应答并清除设备侧中断，virtio 规范要求）。
    pub fn read_isr(&self) -> u8 {
        rd8(self.isr)
    }
    /// 给本设备配置变更中断分配/撤销向量（本库驱动都不需要 → 传 [`NO_VECTOR`]）。
    pub fn set_config_msix(&self, v: u16) {
        self.w16(C_MSIX_CONFIG, v);
    }

    /// 通知设备「队列 `qindex` 有新缓冲」：地址 = notify 基址 + `notify_off * multiplier`。
    pub fn notify(&self, qindex: u16, notify_off: u16) {
        let addr = self.notify + (notify_off as u64) * (self.notify_mult as u64);
        wr16(addr, qindex);
    }

    /// 复位：写 0 到 `device_status`，等设备确认（modern 规定 0 = 复位）。
    pub fn reset(&self) {
        self.set_status(0);
        let mut spins = 0u32;
        while self.status() != 0 && spins < 1000 {
            spins += 1;
        }
    }

    /// 标记设备不可用（协商失败的收尾动作）。
    pub fn failed(&self) {
        self.set_status(ST_FAILED);
    }

    /// 驱动就绪（`ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK`）。
    pub fn driver_ok(&self) {
        self.set_status(ST_ACKNOWLEDGE | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK);
    }
}

/// 读设备特性 `(word0, word1)`。
pub fn read_features(c: &Caps) -> (u32, u32) {
    c.w32(C_DEV_FEAT_SEL, 0);
    let f0 = c.r32(C_DEV_FEAT);
    c.w32(C_DEV_FEAT_SEL, 1);
    let f1 = c.r32(C_DEV_FEAT);
    (f0, f1)
}

/// 复位 → `ACKNOWLEDGE | DRIVER` → 校验 `VIRTIO_F_VERSION_1` → 写驱动特性 →
/// `FEATURES_OK`（回读校验）。
///
/// `pick_feat0(device_feat0)` 给出驱动在 word 0 要协商的位（word 1 恒只取 `VERSION_1`）；
/// 返回设备特性 word 0，调用方据此读 MAC / 容量之类。
pub fn negotiate<F: Fn(u32) -> u32>(c: &Caps, pick_feat0: F) -> Result<u32, NegError> {
    c.reset();
    c.set_status(ST_ACKNOWLEDGE | ST_DRIVER);
    let (dev0, dev1) = read_features(c);
    if dev1 & FEAT_VERSION_1 == 0 {
        return Err(NegError::NoVersion1);
    }
    c.w32(C_DRV_FEAT_SEL, 0);
    c.w32(C_DRV_FEAT, pick_feat0(dev0));
    c.w32(C_DRV_FEAT_SEL, 1);
    c.w32(C_DRV_FEAT, FEAT_VERSION_1);
    c.set_status(ST_ACKNOWLEDGE | ST_DRIVER | ST_FEATURES_OK);
    if c.status() & ST_FEATURES_OK == 0 {
        return Err(NegError::FeaturesRejected);
    }
    Ok(dev0)
}

// ---------------------------------------------------------------------------
// 队列配置与 vring
// ---------------------------------------------------------------------------

/// 把一页 DMA 内存清零（环的初始状态：`avail.idx = 0` / `used.idx = 0` 必须是 0）。
pub fn zero_page(va: u64) {
    let mut off = 0;
    while off < PAGE {
        wr8(va + off, 0);
        off += 1;
    }
}

/// 配置一个 virtqueue：设置大小、MSI-X 向量下标与三个环的**物理**地址并使之生效。
///
/// `msix_index` 是 MSI-X **表项下标**（[`NO_VECTOR`] = 不给这个队列中断）。返回
/// `(实际深度, notify_off)`；深度 0 表示失败。
pub fn setup_queue(
    c: &Caps,
    idx: u16,
    want: u16,
    msix_index: u16,
    desc_pa: u64,
    avail_pa: u64,
    used_pa: u64,
) -> (u16, u16) {
    c.w16(C_Q_SELECT, idx);
    let max = c.r16(C_Q_SIZE);
    if max == 0 {
        return (0, 0);
    }
    let size = if max < want { max } else { want };
    c.w16(C_Q_SIZE, size);
    c.w64(C_Q_DESC, desc_pa);
    c.w64(C_Q_DRIVER, avail_pa);
    c.w64(C_Q_DEVICE, used_pa);
    c.w16(C_Q_MSIX, msix_index);
    c.w16(C_Q_ENABLE, 1);
    (size, c.r16(C_Q_NOTIFY_OFF))
}

/// 一个 virtqueue 在**本域**里的可写坐标（环都排在内核交出的连续 DMA 块的同一页内）。
pub struct Vq {
    pub size: u16,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    pub notify_off: u16,
}

impl Vq {
    /// 环排在内核 DMA 块的 `ring_page` 那页。
    pub fn new(dma_vaddr: u64, ring_page: u64, size: u16, notify_off: u16) -> Self {
        let page = dma_vaddr + ring_page * PAGE;
        Vq {
            size,
            desc: page,
            avail: page + OFF_AVAIL,
            used: page + OFF_USED,
            notify_off,
        }
    }

    /// 写一条描述符（`idx` 项）。
    pub fn set_desc(&self, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let d = self.desc + (idx as u64) * 16;
        wr64(d, addr);
        wr32(d + 8, len);
        wr16(d + 12, flags);
        wr16(d + 14, next);
    }

    /// 把描述符链头挂上 avail 环并推进 `avail.idx`；返回**本条**的 avail 下标。
    ///
    /// 已含必要的写序栅栏（环项先于 `idx`）；门铃由调用方用 [`Vq::kick`] 敲（便于批量投递）。
    pub fn avail_push(&self, desc_head: u16) -> u16 {
        let a = rd16(self.avail + 2);
        let slot = (a as u64) % (self.size as u64);
        wr16(self.avail + 4 + slot * 2, desc_head);
        fence();
        wr16(self.avail + 2, a.wrapping_add(1));
        fence();
        a
    }

    /// 敲门铃（通知设备本队列有新缓冲）。
    pub fn kick(&self, c: &Caps, qindex: u16) {
        c.notify(qindex, self.notify_off);
    }

    /// 设备已完成的请求数游标（used 环的 `idx`）。
    pub fn used_idx(&self) -> u16 {
        rd16(self.used + 2)
    }

    /// 读 used 环第 `slot` 格（`0..size`），返回 `(描述符链头, 总长度)`。
    pub fn used_elem(&self, slot: u16) -> (u16, u32) {
        let e = self.used + 4 + (slot as u64) * 8;
        (rd32(e) as u16, rd32(e + 4))
    }
}
