//! RTL8168/8111（Realtek r8169 系）**轮询**网卡驱动骨架。
//!
//! 目标：拿 `libdevice::grant::DeviceGrant` 驱动真机上的 `10ec:8168`（RTL8111/8168 有线网卡），
//! 做到「能被接线、能收发一帧」。与 `intel_nic.rs` 一样**全轮询**：本内核只有 MSI-X 通路，
//! RTL8168 常态用 INTx/MSI，故不申请中断向量，靠 `IntrStatus` / 描述符 OWN 位轮询。
//!
//! ## 需要的设备授权
//! - **BAR0**：MMIO 寄存器窗口。RTL8168 的寄存器空间只到 `0xE8`（BAR 本身通常 256 字节），
//!   映射 **1 页（4 KiB）** 足够覆盖本驱动用到的全部寄存器。BAR 偏移表来源：Realtek
//!   RTL8168/8111 数据手册的寄存器章节 + 开源 `r8169` 驱动的 `enum rtl_registers`
//!   （两者一致；下面每个常量都标了名字）。
//! - **DMA**：物理连续 **27 页（4 KiB/页）**，本驱动在块内自划分：
//!   - 页 0：RX 描述符环（64 × 16 B = 1 KiB）；
//!   - 页 1：TX 描述符环（64 × 16 B = 1 KiB）；
//!   - 页 2..=25：RX 缓冲（64 × 1536 B = 96 KiB，24 页）；
//!   - 页 26：TX 缓冲（一帧，≤ 2048 B）。
//! - MSI-X：**不用**（`msix_vectors = 0`）。
//!
//! ## 芯片识别与变体
//! `DeviceGrant` 不携带 PCI revision / vendor:device，驱动**只读** `TxConfig`（0x40）的
//! 芯片版本字段 `[30:20]`（掩码 `0x7CF`）来区分变体；识别发生在写任何寄存器之前。
//! 认不出的版本打印一行 `r8169: unsupported chip variant ...` 后 `idle()`，不碰硬件。
//! 支持的家族：RTL8169/8110（classic）、RTL8168B/C、RTL8168D/E、RTL8168F/G/H；同一家族
//! 内共用保守初始化序列（8168 系额外配置 C+ 寄存器：只开 64 位 DMA 的 PCIDAC，
//! 8169/8110 没有该寄存器）。
//!
//! ## 已知未实现（第二批可补）
//! - RTL8168D+ 的固件补丁 / CSI 访问、PHY 参数微调、ASPM/L1 配置；
//! - VLAN / 校验和 offload、巨帧、多播哈希过滤（当前一律 **promiscuous** 收包）；
//! - 中断 / MSI-X（保持轮询）、链路状态变化事件。
//!
//! ## 接线约定（第二批）
//! 入口是 [`run`]，从 `DeviceGrant::load_at(0)` 读授权 —— 因此内核必须用
//! `device::grant_slot(req, 0)` / `grant_empty_multi` 映射多实例槽 0，而不是单实例
//! `device::grant`（后者只映射 `DEVICE_CFG_VADDR`）。帧级 IPC 契约与 `intel_nic.rs`
//! 完全相同：`NET_REQ_TAG` + `NetReq{op: NET_OP_TX/RX/INFO, len, buf}`。

use libdevice::grant::DeviceGrant;
use libdevice::mmio::{fence, rd32, rd8, wr16, wr32, wr8};
use morion::syscall::*;
// 显式从公共模块引入 `PAYLOAD_LEN`（不依赖 `crate::common` 的私有 glob 再导出）。
use morion::syscall::PAYLOAD_LEN;

use crate::common::{
    Message, NetReq, NET_FRAME_MAX, NET_OP_INFO, NET_OP_RX, NET_OP_TX, NET_REQ_TAG,
};

// ===========================================================================
// 寄存器偏移（BAR0 MMIO）—— 名称对应 r8169 的 enum rtl_registers
// ===========================================================================

/// IDR0..IDR5：上电时由 EEPROM/OTP 装载的 MAC（6 字节）。
const REG_IDR0: u64 = 0x00;
/// MAR0..MAR7：多播哈希表（8 × u32）。
const REG_MAR0: u64 = 0x08;
/// TNPDS：TX 描述符环起始物理地址（低 32 位）。
const REG_TNPDS: u64 = 0x20;
/// TNPDS_HI：TX 描述符环起始物理地址（高 32 位）。
const REG_TNPDS_HI: u64 = 0x24;
/// ChipCmd（CR，8 位）：RxBufEmpty / TxEnb / RxEnb / Reset。
const REG_CHIP_CMD: u64 = 0x37;
/// TxPoll：bit6 = NPQ，写 1 通知网卡检查 TX 环（r8169 没有 TDT）。
const REG_TX_POLL: u64 = 0x38;
/// IntrMask（16 位）：写 0 关全部中断。
const REG_INTR_MASK: u64 = 0x3C;
/// IntrStatus（16 位）：写 1 清对应中断。
const REG_INTR_STATUS: u64 = 0x3E;
/// TxConfig（32 位）：高 12 位含芯片版本 ID；低半部是 IFG / DMA burst。
const REG_TX_CONFIG: u64 = 0x40;
/// RxConfig（32 位）：位 0..3 = 收全部物理地址 / 本机 / 多播 / 广播。
const REG_RX_CONFIG: u64 = 0x44;
/// Cfg9346：写 0xC0 解锁配置寄存器，写 0x00 加锁。
const REG_CFG9346: u64 = 0x50;
/// RxMaxSize（16 位）：可接收的最大帧长。
const REG_RX_MAX_SIZE: u64 = 0xDA;
/// CPlusCmd（16 位）：C+ 芯片（8168 系）才有的 offload / 控制位。
const REG_CPLUS_CMD: u64 = 0xE0;
/// CPlusCmd.PCIDAC：允许描述符使用 64 位 DMA 地址（本驱动的描述符带 addr_hi）。
const CPLUS_PCIDAC: u16 = 0x0010;
/// RDSAR：RX 描述符环起始物理地址（低 32 位）。
const REG_RDSAR: u64 = 0xE4;
/// RDSAR_HI：RX 描述符环起始物理地址（高 32 位）。
const REG_RDSAR_HI: u64 = 0xE8;

// ---------------------------------------------------------------------------
// 位定义
// ---------------------------------------------------------------------------

const CFG9346_UNLOCK: u8 = 0xC0;
const CFG9346_LOCK: u8 = 0x00;

// ChipCmd：0x01 = RxBufEmpty(RO)，0x04 = TxEnb，0x08 = RxEnb，0x10 = Reset。
const CMD_RESET: u8 = 0x10;
const CMD_TX_EN: u8 = 0x04;
const CMD_RX_EN: u8 = 0x08;
/// TxPoll.NPQ：通知网卡「TX 环里有新描述符」。
const TX_POLL_NPQ: u8 = 0x40;

// 16 字节描述符（TX / RX 布局相同：opts1 / opts2 / addr_lo / addr_hi）。
const DESC_OWN: u32 = 0x8000_0000; // bit31：1 = 交给网卡；网卡完成后清 0
const TX_FIRST_FRAG: u32 = 0x2000_0000; // bit29
const TX_LAST_FRAG: u32 = 0x1000_0000; // bit28
/// RX opts1 的错误位：RxRES(21) / RxCRC(19) / RxRUNT(18) / RxRWT(17) / RxFOVF(16)。
const RX_DESC_ERR: u32 = 0x003F_0000;

/// TxConfig 初始化值：IFG + 不限 DMA burst（r8169 常用值）。
const TX_CONFIG_VALUE: u32 = 0x0300_0700;
/// RxConfig：RXFTH=0b111（FIFO 阈值最大）+ 收广播 / 多播 / 本机 / 全部物理地址。
const RX_CONFIG_VALUE: u32 = 0x0000_E000 | 0x0000_000F;
/// 芯片版本字段：TxConfig[30:20]。
const CHIP_ID_MASK: u32 = 0x7CF0_0000;
const CHIP_ID_SHIFT: u32 = 20;

// ===========================================================================
// DMA 环与缓冲布局（本驱动自己决定，内核不参与）
// ===========================================================================

const PAGE: u64 = 4096;
/// RX/TX 环各 64 个描述符（任务要求）。
const Q_SIZE: u64 = 64;
/// 每个描述符 16 字节。
const DESC_SZ: u64 = 16;
/// 每个 RX 缓冲 1536 字节（足以容纳 1518 字节以太帧 + FCS，且 24 页整除）。
const RX_BUF_SZ: u64 = 1536;

const RX_RING_PAGE: u64 = 0;
const TX_RING_PAGE: u64 = 1;
const RX_BUF_PAGE: u64 = 2;
const RX_BUF_PAGES: u64 = Q_SIZE * RX_BUF_SZ / PAGE; // 64 × 1536 / 4096 = 24
const TX_BUF_PAGE: u64 = RX_BUF_PAGE + RX_BUF_PAGES; // 26
/// 需要的物理连续 DMA 页数（RX 环 1 + TX 环 1 + RX 缓冲 24 + TX 缓冲 1）。
const DMA_PAGES: u64 = TX_BUF_PAGE + 1; // 27

// ---------------------------------------------------------------------------
// 打印助手
// ---------------------------------------------------------------------------

const HEX: &[u8; 16] = b"0123456789abcdef";
fn put_byte_hex(b: u8) {
    let d = [HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]];
    print(unsafe { core::str::from_utf8_unchecked(&d) });
}
/// 按 `aa:bb:cc:dd:ee:ff` 打印 MAC（`mac` 的第 0 字节 = 帧里的首字节）。
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

// ---------------------------------------------------------------------------
// 芯片变体
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChipFamily {
    /// RTL8169 / 8110（classic，无 C+ 寄存器）。
    Rtl8169,
    /// RTL8168B/C（早期 8168，C+ 寄存器存在）。
    Rtl8168Early,
    /// RTL8168D/E（含 E-VL）。
    Rtl8168E,
    /// RTL8168F/G/H（后期 PCIe 变体）。
    Rtl8168Modern,
}

impl ChipFamily {
    fn name(self) -> &'static str {
        match self {
            ChipFamily::Rtl8169 => "RTL8169/8110",
            ChipFamily::Rtl8168Early => "RTL8168B/C",
            ChipFamily::Rtl8168E => "RTL8168D/E",
            ChipFamily::Rtl8168Modern => "RTL8168F/G/H",
        }
    }
}

/// 按 `TxConfig[30:20]` 的版本 ID 归类；认不出返回 `None`（调用方安全退出）。
fn classify(chip_id: u32) -> Option<ChipFamily> {
    match chip_id {
        0x00..=0x0F => Some(ChipFamily::Rtl8169),
        0x10..=0x1F => Some(ChipFamily::Rtl8168Early),
        0x20..=0x2F => Some(ChipFamily::Rtl8168E),
        0x30..=0x3F => Some(ChipFamily::Rtl8168Modern),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 驱动状态
// ---------------------------------------------------------------------------

enum AttachError {
    /// BAR / DMA 窗口不足，或页大小不是 4096。
    BadWindow,
    /// `TxConfig` 版本字段认不出（不写任何寄存器）。
    UnknownChip(u32),
    /// 软复位超时。
    ResetTimeout,
    /// IDR0..5 读不到有效 MAC。
    NoMac,
}

/// 一台 RTL8168/8111：寄存器窗口 + DMA 窗口 + 环游标。
struct Nic {
    bar: u64,
    page: u64,
    dma_vaddr: u64,
    dma_paddr: u64,
    mac: u64,
    chip_id: u32,
    family: ChipFamily,
    /// 下一个要提交的 TX 描述符下标。
    tx_tail: u64,
    /// 下一个要检查/回收的 RX 描述符下标。
    rx_next: u64,
}

fn idle() -> ! {
    loop {
        sys_sleep(500);
    }
}

/// 从 IDR0..IDR5 读 MAC（复位后仍由硬件按 EEPROM/OTP 装载）。
fn read_mac(bar: u64) -> u64 {
    let mut mac = 0u64;
    let mut i = 0u64;
    while i < 6 {
        mac |= (rd8(bar + REG_IDR0 + i) as u64) << (8 * i);
        i += 1;
    }
    mac
}

/// MAC 是否可用（非全 0、非全 1）。
fn mac_ok(mac: u64) -> bool {
    let m = mac & 0xFFFF_FFFF_FFFF;
    m != 0 && m != 0xFFFF_FFFF_FFFF
}

/// 软复位：先停收发，解锁配置寄存器后置 `CmdReset`，等它自清。超时返回 `false`。
fn soft_reset(bar: u64) -> bool {
    wr8(bar + REG_CHIP_CMD, 0);
    wr8(bar + REG_CFG9346, CFG9346_UNLOCK);
    wr8(bar + REG_CHIP_CMD, CMD_RESET);
    let mut spins = 0u32;
    while (rd8(bar + REG_CHIP_CMD) & CMD_RESET) != 0 {
        spins += 1;
        if spins > 1_000_000 {
            return false;
        }
    }
    true
}

/// 各家族共用的保守初始化：关中断、清多播表、写 TX/RX 配置、promiscuous 收包。
/// 调用前必须已解锁 Cfg9346；调用方在环配置完成后加锁。
fn hw_init(bar: u64, family: ChipFamily) {
    wr16(bar + REG_INTR_MASK, 0);
    wr16(bar + REG_INTR_STATUS, 0xFFFF);

    // 8168 系才有 C+ 寄存器：只打开 PCIDAC（描述符用 64 位物理地址），其余 offload
    // （VLAN / 校验和）保持关闭 —— 最保守的裸帧路径。8169/8110 没有该寄存器，跳过以免
    // 写到保留地址。
    if family != ChipFamily::Rtl8169 {
        wr16(bar + REG_CPLUS_CMD, CPLUS_PCIDAC);
    }

    // 多播哈希清零（当前 promiscuous，不依赖过滤，但避免残留旧哈希）。
    let mut i = 0u64;
    while i < 8 {
        wr32(bar + REG_MAR0 + i * 4, 0);
        i += 1;
    }

    wr32(bar + REG_TX_CONFIG, TX_CONFIG_VALUE);
    wr32(bar + REG_RX_CONFIG, RX_CONFIG_VALUE);
    wr16(bar + REG_RX_MAX_SIZE, RX_BUF_SZ as u16);
}

impl Nic {
    /// 识别芯片 → 复位 → 配置 RX/TX 环 → 使能收发。失败返回具体原因。
    fn attach(g: &DeviceGrant) -> Result<Nic, AttachError> {
        let page = g.page();
        if page != PAGE || g.bar_vaddr == 0 || g.dma_bytes < DMA_PAGES * PAGE {
            return Err(AttachError::BadWindow);
        }
        let bar = g.bar_vaddr;

        // 1. 先只读识别芯片版本；认不出就直接退出，绝不写寄存器。
        let chip_id = (rd32(bar + REG_TX_CONFIG) & CHIP_ID_MASK) >> CHIP_ID_SHIFT;
        let family = match classify(chip_id) {
            Some(f) => f,
            None => return Err(AttachError::UnknownChip(chip_id)),
        };

        // 2. 软复位。
        if !soft_reset(bar) {
            return Err(AttachError::ResetTimeout);
        }

        // 3. 复位后读 MAC（IDR0..5 由硬件按配置装载）。
        let mac = read_mac(bar);
        if !mac_ok(mac) {
            return Err(AttachError::NoMac);
        }

        // 4. 配置寄存器（hw_init 解锁了 Cfg9346，环配置完再上锁）。
        hw_init(bar, family);

        let nic = Nic {
            bar,
            page,
            dma_vaddr: g.dma_vaddr,
            dma_paddr: g.dma_paddr,
            mac,
            chip_id,
            family,
            tx_tail: 0,
            rx_next: 0,
        };

        nic.setup_rx_ring();
        nic.setup_tx_ring();

        // 5. 加锁配置寄存器，然后使能 RX/TX（顺序与 r8169 驱动一致）。
        wr8(bar + REG_CFG9346, CFG9346_LOCK);
        fence();
        wr8(bar + REG_CHIP_CMD, CMD_TX_EN | CMD_RX_EN);
        fence();

        Ok(nic)
    }

    /// RX 环：每个描述符指向独立缓冲，opts1 先写好缓冲长度并交给网卡。
    /// RTL8168 没有 RX tail 寄存器，网卡按环连续回卷。
    fn setup_rx_ring(&self) {
        let rx_ring = self.dma_vaddr + RX_RING_PAGE * self.page;
        let rx_ring_pa = self.dma_paddr + RX_RING_PAGE * self.page;
        let mut i = 0u64;
        while i < Q_SIZE {
            let d = rx_ring + i * DESC_SZ;
            let buf_pa = self.dma_paddr + RX_BUF_PAGE * self.page + i * RX_BUF_SZ;
            wr32(d, DESC_OWN | RX_BUF_SZ as u32);
            wr32(d + 4, 0);
            wr32(d + 8, buf_pa as u32);
            wr32(d + 12, (buf_pa >> 32) as u32);
            i += 1;
        }
        fence();
        wr32(self.bar + REG_RDSAR, rx_ring_pa as u32);
        wr32(self.bar + REG_RDSAR_HI, (rx_ring_pa >> 32) as u32);
    }

    /// TX 环：64 个描述符都指向同一个 TX 缓冲（发送时逐帧复用，等完成再发下一帧）。
    /// RTL8168 没有 TDT，提交后写 `TxPoll.NPQ` 通知网卡。
    fn setup_tx_ring(&self) {
        let tx_ring = self.dma_vaddr + TX_RING_PAGE * self.page;
        let tx_ring_pa = self.dma_paddr + TX_RING_PAGE * self.page;
        let tx_buf_pa = self.dma_paddr + TX_BUF_PAGE * self.page;
        let mut i = 0u64;
        while i < Q_SIZE {
            let d = tx_ring + i * DESC_SZ;
            wr32(d, 0); // opts1：OWN=0，尚未交给网卡
            wr32(d + 4, 0);
            wr32(d + 8, tx_buf_pa as u32);
            wr32(d + 12, (tx_buf_pa >> 32) as u32);
            i += 1;
        }
        fence();
        wr32(self.bar + REG_TNPDS, tx_ring_pa as u32);
        wr32(self.bar + REG_TNPDS_HI, (tx_ring_pa >> 32) as u32);
    }

    fn mac(&self) -> u64 {
        self.mac
    }

    /// 发一帧：拷进 TX 缓冲 → 填描述符 → 写 NPQ → 轮询 OWN 位等完成。
    fn send_frame(&mut self, frame: &[u8]) -> bool {
        if frame.is_empty() || frame.len() as u64 > NET_FRAME_MAX {
            return false;
        }
        // 上一帧若还挂在网卡手里（唯一 TX 缓冲），不要覆盖它。
        let idx = self.tx_tail;
        let d = self.dma_vaddr + TX_RING_PAGE * self.page + idx * DESC_SZ;
        if (rd32(d) & DESC_OWN) != 0 {
            return false;
        }

        let tx_va = self.dma_vaddr + TX_BUF_PAGE * self.page;
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), tx_va as *mut u8, frame.len());
        }
        fence();

        let pa = self.dma_paddr + TX_BUF_PAGE * self.page;
        wr32(d + 4, 0); // opts2：不做 offload
        wr32(d + 8, pa as u32);
        wr32(d + 12, (pa >> 32) as u32);
        fence();
        // opts1 最后写：置 OWN + 首尾分段 + 帧长，把描述符交给网卡。
        wr32(
            d,
            DESC_OWN | TX_FIRST_FRAG | TX_LAST_FRAG | frame.len() as u32,
        );
        fence();
        wr8(self.bar + REG_TX_POLL, TX_POLL_NPQ);

        // 等网卡消费完（OWN 清 0），保证单缓冲可复用。
        let mut spins = 0u32;
        loop {
            if (rd32(d) & DESC_OWN) == 0 {
                self.tx_tail = (idx + 1) % Q_SIZE;
                return true;
            }
            spins += 1;
            if spins > 10_000_000 {
                return false;
            }
        }
    }

    /// 收一帧：检查当前 RX 描述符，有帧则拷进 `dst` 并归还描述符；无帧返回 `None`。
    fn poll_rx(&mut self, dst: &mut [u8]) -> Option<usize> {
        let idx = self.rx_next;
        let d = self.dma_vaddr + RX_RING_PAGE * self.page + idx * DESC_SZ;
        let opts1 = rd32(d);
        if (opts1 & DESC_OWN) != 0 {
            return None; // 网卡还没写回来
        }
        let len = (opts1 & 0xFFFF) as usize;
        let err = (opts1 & RX_DESC_ERR) != 0;

        // 无论好坏都先归还描述符（重置 OWN + 长度），推进软游标。
        wr32(d + 4, 0);
        wr32(d, DESC_OWN | RX_BUF_SZ as u32);
        fence();
        self.rx_next = (idx + 1) % Q_SIZE;

        if err || len == 0 || len as u64 > RX_BUF_SZ {
            return None; // 坏帧 / 空帧：丢弃（已回收）
        }
        let n = if len > dst.len() { dst.len() } else { len };
        let src = self.dma_vaddr + RX_BUF_PAGE * self.page + idx * RX_BUF_SZ;
        unsafe {
            core::ptr::copy_nonoverlapping(src as *const u8, dst.as_mut_ptr(), n);
        }
        Some(n)
    }
}

// ---------------------------------------------------------------------------
// 帧级 IPC 服务（与 intel_nic.rs 同一契约）
// ---------------------------------------------------------------------------

/// 服务帧级 IPC 请求：`TX`(发帧) / `RX`(收帧) / `INFO`(取 MAC)。非阻塞，取到空邮箱为止。
fn serve_net_ipc(nic: &mut Nic) {
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
        if msg.tag != NET_REQ_TAG {
            let _ = sys_reply(0);
            continue;
        }
        let req: NetReq =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const NetReq) };
        let reply = match req.op {
            NET_OP_TX => ipc_tx(nic, &req),
            NET_OP_RX => ipc_rx(nic, &req),
            NET_OP_INFO => nic.mac(),
            _ => 0,
        };
        let _ = sys_reply(reply);
    }
}

/// 把共享页里的裸以太帧发出。
fn ipc_tx(nic: &mut Nic, req: &NetReq) -> u64 {
    if req.buf == 0 || req.len == 0 || req.len > NET_FRAME_MAX || sys_virt_to_phys(req.buf) == 0 {
        return 0;
    }
    let frame = unsafe { core::slice::from_raw_parts(req.buf as *const u8, req.len as usize) };
    if nic.send_frame(frame) {
        1
    } else {
        0
    }
}

/// 排空 RX 取一帧写进共享页；回复帧长（无帧 0）。
fn ipc_rx(nic: &mut Nic, req: &NetReq) -> u64 {
    if req.buf == 0 || sys_virt_to_phys(req.buf) == 0 {
        return 0;
    }
    let dst =
        unsafe { core::slice::from_raw_parts_mut(req.buf as *mut u8, NET_FRAME_MAX as usize) };
    nic.poll_rx(dst).map_or(0, |n| n as u64)
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 驱动入口：从**多实例槽 0** 读授权（内核须用 `grant_slot(.., 0)` 接线），
/// 初始化失败则打印原因并 `idle()`，成功则打印一行 bring-up 日志后服务帧级 IPC。
pub fn run() {
    let g = DeviceGrant::load_at(0);
    if !g.is_valid() {
        println("r8169: no device grant in slot 0, idle");
        idle();
    }

    let mut nic = match Nic::attach(&g) {
        Ok(n) => n,
        Err(AttachError::UnknownChip(id)) => {
            print("r8169: unsupported chip variant (TxConfig id=0x");
            print_hex(id as u64);
            println("), not touching registers, idle");
            idle();
        }
        Err(AttachError::BadWindow) => {
            print("r8169: bad BAR/DMA window (need ");
            print_u64(DMA_PAGES);
            println(" contiguous 4 KiB DMA pages), idle");
            idle();
        }
        Err(AttachError::ResetTimeout) => {
            println("r8169: soft reset timeout, idle");
            idle();
        }
        Err(AttachError::NoMac) => {
            println("r8169: invalid MAC (IDR0..5), idle");
            idle();
        }
    };

    print("r8169: up, chip=0x");
    print_hex(nic.chip_id as u64);
    print(" (");
    print(nic.family.name());
    print("), BAR0=0x");
    print_hex(g.bar_paddr);
    print(", MAC=");
    print_mac(nic.mac());
    println("");

    loop {
        serve_net_ipc(&mut nic);
        sys_sleep(10);
    }
}
