//! 域 19 — USB 存储驱动 `xhci_srv`（驱动路线 **D4 续 03c**：xHCI / USB 存储）。
//!
//! 目的：让 **U 盘**也能像 NVMe / SATA 一样经 `block_srv` 的卷层对外提供 —— 用同一套
//! **通用设备授权**（声明式 PCI 查找 + BAR 映射 + 物理连续 DMA）驱动一台新类型控制器，
//! 全程不改内核设备逻辑。
//!
//! **链路**：xHCI 控制器复位 → 端口复位 → Enable Slot → Address Device → 取设备/配置描述符
//! → SET_CONFIGURATION → Configure Endpoint（两条 Bulk）→ USB 存储 **BOT + SCSI 透明命令集**
//! （INQUIRY / READ CAPACITY(10) / READ(10) /（写）WRITE(10) / REQUEST SENSE）。
//!
//! **中断策略**：本仓库只有 MSI-X 一条中断通路，而 xHCI 常态用 INTx/MSI —— 故与 [`crate::ahci_srv`]
//! 一致**全轮询**（轮询事件环 `ERDP`，有界 `POLL_MAX` + `sys_sleep`），不申请向量
//! （内核声明里 `msix_vectors = 0`），`irq_cmds == cmds` / `poll_cmds = 0` 判据保持不变。
//!
//! ⚠️ xHCI 的命令环 / 事件环 / 传输环 + TRB（cycle bit、Link TRB 回绕）与**上下文
//! 字段位偏移**（Slot 的 Root Hub Port Number 在 DWORD1 bits 23:16；EP 的 MPS / EP Type /
//! CErr 全在 DWORD1）是首版最易错处，调试时逐字段对照 `xhci.h` 与 QEMU trace 最有效。

use crate::common::*;
use libdevice::grant::DeviceGrant;
use libdevice::mmio::{fence, rd32, rd8, wr32, wr64};
use morion::syscall::*;

const PAGE: u64 = 4096;

// ---------------------------------------------------------------------------
// Capability 寄存器（MMIO + 0）
// ---------------------------------------------------------------------------
const CAP_CAPLENGTH: u64 = 0x00; // u8
const CAP_HCSPARAMS1: u64 = 0x04;
const CAP_HCSPARAMS2: u64 = 0x08;
const CAP_HCCPARAMS1: u64 = 0x10;
const CAP_DBOFF: u64 = 0x14;
const CAP_RTSOFF: u64 = 0x18;

// ---------------------------------------------------------------------------
// 操作寄存器（MMIO + CAPLENGTH）
// ---------------------------------------------------------------------------
const OP_USBCMD: u64 = 0x00;
const OP_USBSTS: u64 = 0x04;
const OP_CRCR: u64 = 0x18;
const OP_DCBAAP: u64 = 0x30;
const OP_CONFIG: u64 = 0x38;
const OP_PORT_BASE: u64 = 0x400;
const OP_PORT_STRIDE: u64 = 0x10;

const CMD_RS: u32 = 1 << 0;
const CMD_HCRST: u32 = 1 << 1;
const STS_HCH: u32 = 1 << 0;
const STS_CNR: u32 = 1 << 11;

// xHCI 扩展能力: "USB Legacy Support" (xECP id = 1)
/// xECP id = 1 即 "USB Legacy Support"。
const LEGACY_SUP_ID: u32 = 1;
/// USBLEGSUP bit16 = BIOS owned, bit24 = OS owned。
const LEGACY_BIOS_OWNED: u32 = 1 << 16;
const LEGACY_OS_OWNED: u32 = 1 << 24;

// PORTSC 位
const PORT_CCS: u32 = 1 << 0;
const PORT_PED: u32 = 1 << 1;
const PORT_PR: u32 = 1 << 4;
/// 端口链路状态 PLS (bits 8:5): 0 = U0 (正常), 7 = Polling (USB2 复位中), 8 = Recovery。
const PORT_PLS_MASK: u32 = 0xF << 5;
const PORT_PP: u32 = 1 << 9;
/// LWS (bit16): 只有置 1 时写 PLS 才生效。**本驱动永远不置这个位** ——
/// 顺手带上它等于「请求把链路切到读回的那个 PLS」, 真机会把正在做复位/训练的
/// 端口硬拽进 Recovery(8), 之后 SET_ADDRESS 收不到应答 (Address Device cc=4)。
const PORT_LWS: u32 = 1 << 16;
/// CSC = bit17、PRC = bit21（⚠️ 不是 16 / 20 —— 差一位就会写成 LWS / OCC）。
const PORT_CSC: u32 = 1 << 17;
const PORT_PRC: u32 = 1 << 21;
/// Warm Port Reset (bit31)：**只有它能把已退回 USB2 的 USB3 设备重新拉回 SuperSpeed**。
/// 普通 Port Reset 只会在当前速率上复位。USB2 端口上写这一位无效，故可以无脑先试。
const PORT_WPR: u32 = 1 << 31;
/// SuperSpeed 端口在控制器 `RS=1` 后要经历 RxDetect → Polling → U0，
/// 真机实测几百毫秒内 `CCS` 才会置 1。控制器刚启动就普查会把 SS 口误判成“没插设备”。
const LINK_POLL_MS: u32 = 10;
/// 一个控制器都没看到设备时最多等这么久（避免 13 台空控制器拖慢启动）。
const LINK_WAIT_EMPTY_MS: u32 = 250;
/// 至少看到一个设备后，再给同控制器的其它端口留这么久（USB3 训练慢）。
const LINK_WAIT_MIN_MS: u32 = 600;
/// 硬上限。
const LINK_WAIT_MAX_MS: u32 = 1200;

// ---------------------------------------------------------------------------
// 运行时寄存器（MMIO + RTSOFF；interrupter 0 = +0x20）
// ---------------------------------------------------------------------------
const RT_IR0: u64 = 0x20;
const IR_ERSTSZ: u64 = 0x08;
/// ⚠️ ERSTBA 在 0x10、ERDP 在 0x18（xHCI 规范 Table 5-30）—— **极易记反**。
/// 写反的现象：ERDP 实际落到 ERSTBA 上，事件环段表被指向垃圾地址 → QEMU 置 HCE，
/// 此后不再投递任何事件（Enable Slot 命令照跑，但拿不到 Completion Event）。
const IR_ERSTBA: u64 = 0x10;
const IR_ERDP: u64 = 0x18;
/// ERDP 的 EHB（Event Handler Busy，bit3）：写 1 清「有事件待处理」。
const ERDP_EHB: u64 = 1 << 3;

// ---------------------------------------------------------------------------
// TRB 类型（control 的 bits 15:10）
// ---------------------------------------------------------------------------
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
/// Evaluate Context（用于按 bMaxPacketSize0 修正 EP0 的包长）。
const TRB_EVALUATE_CONTEXT: u32 = 13;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_CMD_COMPLETION: u32 = 33;
const TRB_PORT_STATUS_CHANGE: u32 = 34;

/// 完成码：1 = Success，13 = Short Packet（都算成功）。
const CC_SUCCESS: u32 = 1;
const CC_SHORT_PACKET: u32 = 13;

/// 一个 TRB（16 字节）。
#[repr(C)]
#[derive(Clone, Copy)]
struct Trb {
    param: u64,
    status: u32,
    control: u32,
}

/// 把 `control` 里的 TRB 类型取出来。
fn trb_type(t: &Trb) -> u32 {
    (t.control >> 10) & 0x3F
}
/// 把 `status` 高 8 位的完成码取出来。
fn trb_cc(t: &Trb) -> u32 {
    (t.status >> 24) & 0xFF
}

// ---------------------------------------------------------------------------
// DMA 页布局（内核授权 16 页；每页 4 KiB）
// ---------------------------------------------------------------------------
const DMA_PAGES: u64 = 16;
const ERST_PAGE: u64 = 0;
const EVT_PAGE: u64 = 1;
const CMD_PAGE: u64 = 2;
const DCBAA_PAGE: u64 = 3;
const INPUT_PAGE: u64 = 4;
const DEVCTX_PAGE: u64 = 5;
const EP0_PAGE: u64 = 6;
const BIN_PAGE: u64 = 7;
const BOUT_PAGE: u64 = 8;
const DATA_PAGE: u64 = 9;
const BOT_PAGE: u64 = 10;
const SCRATCH_ARR_PAGE: u64 = 11;
const SCRATCH_PAGE0: u64 = 12; // 12..16 共 4 页作 scratchpad

/// 每条环的 TRB 数（末位留作 Link TRB，故可用 63 个）。
const RING_TRBS: usize = 64;

/// 轮询上限（每次让出 1 tick ≈ 1 ms，故上界 ≈ 2 s）。
const POLL_MAX: u32 = 2000;

/// 宿主造 USB 测试盘时写进扇区 0 的签名（自测据此确认"读到的确实是那块盘"）。
const SIG: [u8; 16] = *b"MORION-USB-TST!!";

/// SCSI LBA 的字节数（逻辑块 512 B）。
const SECTOR: u32 = 512;

// ---------------------------------------------------------------------------
// 环（命令环 / 传输环）
// ---------------------------------------------------------------------------

/// 一条生产者环（软件写、控制器读）。末位是 Link TRB（指回环首、TC=1）。
struct Ring {
    va: u64,
    pa: u64,
    enq: usize,
    pcs: bool,
}

impl Ring {
    fn new(va: u64, pa: u64) -> Self {
        let mut r = Ring {
            va,
            pa,
            enq: 0,
            pcs: true,
        };
        r.write_link();
        r
    }

    /// 写末位 Link TRB（带当前生产者周期位）。
    fn write_link(&mut self) {
        let link = Trb {
            param: self.pa,
            status: 0,
            control: (TRB_LINK << 10) | (1 << 1) | u32::from(self.pcs),
        };
        unsafe {
            core::ptr::write_volatile((self.va + ((RING_TRBS - 1) * 16) as u64) as *mut Trb, link);
        }
    }

    /// 下一个待取 TRB 的物理地址（含 DCS）—— Evaluate Context 要交回**当前**位置，
    /// 不能写环首，否则控制器会把已执行过的 TRB 再跑一遍。
    fn deq_pa(&self) -> u64 {
        (self.pa + (self.enq as u64) * 16) | u64::from(self.pcs)
    }

    /// 压入一个 TRB（自动补 cycle bit、处理回绕）。
    fn push(&mut self, mut t: Trb) {
        if self.enq == RING_TRBS - 1 {
            self.write_link();
            self.pcs = !self.pcs;
            self.enq = 0;
        }
        t.control = (t.control & !1) | u32::from(self.pcs);
        unsafe {
            core::ptr::write_volatile((self.va + (self.enq * 16) as u64) as *mut Trb, t);
        }
        self.enq += 1;
    }
}

/// 事件环（控制器写、软件读）。
struct EventRing {
    va: u64,
    pa: u64,
    deq: usize,
    ccs: bool,
}

impl EventRing {
    fn new(va: u64, pa: u64) -> Self {
        EventRing {
            va,
            pa,
            deq: 0,
            ccs: true,
        }
    }

    /// 试着取一个事件（cycle 不匹配即无事件）。取到后推进 dequeue 并写 ERDP。
    fn poll(&mut self, erdp: u64) -> Option<Trb> {
        let slot = self.va + (self.deq * 16) as u64;
        let t = unsafe { core::ptr::read_volatile(slot as *const Trb) };
        let cyc = (t.control & 1) != 0;
        if cyc != self.ccs {
            return None;
        }
        self.deq += 1;
        if self.deq >= RING_TRBS {
            self.deq = 0;
            self.ccs = !self.ccs;
        }
        // 写 ERDP 通知控制器「事件已消费」（EHB 写 1 清）。
        let deq_pa = self.pa + (self.deq * 16) as u64;
        wr64(erdp, deq_pa | ERDP_EHB);
        Some(t)
    }
}

// ---------------------------------------------------------------------------
// 驱动状态
// ---------------------------------------------------------------------------

/// `bulk_xfer` 选择哪条传输环。
const EP_EP0: u8 = 0;
const EP_BULK_IN: u8 = 1;
const EP_BULK_OUT: u8 = 2;

struct Xhci {
    mmio: u64,
    op: u64,
    rt: u64,
    db: u64,
    dma_pa: u64,

    cmd: Ring,
    evt: EventRing,
    ep0: Ring,
    bulkin: Ring,
    bulkout: Ring,

    dcbaa_va: u64,
    dcbaa_pa: u64,
    input_va: u64,
    input_pa: u64,
    devctx_pa: u64,
    data_va: u64,
    data_pa: u64,
    bot_va: u64,
    bot_pa: u64,

    ctx_size: u64,
    max_slots: u32,
    num_ports: u32,

    slot: u32,
    /// Address Device 时写进输入上下文的 slot 上下文（8 个 DWORD）。
    /// Configure Endpoint 必须原样回填 —— xHC 会整份替换自己的 slot 上下文，
    /// 其中 **Device Address (DWORD3 bits 7:0)** 被清 0 后，后续所有传输都会发往
    /// 地址 0（真机全部 cc=4；QEMU 不校验所以看不出来）。
    slot_ctx: [u32; 8],
    bulkin_dci: u32,
    bulkout_dci: u32,
    bulkin_mps: u16,
    bulkout_mps: u16,
    tag: u32,
}

/// 按 ASCII 打印一段定长字节（用于打签名）。
fn print_sig(sig: &[u8; 16]) {
    print(unsafe { core::str::from_utf8_unchecked(&sig[..]) });
}

/// 永不返回的保活循环（无设备 / 初始化失败时用）。
fn idle() -> ! {
    loop {
        sys_sleep(500);
    }
}

/// 把一段缓冲拷到另一段（两段分属不同页，不会重叠）。
fn copy_buf(dst: u64, src: u64, bytes: usize) {
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, bytes);
    }
}

/// 只在首次错误时打一行（避免刷屏）。
static mut ERR_LOGGED: bool = false;
fn err_once(msg: &str) {
    unsafe {
        if ERR_LOGGED {
            return;
        }
        ERR_LOGGED = true;
    }
    print("xhci: ");
    println(msg);
}

// ---------------------------------------------------------------------------
// PORTSC 读写辅助
// ---------------------------------------------------------------------------

/// 端口是否已可用：Enabled (PED=1) 且链路在 U0 (PLS=0)。
fn port_ready(sc: u32) -> bool {
    sc & PORT_PED != 0 && sc & PORT_PLS_MASK == 0
}

/// 安全写 PORTSC：只把 `bits` 里给的位写 1，其余位按读回值写。
///
/// 必须回避的三个「写 1 有副作用」的位：
/// - **PED (bit1) 写 1 = 禁用端口**，绝不能把读到的 1 原样写回；
/// - **LWS (bit16) 写 1 = 允许改 PLS**，配合读回的 PLS 相当于「请求切到当前链路状态」，
///   会打断正在进行的端口复位/训练（真机 pls=7 → 8 就是它干的）；
/// - **PR (bit4) 写 1 = 发起复位**，只在明确要复位时由调用方放进 `bits`。
fn port_write(reg: u64, sc: u32, bits: u32) {
    wr32(
        reg,
        (sc & !(PORT_PED | PORT_PR | PORT_LWS | PORT_PLS_MASK)) | bits,
    );
}

/// 等端口建立链路：PED=1 且 PLS=U0。超时 / 设备被拔掉返回 `None`。
fn wait_ready(reg: u64, ticks: u32) -> Option<u32> {
    let mut t = 0u32;
    while t < ticks {
        let sc = rd32(reg);
        if sc & PORT_CCS == 0 {
            return None;
        }
        if port_ready(sc) {
            return Some(sc);
        }
        sys_sleep(1);
        t += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// 控制器初始化
// ---------------------------------------------------------------------------

impl Xhci {
    /// 打印一行调试信息（带步骤名）。
    fn log(&self, step: &str) {
        print("xhci: ");
        println(step);
    }

    /// 在事件环上等一个满足条件的事件（带总超时）。
    ///
    /// `kind` = 期望的 TRB 类型；传输事件还要匹配 `slot`/`dci`。无关事件（如端口状态
    /// 变化）直接丢弃。超时返回 `None`。
    fn wait_event(&mut self, kind: u32, slot: u32, dci: u32) -> Option<Trb> {
        let erdp = self.rt + RT_IR0 + IR_ERDP;
        let mut ticks = 0u32;
        let mut iters = 0u32;
        while ticks < POLL_MAX && iters < 100_000 {
            iters += 1;
            match self.evt.poll(erdp) {
                Some(e) => {
                    let ty = trb_type(&e);
                    if ty != kind {
                        continue; // 无关事件（端口状态变化等）丢弃
                    }
                    if kind != TRB_TRANSFER_EVENT {
                        return Some(e);
                    }
                    let eslot = (e.control >> 24) & 0xFF;
                    let edci = (e.control >> 16) & 0x1F;
                    if eslot == slot && edci == dci {
                        return Some(e);
                    }
                }
                None => {
                    sys_sleep(1);
                    ticks += 1;
                }
            }
        }
        None
    }

    /// 复位控制器：停 → HCRST → 等 CNR 清零。
    fn reset(&mut self) -> bool {
        let mut t = 0u32;
        while rd32(self.op + OP_USBSTS) & STS_CNR != 0 && t < POLL_MAX {
            sys_sleep(1);
            t += 1;
        }
        // 先停调度（RS=0）并等 halted。
        let cmd = rd32(self.op + OP_USBCMD);
        wr32(self.op + OP_USBCMD, cmd & !CMD_RS);
        let mut t = 0u32;
        while rd32(self.op + OP_USBSTS) & STS_HCH == 0 && t < POLL_MAX {
            sys_sleep(1);
            t += 1;
        }
        // HCRST：自清位，轮询到 0。
        wr32(self.op + OP_USBCMD, CMD_HCRST);
        let mut t = 0u32;
        while rd32(self.op + OP_USBCMD) & CMD_HCRST != 0 && t < POLL_MAX {
            sys_sleep(1);
            t += 1;
        }
        // 复位后再等控制器就绪。
        let mut t = 0u32;
        while rd32(self.op + OP_USBSTS) & STS_CNR != 0 && t < POLL_MAX {
            sys_sleep(1);
            t += 1;
        }
        rd32(self.op + OP_USBSTS) & STS_CNR == 0
    }

    /// 让 USBCMD.RS=1 并等 HCH 清零（进入运行）。
    fn start(&mut self) {
        let cmd = rd32(self.op + OP_USBCMD);
        wr32(self.op + OP_USBCMD, cmd | CMD_RS);
        let mut t = 0u32;
        while rd32(self.op + OP_USBSTS) & STS_HCH != 0 && t < POLL_MAX {
            sys_sleep(1);
            t += 1;
        }
    }

    /// 接管 "USB Legacy Support" 扩展能力 (xECP id = 1)。
    ///
    /// 固件只要开着 Legacy USB (能从这个 U 盘启动就是证据), BIOS 的 SMI 处理器就仍认为
    /// 端口归它管, 会不断打断/重训链路 —— 现象正是端口一直停在 Polling(7) / Recovery(8),
    /// 随后的 Address Device 报 cc=4 (USB 事务错误)。QEMU 没有 SMI 层, 所以这个坑只有
    /// 真机会踩。规范做法: 置 OS-owned, 等 BIOS-owned 清零, 再关掉 SMI 使能位。
    fn take_legacy(&self) {
        let hcc = rd32(self.mmio + CAP_HCCPARAMS1);
        let mut off = ((hcc >> 16) & 0xFFFF) as u64 * 4;
        let mut guard = 0u32;
        while off != 0 && guard < 16 {
            let val = rd32(self.mmio + off);
            let id = val & 0xFF;
            let next = ((val >> 8) & 0xFF) as u64 * 4;
            if id == LEGACY_SUP_ID {
                print("xhci: legacy usblegsup=0x");
                print_hex(val as u64);
                if val & LEGACY_OS_OWNED == 0 {
                    wr32(self.mmio + off, val | LEGACY_OS_OWNED);
                    let mut t = 0u32;
                    while t < 1000 {
                        if rd32(self.mmio + off) & LEGACY_BIOS_OWNED == 0 {
                            break;
                        }
                        sys_sleep(1);
                        t += 1;
                    }
                    if rd32(self.mmio + off) & LEGACY_BIOS_OWNED == 0 {
                        println(" -> OS owned");
                    } else {
                        println(" -> BIOS still owned (SMI may fight us)");
                    }
                } else {
                    println(" -> already OS owned");
                }
                // 关 SMI 使能: USBLEGCTLSTS 紧跟在 USBLEGSUP 后一个 dword。
                wr32(self.mmio + off + 4, 0);
                return;
            }
            if next == 0 {
                break;
            }
            off += next;
            guard += 1;
        }
        println("xhci: no legacy support cap");
    }

    /// 打印一个端口的 PORTSC 关键字段 (十进制)。
    ///
    /// 真机上没有串口, 用户只能拍照 —— 十进制字段比一串十六进制好认得多,
    /// 也避免"到底哪一位是 PLS"这种歧义。
    fn log_port(&self, p: u32, tag: &str, sc: u32) {
        print("xhci: port ");
        print_u64(p as u64);
        print(" ");
        print(tag);
        print(": ccs=");
        print_u64((sc & PORT_CCS) as u64);
        print(" ped=");
        print_u64(((sc & PORT_PED) >> 1) as u64);
        print(" pp=");
        print_u64(((sc & PORT_PP) >> 9) as u64);
        print(" pls=");
        print_u64(((sc & PORT_PLS_MASK) >> 5) as u64);
        print(" spd=");
        print_u64(((sc >> 10) & 0xF) as u64);
        print(" oca=");
        print_u64(((sc >> 3) & 1) as u64);
        println("");
    }

    /// 手动复位一个端口（`Address Device` 失败后的兜底），返回终态 PORTSC。
    ///
    /// 规范 (xHCI 4.19.5)：只有 PED=0 时才能触发复位；USB3 端口要写 **Warm Port Reset**
    /// （bit31，链路重新训练，能回 SuperSpeed），USB2 端口写 **Port Reset**（bit4）。
    /// 二者不能同时置 1。
    fn reset_port(&self, port: u32) -> u32 {
        let reg = self.op + OP_PORT_BASE + ((port as u64 - 1) * OP_PORT_STRIDE);
        let mut sc = rd32(reg);
        // 禁用前先记住速率：USB3 设备复位后速度字段可能读回 0，事后就分不出来了。
        let speed = (sc >> 10) & 0xF;
        if sc & PORT_PED != 0 {
            port_write(reg, sc, PORT_PED); // 写 PED=1 = 禁用
            let mut t = 0u32;
            while t < 100 && rd32(reg) & PORT_PED != 0 {
                sys_sleep(1);
                t += 1;
            }
        }
        sc = rd32(reg);
        port_write(reg, sc, PORT_CSC | PORT_PRC); // 先清挂起的变化位
        sc = rd32(reg);
        if speed == 4 || speed == 5 {
            port_write(reg, sc, PORT_PP | PORT_WPR);
            let mut t = 0u32;
            while t < 500 && rd32(reg) & PORT_WPR != 0 {
                sys_sleep(1);
                t += 1;
            }
        } else {
            port_write(reg, sc, PORT_PP | PORT_PR);
            let mut t = 0u32;
            while t < 500 && rd32(reg) & PORT_PR != 0 {
                sys_sleep(1);
                t += 1;
            }
        }
        if let Some(sc) = wait_ready(reg, 500) {
            port_write(reg, sc, PORT_CSC | PORT_PRC);
        }
        rd32(reg)
    }

    /// 控制器 `RS=1` 后等各端口把链路训练完（USB3 的 `CCS` 不是立刻置 1）。
    ///
    /// 之前的实现上电后只等 10 ms 就普查，真机上 SS 口还在 `PLS=5 (RxDetect)`、
    /// `CCS=0`，于是被当成“空端口”跳过；同一控制器的 USB2 口先连上，设备名额被它占掉，
    /// U 盘永远轮不到。这里至少等到“连接数不再变化”，并给 SS 口留够时间。
    fn wait_connect(&self) {
        let mut saw = false;
        let mut last = 0u32;
        let mut stable = 0u32;
        let mut t = 0u32;
        while t < LINK_WAIT_MAX_MS {
            let mut n = 0u32;
            let mut training = false;
            let mut p = 1u32;
            while p <= self.num_ports {
                let reg = self.op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
                let sc = rd32(reg);
                if sc & PORT_CCS != 0 {
                    n += 1;
                }
                // USB3 端口在 RxDetect(5)/Polling(7)/Recovery(8) 时链路还没训练完，
                // 不能因为连接数暂时稳定就收工。
                let pls = (sc & PORT_PLS_MASK) >> 5;
                if sc & PORT_CCS == 0 && matches!(pls, 5 | 7 | 8) {
                    training = true;
                }
                p += 1;
            }
            if n > 0 {
                saw = true;
            }
            if n == last {
                stable += LINK_POLL_MS;
            } else {
                last = n;
                stable = 0;
            }
            // 没有设备：很快放弃；有设备：至少等 LINK_WAIT_MIN_MS，再等
            // “连接数稳定 + 没有端口还在训练”。
            if !saw && t >= LINK_WAIT_EMPTY_MS {
                break;
            }
            if saw && t >= LINK_WAIT_MIN_MS && stable >= 150 && !training {
                break;
            }
            sys_sleep(LINK_POLL_MS as u64);
            t += LINK_POLL_MS;
        }
    }

    /// 挑一个已经使能（PED=1 / PLS=U0）的端口，**SuperSpeed 优先**。
    fn pick_ready(&self) -> Option<(u32, u32)> {
        let mut best: Option<(u32, u32)> = None;
        let mut p = 1u32;
        while p <= self.num_ports {
            let reg = self.op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
            let sc = rd32(reg);
            if !port_ready(sc) {
                p += 1;
                continue;
            }
            let speed = (sc >> 10) & 0xF;
            if speed == 4 || speed == 5 {
                port_write(reg, sc, PORT_CSC | PORT_PRC);
                self.log_port(p, "auto-ss", rd32(reg));
                return Some((p, speed));
            }
            if best.is_none() {
                best = Some((p, speed));
            }
            p += 1;
        }
        if let Some((port, speed)) = best {
            let reg = self.op + OP_PORT_BASE + ((port as u64 - 1) * OP_PORT_STRIDE);
            let sc = rd32(reg);
            port_write(reg, sc, PORT_CSC | PORT_PRC);
            self.log_port(port, "auto", rd32(reg));
            return Some((port, speed));
        }
        None
    }

    /// 找到一个可用端口，返回 (端口号, 速度)。
    ///
    /// USB3 连接器在控制器上会占两个 PORTSC（SS 口 + USB2 伴随口），但**不能**用
    /// “`CCS=1 / PED=0 / PLS=4`” 去猜伴随口 —— 刚启动时真正的 SS 口也是这个状态，
    /// 之前正是这条判断把 U 盘所在的 SS 口跳过了。
    ///
    /// 正确顺序：
    /// ① 已有使能端口 → 直接取（SS 优先）；
    /// ② 已连接但未使能的端口 → 先等链路训练，再写 **Warm Reset**（USB3 专属；
    ///    USB2 端口上该位被忽略），SS 一旦就绪立即返回；
    /// ③ 仍不行 → 普通 Port Reset 兜底。
    fn find_port(&self) -> Option<(u32, u32)> {
        // ① 已经训练好的端口。
        if let Some(v) = self.pick_ready() {
            return Some(v);
        }

        // ② 已连接但还没使能：先等，再 Warm Reset 重新训练。
        let mut p = 1u32;
        while p <= self.num_ports {
            let reg = self.op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
            let sc = rd32(reg);
            if sc & PORT_CCS == 0 || port_ready(sc) {
                p += 1;
                continue;
            }
            self.log_port(p, "pre", sc);
            // 先给控制器自己完成 Polling → U0 的时间。
            if let Some(sc) = wait_ready(reg, 300) {
                port_write(reg, sc, PORT_CSC | PORT_PRC);
                let speed = (sc >> 10) & 0xF;
                self.log_port(p, "trained", rd32(reg));
                if speed == 4 || speed == 5 {
                    return Some((p, speed));
                }
                p += 1;
                continue;
            }
            // Warm Reset：USB3 链路重新训练；USB2 端口上写这一位无效（RsvdZ）。
            let sc = rd32(reg);
            port_write(reg, sc, PORT_PP | PORT_WPR);
            let mut t = 0u32;
            while t < 500 && rd32(reg) & PORT_WPR != 0 {
                sys_sleep(1);
                t += 1;
            }
            if let Some(sc) = wait_ready(reg, 600) {
                port_write(reg, sc, PORT_CSC | PORT_PRC);
                let sc = rd32(reg);
                if sc & PORT_PED != 0 {
                    self.log_port(p, "warm", sc);
                    let speed = (sc >> 10) & 0xF;
                    if speed == 4 || speed == 5 {
                        return Some((p, speed));
                    }
                }
            }
            p += 1;
        }

        // ②' 没有 SS 口，退而求其次用任意已使能端口（USB2 设备）。
        if let Some(v) = self.pick_ready() {
            return Some(v);
        }

        // ③ 兜底：普通端口复位（USB2）/ Warm Reset（USB3），最多 3 次。
        let mut p = 1u32;
        while p <= self.num_ports {
            let reg = self.op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
            if rd32(reg) & PORT_CCS == 0 {
                p += 1;
                continue;
            }
            let mut tries = 0u32;
            while tries < 3 {
                self.log_port(p, "retry", self.reset_port(p));
                let sc = rd32(reg);
                if port_ready(sc) {
                    self.log_port(p, "post", sc);
                    return Some((p, (sc >> 10) & 0xF));
                }
                sys_sleep(50);
                tries += 1;
            }
            self.log_port(p, "fail", rd32(reg));
            p += 1;
        }
        None
    }

    /// 发一条无输入上下文的命令（如 Enable Slot），返回 (完成码, 事件)。
    fn simple_cmd(&mut self, trb_type_bits: u32, slot: u32, param: u64) -> Option<Trb> {
        self.cmd.push(Trb {
            param,
            status: 0,
            control: (trb_type_bits << 10) | (slot << 24),
        });
        fence();
        wr32(self.db, 0); // 命令环门铃 = doorbell[0]
        self.wait_event(TRB_CMD_COMPLETION, 0, 0)
    }

    /// 按输入上下文发一条命令（Address Device / Configure Endpoint）。
    fn context_cmd(&mut self, trb_type_bits: u32, slot: u32) -> Option<Trb> {
        self.simple_cmd(trb_type_bits, slot, self.input_pa)
    }

    /// 向某条传输环压一个 Normal TRB、敲门铃并等完成。
    fn bulk_xfer(&mut self, which: u8, pa: u64, len: u32, dir_in: bool) -> bool {
        let dci = match which {
            EP_EP0 => 1,
            EP_BULK_IN => self.bulkin_dci,
            _ => self.bulkout_dci,
        };
        let trb = Trb {
            param: pa,
            status: len & 0x1FFFF,
            control: (TRB_NORMAL << 10) | (1 << 5) | if dir_in { 1 << 16 } else { 0 },
        };
        match which {
            EP_EP0 => self.ep0.push(trb),
            EP_BULK_IN => self.bulkin.push(trb),
            _ => self.bulkout.push(trb),
        }
        fence();
        wr32(self.db + (self.slot as u64) * 4, dci);
        match self.wait_event(TRB_TRANSFER_EVENT, self.slot, dci) {
            Some(e) => {
                let cc = trb_cc(&e);
                cc == CC_SUCCESS || cc == CC_SHORT_PACKET
            }
            None => false,
        }
    }

    /// 一次 EP0 控制传输（setup + 可选 data + status）。`data_pa` 已在本域 DMA 页里。
    ///
    /// 参数就是 USB 控制传输的 7 个字段（bmRequestType/bRequest/wValue/wIndex/wLength/方向/缓冲），
    /// 拆成结构体反而更难对照协议，故此处保留多参数签名。
    #[allow(clippy::too_many_arguments)]
    fn control(
        &mut self,
        bm: u8,
        req: u8,
        value: u16,
        index: u16,
        len: u16,
        dir_in: bool,
        data_pa: u64,
    ) -> bool {
        // Setup Stage：TRT = 1(IN) / 2(OUT) / 0(无数据)。
        let trt = if len == 0 {
            0u32
        } else if dir_in {
            1
        } else {
            2
        };
        let param = (bm as u64)
            | ((req as u64) << 8)
            | ((value as u64) << 16)
            | ((index as u64) << 32)
            | ((len as u64) << 48);
        self.ep0.push(Trb {
            param,
            status: 8,
            control: (TRB_SETUP << 10) | (1 << 6) | (trt << 16),
        });
        if len != 0 {
            self.ep0.push(Trb {
                param: data_pa,
                status: len as u32,
                control: (TRB_DATA << 10) | if dir_in { 1 << 16 } else { 0 },
            });
        }
        // Status Stage：数据阶段为 IN 时状态阶段是 OUT（DIR=0），否则 IN（DIR=1）。
        let status_dir = !(dir_in && len != 0);
        self.ep0.push(Trb {
            param: 0,
            status: 0,
            control: (TRB_STATUS << 10) | if status_dir { 1 << 16 } else { 0 } | (1 << 5),
        });
        fence();
        wr32(self.db + (self.slot as u64) * 4, 1); // DCI 1 = EP0
        match self.wait_event(TRB_TRANSFER_EVENT, self.slot, 1) {
            Some(e) => {
                let cc = trb_cc(&e);
                if cc == CC_SUCCESS || cc == CC_SHORT_PACKET {
                    return true;
                }
                // 真机诊断: 完成码 + 残余长度 (4 = 事务错误, 6 = Stall)
                print("xhci: EP0 req=");
                print_u64(req as u64);
                print(" cc=");
                print_u64(cc as u64);
                print(" residual=");
                print_u64((e.status & 0x00FF_FFFF) as u64);
                println("");
                false
            }
            None => {
                print("xhci: EP0 req=");
                print_u64(req as u64);
                println(" no completion event");
                false
            }
        }
    }

    /// 读设备描述符（前 `want` 字节）——Address Device 后确认链路通。
    ///
    /// 真机上有设备在 SET_ADDRESS 之后要几十毫秒才应答第一个控制传输，一次失败就
    /// 放弃会把整台控制器判死；故先给稳定时间，再重试。
    fn get_device_descriptor(&mut self, want: u16) -> bool {
        sys_sleep(50);
        let mut attempt = 0u32;
        while attempt < 3 {
            if self.control(0x80, 6, 1 << 8, 0, want, true, self.data_pa) {
                return true;
            }
            attempt += 1;
            if attempt < 3 {
                sys_sleep(50);
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// BOT + SCSI
// ---------------------------------------------------------------------------

/// 组装 31 字节的 CBW（Command Block Wrapper）。
fn build_cbw(buf: &mut [u8; 31], tag: u32, data_len: u32, dir_in: bool, cb: &[u8]) {
    buf[0..4].copy_from_slice(&0x4342_5355u32.to_le_bytes()); // "USBC"
    buf[4..8].copy_from_slice(&tag.to_le_bytes());
    buf[8..12].copy_from_slice(&data_len.to_le_bytes());
    buf[12] = if dir_in { 0x80 } else { 0x00 };
    buf[13] = 0; // LUN 0
    buf[14] = cb.len() as u8;
    let mut i = 0usize;
    while i < 16 {
        buf[15 + i] = if i < cb.len() { cb[i] } else { 0 };
        i += 1;
    }
}

/// 解析 13 字节的 CSW（Command Status Wrapper）：(tag, residue, status, 签名是否对)。
fn parse_csw(buf: &[u8; 13]) -> (u32, u32, u8, bool) {
    let sig = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let tag = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let residue = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    (tag, residue, buf[12], sig == 0x5342_5355)
}

/// BOT 结果。
struct BotResult {
    /// CSW 签名 / tag 正确且没被 phase error 打断。
    ok: bool,
    /// CSW 的 bCSWStatus（0 = 命令成功）。
    status: u8,
}

impl Xhci {
    /// 跑一笔完整 BOT：CBW(OUT) → 可选 data → CSW(IN)。
    fn bot_once(&mut self, cb: &[u8], data_len: u32, dir_in: bool) -> BotResult {
        self.tag = self.tag.wrapping_add(1);
        let tag = self.tag;

        // ① CBW（31 字节）经 Bulk-Out 发出。
        let mut cbw = [0u8; 31];
        build_cbw(&mut cbw, tag, data_len, dir_in, cb);
        copy_buf(self.bot_va, cbw.as_ptr() as u64, 31);
        if !self.bulk_xfer(EP_BULK_OUT, self.bot_pa, 31, false) {
            err_once("CBW bulk-out failed");
            return BotResult {
                ok: false,
                status: 0xFF,
            };
        }

        // ② 数据阶段（若命令有数据）。
        if data_len != 0 {
            let (which, len) = if dir_in {
                (EP_BULK_IN, data_len)
            } else {
                (EP_BULK_OUT, data_len)
            };
            if !self.bulk_xfer(which, self.data_pa, len, dir_in) {
                err_once("data bulk transfer failed");
                return BotResult {
                    ok: false,
                    status: 0xFF,
                };
            }
        }

        // ③ CSW（13 字节）从 Bulk-In 读回。
        if !self.bulk_xfer(EP_BULK_IN, self.bot_pa + 64, 13, true) {
            err_once("CSW bulk-in failed");
            return BotResult {
                ok: false,
                status: 0xFF,
            };
        }
        let mut csw = [0u8; 13];
        copy_buf(csw.as_mut_ptr() as u64, self.bot_va + 64, 13);
        let (rtag, _residue, status, sig_ok) = parse_csw(&csw);
        BotResult {
            ok: sig_ok && rtag == tag,
            status,
        }
    }

    /// 发一条 SCSI 命令，自动处理 UNIT ATTENTION（先 REQUEST SENSE 再重试）。
    fn scsi_cmd(&mut self, cb: &[u8], data_len: u32, dir_in: bool) -> bool {
        let mut attempt = 0u32;
        while attempt < 3 {
            let r = self.bot_once(cb, data_len, dir_in);
            if !r.ok {
                return false;
            }
            if r.status == 0 {
                return true;
            }
            // CSW status != 0：取 sense（UNIT ATTENTION 等）后重试。
            self.request_sense();
            attempt += 1;
        }
        false
    }

    /// REQUEST SENSE（0x03）：18 字节，结果丢弃（只为清掉 UNIT ATTENTION）。
    fn request_sense(&mut self) {
        let cb = [0x03u8, 0, 0, 0, 18, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let _ = self.bot_once(&cb, 18, true);
    }

    /// INQUIRY（0x12）：取 36 字节识别信息。
    fn scsi_inquiry(&mut self) -> bool {
        let cb = [0x12u8, 0, 0, 0, 36, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        self.scsi_cmd(&cb, 36, true)
    }

    /// READ CAPACITY(10)（0x25）：返回 (最后 LBA, 块字节数)。
    fn scsi_read_capacity(&mut self) -> Option<(u32, u32)> {
        let cb = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        if !self.scsi_cmd(&cb, 8, true) {
            return None;
        }
        let mut buf = [0u8; 8];
        copy_buf(buf.as_mut_ptr() as u64, self.data_va, 8);
        let last_lba = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let blk = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        Some((last_lba, blk))
    }

    /// READ(10)（0x28）：读 `count` 个扇区到私有数据页。
    fn scsi_read10(&mut self, lba: u32, count: u16) -> bool {
        let mut cb = [0u8; 16];
        cb[0] = 0x28;
        cb[2] = (lba >> 24) as u8;
        cb[3] = (lba >> 16) as u8;
        cb[4] = (lba >> 8) as u8;
        cb[5] = lba as u8;
        cb[7] = (count >> 8) as u8;
        cb[8] = count as u8;
        self.scsi_cmd(&cb, (count as u32) * SECTOR, true)
    }

    /// WRITE(10)（0x2A）：把私有数据页的 `count` 个扇区写入 `lba`（安全门由调用方把关）。
    ///
    /// 写完立刻发 SYNCHRONIZE CACHE(10)：U 盘自己的写缓存不刷到闪存的话，关机/拔盘后
    /// 文件会"写成功但不见了"（真机 `/usb` 空分区的根因之一）。
    fn scsi_write10(&mut self, lba: u32, count: u16) -> bool {
        let mut cb = [0u8; 16];
        cb[0] = 0x2A;
        cb[2] = (lba >> 24) as u8;
        cb[3] = (lba >> 16) as u8;
        cb[4] = (lba >> 8) as u8;
        cb[5] = lba as u8;
        cb[7] = (count >> 8) as u8;
        cb[8] = count as u8;
        if !self.scsi_cmd(&cb, (count as u32) * SECTOR, false) {
            return false;
        }
        // 不支持该命令的盘（少数）只记一次日志，不让整笔写失败。
        if !self.scsi_sync_cache() {
            err_once("SYNCHRONIZE CACHE(10) failed (write may not be durable)");
        }
        true
    }

    /// SYNCHRONIZE CACHE(10)（0x35）：把 U 盘内部写缓存刷到闪存。
    fn scsi_sync_cache(&mut self) -> bool {
        let cb = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        self.scsi_cmd(&cb, 0, false)
    }
}

// ---------------------------------------------------------------------------
// 描述符解析
// ---------------------------------------------------------------------------

/// 一条 Bulk 端点：(端点号, DCI, MaxPacketSize)。
#[derive(Clone, Copy)]
struct BulkEp {
    dci: u32,
    mps: u16,
}

/// 在配置描述符里找接口 0 的两条 Bulk 端点（IN / OUT）。
///
/// 只认 Mass Storage / SCSI / BOT（`bInterfaceClass=08`、subclass `06`、protocol `50`）。
fn parse_bulk_endpoints(buf_va: u64, total: u32) -> Option<(BulkEp, BulkEp)> {
    let mut off = 0u32;
    let mut bin = None;
    let mut bout = None;
    while off + 2 <= total {
        let len = rd8(buf_va + off as u64) as u32;
        let dtype = rd8(buf_va + off as u64 + 1);
        if len < 2 {
            break;
        }
        if dtype == 5 {
            // Endpoint 描述符。
            let addr = rd8(buf_va + off as u64 + 2);
            let attr = rd8(buf_va + off as u64 + 3);
            let mps = (rd8(buf_va + off as u64 + 4) as u16)
                | ((rd8(buf_va + off as u64 + 5) as u16) << 8);
            if attr & 0x3 == 2 {
                let num = (addr & 0x0F) as u32;
                if addr & 0x80 != 0 {
                    bin = Some(BulkEp {
                        dci: 2 * num + 1,
                        mps: mps & 0x7FF,
                    });
                } else {
                    bout = Some(BulkEp {
                        dci: 2 * num,
                        mps: mps & 0x7FF,
                    });
                }
            }
        }
        off += len;
    }
    match (bin, bout) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 主入口（多控制器 + USB 存储 / USB HID 键盘）
// ---------------------------------------------------------------------------

/// 单台控制器的探测结果。
enum ProbeOutcome {
    /// 这台控制器没有可用设备（或初始化失败）：继续看下一台。
    None,
    /// 挂上了 USB 存储，已异步通知 block_srv。
    Storage(StorageDev),
    /// 配好了 USB HID 键盘。
    Keyboard(KeyboardDev),
}

/// 已挂进 block_srv 卷层的 USB 存储控制器。
struct StorageDev {
    hc: Xhci,
    cap_sectors: u64,
}

/// 一台已配置好的 USB HID 键盘控制器。
struct KeyboardDev {
    hc: Xhci,
    dci: u32,
    /// 上一次的 6 个按键码（boot report 的 byte2..7），用于只上报"新按下"。
    prev: [u8; 6],
    /// 是否已压入一笔等待数据的 Interrupt IN 传输。
    armed: bool,
}

impl KeyboardDev {
    /// 在 HID 中断端点上压一笔 8 字节输入传输（无数据时控制器会一直 NAK，不产生完成事件）。
    fn arm(&mut self) {
        let trb = Trb {
            param: self.hc.data_pa,
            status: 8,
            control: (TRB_NORMAL << 10) | (1 << 5) | (1 << 16),
        };
        self.hc.bulkin.push(trb);
        fence();
        wr32(self.hc.db + (self.hc.slot as u64) * 4, self.dci);
        self.armed = true;
    }

    /// 非阻塞收 HID 报告；收到就把新按下的键推进内核键队列，并重新压一笔传输。
    fn poll(&mut self) {
        let erdp = self.hc.rt + RT_IR0 + IR_ERDP;
        let mut got = false;
        while let Some(e) = self.hc.evt.poll(erdp) {
            if trb_type(&e) != TRB_TRANSFER_EVENT {
                continue; // 端口状态变化等无关事件
            }
            let slot = (e.control >> 24) & 0xFF;
            let dci = (e.control >> 16) & 0x1F;
            if slot != self.hc.slot || dci != self.dci {
                continue;
            }
            let mut rep = [0u8; 8];
            let mut i = 0usize;
            while i < 8 {
                rep[i] = rd8(self.hc.data_va + i as u64);
                i += 1;
            }
            self.emit(&rep);
            got = true;
        }
        if got && self.armed {
            self.armed = false;
            self.arm();
        }
    }

    /// boot report → 字符：只上报"这一帧新按下"的键（按住不放不会重复）。
    fn emit(&mut self, rep: &[u8; 8]) {
        let shift = rep[0] & 0x22 != 0; // bit1 左 shift / bit5 右 shift
        let mut i = 0usize;
        while i < 6 {
            let code = rep[2 + i];
            if code != 0 && !self.prev.contains(&code) {
                // 方向键 / 翻页键: 翻内核终端的日志 (真机 SAFE 变体没有串口;
                // 屏幕由内核终端负责时这两组键不属于任何用户程序)。
                match code {
                    0x4B => {
                        sys_console_scroll(20);
                    } // PageUp
                    0x4E => {
                        sys_console_scroll(-20);
                    } // PageDown
                    0x52 => {
                        sys_console_scroll(1);
                    } // ↑
                    0x51 => {
                        sys_console_scroll(-1);
                    } // ↓
                    0x50 => {
                        sys_key_push(0x02); // ← / Ctrl+B
                    }
                    0x4F => {
                        sys_key_push(0x06); // → / Ctrl+F
                    }
                    0x4A => {
                        sys_key_push(0x01); // Home / Ctrl+A
                    }
                    0x4D => {
                        sys_key_push(0x05); // End / Ctrl+E
                    }
                    0x4C => {
                        sys_key_push(0x04); // Delete / Ctrl+D
                    }
                    0x2B => {
                        sys_key_push(0x09); // Tab
                    }
                    _ => {
                        if let Some(c) = hid_char(code, shift) {
                            sys_key_push(c);
                        }
                    }
                }
            }
            i += 1;
        }
        self.prev.copy_from_slice(&rep[2..8]);
    }
}

/// HID usage ID（boot keyboard）→ ASCII；不可打印键返回 `None`。
fn hid_char(code: u8, shift: bool) -> Option<u8> {
    let c = match code {
        0x04..=0x1D => b'a' + (code - 0x04),
        0x1E..=0x27 => b'1' + (code - 0x1E),
        0x28 => return Some(b'\n'),
        0x2A => return Some(0x08),
        0x2C => b' ',
        0x2D => return Some(if shift { b'_' } else { b'-' }),
        0x2E => return Some(if shift { b'+' } else { b'=' }),
        0x2F => return Some(if shift { b'{' } else { b'[' }),
        0x30 => return Some(if shift { b'}' } else { b']' }),
        0x31 => return Some(if shift { b'|' } else { b'\\' }),
        0x32 => return Some(if shift { b':' } else { b';' }),
        0x33 => return Some(if shift { b'"' } else { b'\'' }),
        0x34 => return Some(if shift { b'~' } else { b'`' }),
        0x35 => return Some(if shift { b'<' } else { b',' }),
        0x36 => return Some(if shift { b'>' } else { b'.' }),
        0x37 => return Some(if shift { b'?' } else { b'/' }),
        _ => return None,
    };
    if shift {
        if c.is_ascii_lowercase() {
            return Some(c - 32);
        }
        // 数字行：shift 后是符号。
        return Some(match code {
            0x1E => b'!',
            0x1F => b'@',
            0x20 => b'#',
            0x21 => b'$',
            0x22 => b'%',
            0x23 => b'^',
            0x24 => b'&',
            0x25 => b'*',
            0x26 => b'(',
            0x27 => b')',
            _ => c,
        });
    }
    Some(c)
}

/// 配置描述符里与本驱动有关的接口信息。
#[derive(Clone, Copy, Default)]
struct IfaceInfo {
    /// 第一个接口的类别三元组（只用于诊断打印）。
    class: u8,
    subclass: u8,
    proto: u8,
    /// 接口总数。
    n_ifaces: u32,
    bulk_in: Option<BulkEp>,
    bulk_out: Option<BulkEp>,
    /// HID 接口（class=03 且带中断 IN）：(DCI, MaxPacketSize, Interval)。
    hid: Option<(u32, u16, u8)>,
    /// 这个 HID 是否声称 boot 子类/协议（1/1）。
    hid_boot: bool,
}

/// 遍历配置描述符，收集接口类别与端点。
///
/// ⚠️ 复合设备很常见（键盘带多媒体键、摄像头带麦克风）：端点必须按**所属接口**归类，
/// 不能拿"最后一个接口的类别"去判断整台设备 —— 旧实现会把 HID 键盘判成不支持。
///
/// ⚠️ 只收 **bAlternateSetting = 0** 的端点：`SET_CONFIGURATION` 之后每个接口都回到
/// alt 0，别的 alt 必须显式 `SET_INTERFACE` 才生效。UAS 盘（如 thinkplus 256GB）就是
/// alt0 = BOT(0x50) / alt1 = UAS(0x62)；把 alt1 的 EP3/EP4 当成 Bulk 对，设备根本不在
/// 那两条管子上收包，BOT 的第一条 CBW 就会超时（真机表现 `storage-inquiry-fail`）。
fn parse_config(buf_va: u64, total: u32) -> IfaceInfo {
    let mut info = IfaceInfo::default();
    let mut cur = (0u8, 0u8, 0u8); // 当前接口的 (class, subclass, proto)
    let mut alt0 = true; // 当前接口是否 alt 0（描述符顺序：接口后紧跟它的端点）
    let mut off = 0u32;
    while off + 2 <= total {
        let len = rd8(buf_va + off as u64) as u32;
        let dtype = rd8(buf_va + off as u64 + 1);
        if len < 2 {
            break;
        }
        let base = buf_va + off as u64;
        if dtype == 4 && len >= 9 {
            // Interface descriptor
            cur = (rd8(base + 5), rd8(base + 6), rd8(base + 7));
            alt0 = rd8(base + 3) == 0;
            if alt0 && info.n_ifaces == 0 {
                info.class = cur.0;
                info.subclass = cur.1;
                info.proto = cur.2;
            }
            if alt0 {
                info.n_ifaces += 1;
            }
        } else if dtype == 5 && len >= 7 && alt0 {
            // Endpoint descriptor（只认当前接口的 alt 0）
            let addr = rd8(base + 2);
            let attr = rd8(base + 3);
            let mps = (rd8(base + 4) as u16) | ((rd8(base + 5) as u16) << 8);
            let interval = rd8(base + 6);
            let num = (addr & 0x0F) as u32;
            match attr & 0x3 {
                2 if addr & 0x80 != 0 => {
                    info.bulk_in = Some(BulkEp {
                        dci: 2 * num + 1,
                        mps: mps & 0x7FF,
                    })
                }
                2 => {
                    info.bulk_out = Some(BulkEp {
                        dci: 2 * num,
                        mps: mps & 0x7FF,
                    })
                }
                3 if addr & 0x80 != 0 && cur.0 == 0x03 => {
                    // HID 中断 IN：优先挑 boot 子类/协议的那一个接口。
                    let boot = cur.1 == 0x01 && cur.2 == 0x01;
                    if info.hid.is_none() || (boot && !info.hid_boot) {
                        info.hid = Some((2 * num + 1, mps & 0x7FF, interval));
                        info.hid_boot = boot;
                    }
                }
                _ => {}
            }
        }
        off += len;
    }
    info
}

/// 读 iProduct 字符串描述符并打印其中的 ASCII（"这到底是什么设备"最直接的证据）。
///
/// `idx` 必须由调用方在**读配置描述符之前**从设备描述符里取好 —— 两者共用同一块
/// DMA 缓冲。失败静默跳过（有的设备不实现 0x0409 或没有字符串描述符）。
fn log_product_string(hc: &mut Xhci, idx: u8) {
    if idx == 0 {
        return;
    }
    if !hc.control(
        0x80,
        6,
        (3u16 << 8) | idx as u16,
        0x0409,
        128,
        true,
        hc.data_pa,
    ) {
        return;
    }
    let len = rd8(hc.data_va) as u64;
    if len < 4 {
        return;
    }
    print("xhci: product=\"");
    let mut i = 2u64;
    while i + 1 < len && i < 128 {
        let c = rd8(hc.data_va + i); // UTF-16LE 的低字节；ASCII 足够识别型号
        if (0x20..0x7f).contains(&c) {
            let one = [c];
            print(unsafe { core::str::from_utf8_unchecked(&one) });
        }
        i += 2;
    }
    println("\"");
}

/// 配置 HID 的 Interrupt IN 端点（EP Type = 7）。
fn configure_hid(hc: &mut Xhci, slot: u32, dci: u32, mps: u16, interval: u8) -> bool {
    let (_, ep_base) = clear_input(hc);
    wr32(hc.input_va + 4, 1 | (1 << dci));
    // Slot 上下文原样回填（Device Address 不能丢），只改 Context Entries。
    restore_slot_ctx(hc, dci);
    let ep = input_ep_ctx(hc, ep_base, dci);
    // EP 上下文 DWORD0：Interval 在 bits 23:16。
    wr32(ep, (interval as u32) << 16);
    // DWORD1：MPS | EP Type=7(Interrupt IN) | CErr=3。
    wr32(ep + 4, ((mps as u32) << 16) | (7 << 3) | (3 << 1));
    wr64(ep + 8, hc.bulkin.pa | 1);
    match hc.context_cmd(TRB_CONFIGURE_ENDPOINT, slot) {
        Some(e) => trb_cc(&e) == CC_SUCCESS,
        None => false,
    }
}

/// 每个控制器打印一行**探测结论**（真机跑一次 `dmesg -g probe` 就能看全整机 USB 拓扑）。
///
/// 真机没有串口、也没有 `lsusb`，驱动日志又长又散；这行是给人拍照用的摘要。
fn probe_line(idx: u32, vid: u16, pid: u16, result: &str) {
    print("xhci: probe ctrl#");
    print_u64(idx as u64);
    print(" vid=0x");
    print_hex(vid as u64);
    print(" pid=0x");
    print_hex(pid as u64);
    print(" result=");
    println(result);
}

/// 失败分支的探测结论：多带端口/速度，用来看"是哪一类端口上的设备"。
fn probe_line_port(idx: u32, port: u32, speed: u32, result: &str) {
    print("xhci: probe ctrl#");
    print_u64(idx as u64);
    print(" port=");
    print_u64(port as u64);
    print(" spd=");
    print_u64(speed as u64);
    print(" result=");
    println(result);
}

/// 探测一台控制器：初始化 → 找端口 → 地址设备 → 识别存储 / HID 键盘。
fn probe_controller(g: &DeviceGrant, idx: u32) -> ProbeOutcome {
    if g.dma_bytes < DMA_PAGES * PAGE {
        println("xhci: device grant DMA too small, aborting");
        return ProbeOutcome::None;
    }
    let mmio = g.bar_vaddr;
    let dma_va = g.dma_vaddr;
    let dma_pa = g.dma_paddr;

    let caplength = rd8(mmio + CAP_CAPLENGTH) as u64;
    let op = mmio + caplength;
    let hcs1 = rd32(mmio + CAP_HCSPARAMS1);
    let hcs2 = rd32(mmio + CAP_HCSPARAMS2);
    let hcc1 = rd32(mmio + CAP_HCCPARAMS1);
    let rt = mmio + (rd32(mmio + CAP_RTSOFF) & !0x1F) as u64;
    let db = mmio + (rd32(mmio + CAP_DBOFF) & !0x3) as u64;
    let max_slots = hcs1 & 0xFF;
    let num_ports = (hcs1 >> 24) & 0xFF;
    let ctx_size: u64 = if hcc1 & (1 << 2) != 0 { 64 } else { 32 };
    let scratchpad = ((hcs2 >> 27) & 0x1F) | (((hcs2 >> 21) & 0x1F) << 5);

    print("xhci: ctrl#");
    print_u64(idx as u64);
    print(" bar=0x");
    print_hex(mmio);
    print(" caplen=");
    print_u64(caplength);
    print(" max_slots=");
    print_u64(max_slots as u64);
    print(" ports=");
    print_u64(num_ports as u64);
    print(" ctx=");
    print_u64(ctx_size);
    print(" scratchpad=");
    print_u64(scratchpad as u64);
    println("");

    let base = |page: u64| dma_va + page * PAGE;
    let pbase = |page: u64| dma_pa + page * PAGE;

    let mut hc = Xhci {
        mmio,
        op,
        rt,
        db,
        dma_pa,
        cmd: Ring::new(base(CMD_PAGE), pbase(CMD_PAGE)),
        evt: EventRing::new(base(EVT_PAGE), pbase(EVT_PAGE)),
        ep0: Ring::new(base(EP0_PAGE), pbase(EP0_PAGE)),
        bulkin: Ring::new(base(BIN_PAGE), pbase(BIN_PAGE)),
        bulkout: Ring::new(base(BOUT_PAGE), pbase(BOUT_PAGE)),
        dcbaa_va: base(DCBAA_PAGE),
        dcbaa_pa: pbase(DCBAA_PAGE),
        input_va: base(INPUT_PAGE),
        input_pa: pbase(INPUT_PAGE),
        devctx_pa: pbase(DEVCTX_PAGE),
        data_va: base(DATA_PAGE),
        data_pa: pbase(DATA_PAGE),
        bot_va: base(BOT_PAGE),
        bot_pa: pbase(BOT_PAGE),
        ctx_size,
        max_slots,
        num_ports,
        slot: 0,
        slot_ctx: [0; 8],
        bulkin_dci: 0,
        bulkout_dci: 0,
        bulkin_mps: 512,
        bulkout_mps: 512,
        tag: 0,
    };

    zero_bytes(dma_va as *mut u8, (DMA_PAGES * PAGE) as usize);

    // 先夺回 "USB Legacy Support" 所有权, 再复位控制器 —— 顺序不能反。
    hc.take_legacy();

    if !hc.reset() {
        println("xhci: controller reset FAILED (CNR still set)");
        return ProbeOutcome::None;
    }

    hc.cmd.write_link();
    hc.ep0.write_link();
    hc.bulkin.write_link();
    hc.bulkout.write_link();

    wr32(op + OP_USBSTS, 0x105C);

    // ERST：1 段，段表项 = {环基址, 环 TRB 数}。
    wr32(rt + RT_IR0 + IR_ERSTSZ, 1);
    wr64(base(ERST_PAGE), pbase(EVT_PAGE));
    wr64(base(ERST_PAGE) + 8, RING_TRBS as u64 & 0xFFFF);
    wr64(rt + RT_IR0 + IR_ERSTBA, pbase(ERST_PAGE));
    wr64(rt + RT_IR0 + IR_ERDP, pbase(EVT_PAGE));

    if scratchpad > 0 {
        if scratchpad > 4 {
            print("xhci: scratchpad too many (");
            print_u64(scratchpad as u64);
            println("), aborting");
            return ProbeOutcome::None;
        }
        // DCBAA[0] 是「scratchpad 数组的**物理**地址」(xHCI 规范 6.1) —— 控制器自己会
        // 按物理地址去读这张表。写虚拟地址时 QEMU (scratchpad=0) 永远走不到这里, 而真机
        // (scratchpad=2) 会拿到一个非法指针: Enable Slot 能成功, Address Device 直接失败。
        wr64(hc.dcbaa_va, pbase(SCRATCH_ARR_PAGE));
        let mut i = 0u64;
        while i < scratchpad as u64 {
            wr64(base(SCRATCH_ARR_PAGE) + i * 8, pbase(SCRATCH_PAGE0 + i));
            i += 1;
        }
    }
    wr64(op + OP_DCBAAP, hc.dcbaa_pa);

    let cfg = rd32(op + OP_CONFIG);
    wr32(op + OP_CONFIG, (cfg & !0xFF) | max_slots);
    wr64(op + OP_CRCR, hc.cmd.pa | 1);

    hc.start();
    print("xhci: ctrl#");
    print_u64(idx as u64);
    println(" started (controller running)");

    // 端口上电 + 等 USB3 链路训练。**必须在 RS=1 之后**：控制器停机时端口状态机
    // 不跑，SS 口会一直停在 RxDetect(CCS=0)，之前的 10 ms 上电等待等于没等。
    let mut present = false;
    let mut p = 1u32;
    while p <= num_ports {
        let reg = op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
        let sc = rd32(reg);
        if sc & PORT_PP == 0 {
            port_write(reg, sc, PORT_PP);
        }
        p += 1;
    }
    hc.wait_connect();

    print("xhci: ctrl#");
    print_u64(idx as u64);
    print(" ports=");
    print_u64(num_ports as u64);
    let mut p = 1u32;
    while p <= num_ports {
        let reg = op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
        let sc = rd32(reg);
        print(" 0x");
        print_hex(sc as u64);
        if sc & PORT_CCS != 0 {
            present = true;
        }
        p += 1;
    }
    println(if present {
        " (device present)"
    } else {
        " (no device)"
    });
    if !present {
        return ProbeOutcome::None;
    }

    // 全端口普查：真机只能靠这几行看清 SuperSpeed / USB2 两条 lane 的分布
    // （USB3 连接器 = 1 个 SS 口 + 1 个 USB2 伴随口，两者会同时报 CCS）。
    let mut cp = 1u32;
    while cp <= num_ports {
        let reg = op + OP_PORT_BASE + ((cp as u64 - 1) * OP_PORT_STRIDE);
        hc.log_port(cp, "census", rd32(reg));
        cp += 1;
    }

    // ---- 端口复位 ----
    let (port, speed) = match hc.find_port() {
        Some(v) => v,
        None => {
            print("xhci: ctrl#");
            print_u64(idx as u64);
            println(": no connected USB device");
            probe_line(idx, 0, 0, "no-device");
            return ProbeOutcome::None;
        }
    };
    print("xhci: port ");
    print_u64(port as u64);
    print(" speed=");
    print_u64(speed as u64);
    print(" portsc=0x");
    print_hex(rd32(op + OP_PORT_BASE + ((port as u64 - 1) * OP_PORT_STRIDE)) as u64);
    println(" connected");

    // ---- Enable Slot ----
    let ev = match hc.simple_cmd(TRB_ENABLE_SLOT, 0, 0) {
        Some(e) => e,
        None => {
            println("xhci: Enable Slot: no command completion event");
            return ProbeOutcome::None;
        }
    };
    if trb_cc(&ev) != CC_SUCCESS {
        print("xhci: Enable Slot FAILED cc=");
        print_u64(trb_cc(&ev) as u64);
        println("");
        return ProbeOutcome::None;
    }
    let slot = (ev.control >> 24) & 0xFF;
    hc.slot = slot;
    wr64(hc.dcbaa_va + (slot as u64) * 8, hc.devctx_pa);
    print("xhci: slot ");
    print_u64(slot as u64);
    println(" enabled");

    // ---- Address Device（失败则复位端口 + 换新 slot 重试）----
    let default_mps: u16 = match speed {
        1 | 2 => 8,
        4 | 5 => 512,
        _ => 64,
    };
    // 诊断: 真正送进 slot 上下文的三要素 (真机 cc=4 时, 这行能直接排除"速度/MPS 认错")。
    print("xhci: address slot=");
    print_u64(slot as u64);
    print(" port=");
    print_u64(port as u64);
    print(" spd=");
    print_u64(speed as u64);
    print(" mps=");
    print_u64(default_mps as u64);
    println("");
    let mut slot = slot;
    let mut ready = false;
    // EP0 的最大包长可能和「按端口速度推断」的不一致：真机 cc=3 (Babble) + residual=10
    // 说明设备只回了 8 字节（8 = 全速默认包长），而我们按高速声明了 64 → 控制器报 babble。
    // 故按 默认 → 8 → 16 → 32 依次重试，每次失败都复位端口、换新 slot 重来。
    let mut mps_list = [default_mps; 4];
    let mut n_mps = 1usize;
    for m in [8u16, 16, 32] {
        if m < default_mps {
            mps_list[n_mps] = m;
            n_mps += 1;
        }
    }
    let mut ci = 0usize;
    while ci < n_mps {
        let mps = mps_list[ci];
        ci += 1;
        if address_device(&mut hc, slot, speed, port, mps) {
            hc.log("device addressed");
            // USB 2.0 §9.2.6.3：第一次只读 8 字节（设备保证一个包就够），拿到
            // bMaxPacketSize0 后按它更新 EP0，再读完整的 18 字节。跳过这一步，
            // 真机会在 18 字节那次传输上报 cc=3 (Babble)。
            if hc.get_device_descriptor(8) {
                let b0 = rd8(hc.data_va + 7) as u16;
                if matches!(b0, 8 | 16 | 32 | 64) && b0 != mps {
                    if evaluate_ep0_mps(&mut hc, slot, b0) {
                        print("xhci: EP0 mps -> ");
                        print_u64(b0 as u64);
                        println("");
                    } else {
                        println("xhci: EP0 mps update FAILED (continuing)");
                    }
                }
                if hc.get_device_descriptor(18) {
                    ready = true;
                    break;
                }
            }
            println("xhci: GET_DESCRIPTOR(device) FAILED");
        } else {
            println("xhci: Address Device FAILED");
        }
        if ci == n_mps {
            break;
        }
        // 真机上第一次 SET_ADDRESS / 第一笔控制传输偶发不应答:
        // 复位端口让设备回默认地址, 并用一个新 slot 再试 (失败的 slot 可能已进 Error)。
        print("xhci: retry ");
        print_u64(ci as u64);
        print(" mps=");
        print_u64(mps_list[ci] as u64);
        println("");
        let sc = hc.reset_port(port);
        hc.log_port(port, "retry", sc);
        let _ = hc.simple_cmd(TRB_DISABLE_SLOT, slot, 0);
        match hc.simple_cmd(TRB_ENABLE_SLOT, 0, 0) {
            Some(ev) if trb_cc(&ev) == CC_SUCCESS => {
                slot = (ev.control >> 24) & 0xFF;
                hc.slot = slot;
                wr64(hc.dcbaa_va + (slot as u64) * 8, hc.devctx_pa);
            }
            _ => break,
        }
        sys_sleep(50);
    }
    if !ready {
        println("xhci: device unusable on this controller");
        probe_line_port(idx, port, speed, "desc-fail");
        return ProbeOutcome::None;
    }
    let bmax0 = rd8(hc.data_va + 7) as u16;
    // ⚠️ 必须在读配置描述符**之前**取：两次读都用 hc.data_va，后者会覆盖前者。
    let dev_class = rd8(hc.data_va + 4);
    let prod_idx = rd8(hc.data_va + 15);
    let vid = ((rd8(hc.data_va + 9) as u16) << 8) | rd8(hc.data_va + 8) as u16;
    let pid = ((rd8(hc.data_va + 11) as u16) << 8) | rd8(hc.data_va + 10) as u16;
    print("xhci: device descriptor vid=0x");
    print_hex(vid as u64);
    print(" pid=0x");
    print_hex(pid as u64);
    print(" bMaxPacket0=");
    print_u64(bmax0 as u64);
    println("");

    if !hc.control(0x80, 6, 2 << 8, 0, 9, true, hc.data_pa) {
        println("xhci: GET_DESCRIPTOR(config,9) FAILED");
        probe_line(idx, vid, pid, "cfg-fail");
        return ProbeOutcome::None;
    }
    let total = (rd8(hc.data_va + 2) as u32) | ((rd8(hc.data_va + 3) as u32) << 8);
    let want = if total > 512 { 512 } else { total };
    if want < 9 || !hc.control(0x80, 6, 2 << 8, 0, want as u16, true, hc.data_pa) {
        println("xhci: GET_DESCRIPTOR(config,full) FAILED");
        probe_line(idx, vid, pid, "cfg-fail");
        return ProbeOutcome::None;
    }
    let iface = parse_config(hc.data_va, want);
    // 诊断: 这台设备到底是什么 (真机没有 lspci -v, 只能把这几个字段打出来)。
    print("xhci: config ifaces=");
    print_u64(iface.n_ifaces as u64);
    print(" class=0x");
    print_hex(iface.class as u64);
    print(" sub=0x");
    print_hex(iface.subclass as u64);
    print(" proto=0x");
    print_hex(iface.proto as u64);
    print(" devclass=0x");
    print_hex(dev_class as u64);
    print(" bulk_in=");
    print_u64(iface.bulk_in.is_some() as u64);
    print(" bulk_out=");
    print_u64(iface.bulk_out.is_some() as u64);
    print(" hid=");
    print_u64(iface.hid.is_some() as u64);
    println("");
    log_product_string(&mut hc, prod_idx);
    if iface.class == 0x09 {
        println("xhci: this is a USB hub - devices behind it are not scanned");
        probe_line(idx, vid, pid, "hub");
    }

    // ---- USB HID 键盘（class=03 且带中断 IN；不要求 boot 子类/协议）----
    if let Some((dci, mps, interval)) = iface.hid {
        if !iface.hid_boot {
            println("xhci: HID is not boot subtype - trying anyway");
        }
        if !hc.control(0x00, 9, 1, 0, 0, false, 0) {
            println("xhci: kbd SET_CONFIGURATION FAILED");
            probe_line(idx, vid, pid, "hid-setcfg-fail");
            return ProbeOutcome::None;
        }
        if !configure_hid(&mut hc, slot, dci, mps, interval) {
            println("xhci: kbd Configure Endpoint FAILED");
            probe_line(idx, vid, pid, "hid-cfgep-fail");
            return ProbeOutcome::None;
        }
        // SET_PROTOCOL(boot)；个别键盘不认，仍按 boot report 尝试。
        if !hc.control(0x21, 0x0B, 0, 0, 0, false, 0) {
            println("xhci: kbd SET_PROTOCOL FAILED (still trying report format)");
        }
        print("xhci: USB HID keyboard ready (port=");
        print_u64(port as u64);
        print(", slot=");
        print_u64(slot as u64);
        print(", dci=");
        print_u64(dci as u64);
        print(") interval=");
        print_u64(interval as u64);
        println("");
        probe_line(idx, vid, pid, "hid-keyboard");
        return ProbeOutcome::Keyboard(KeyboardDev {
            hc,
            dci,
            prev: [0u8; 6],
            armed: false,
        });
    }

    // ---- USB 大容量存储（BOT + SCSI）----
    let (bin, bout) = match (iface.bulk_in, iface.bulk_out) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            println("xhci: device ignored (unsupported interface class)");
            probe_line(idx, vid, pid, "other");
            return ProbeOutcome::None;
        }
    };
    hc.bulkin_dci = bin.dci;
    hc.bulkout_dci = bout.dci;
    hc.bulkin_mps = bin.mps;
    hc.bulkout_mps = bout.mps;
    print("xhci: bulk-in dci=");
    print_u64(bin.dci as u64);
    print(" mps=");
    print_u64(bin.mps as u64);
    print(" out dci=");
    print_u64(bout.dci as u64);
    print(" mps=");
    print_u64(bout.mps as u64);
    println("");

    if !hc.control(0x00, 9, 1, 0, 0, false, 0) {
        println("xhci: SET_CONFIGURATION FAILED");
        probe_line(idx, vid, pid, "storage-setcfg-fail");
        return ProbeOutcome::None;
    }

    if !configure_endpoints(&mut hc, slot) {
        println("xhci: Configure Endpoint FAILED");
        probe_line(idx, vid, pid, "storage-cfgep-fail");
        return ProbeOutcome::None;
    }
    hc.log("bulk endpoints configured");

    if !hc.scsi_inquiry() {
        println("xhci: INQUIRY FAILED");
        probe_line(idx, vid, pid, "storage-inquiry-fail");
        return ProbeOutcome::None;
    }
    print("xhci: inquiry vendor=");
    print_sig(&{
        let mut v = [b' '; 16];
        let mut i = 0usize;
        while i < 8 {
            v[i] = rd8(hc.data_va + 8 + i as u64);
            i += 1;
        }
        v
    });
    println("");

    let (last_lba, blk) = match hc.scsi_read_capacity() {
        Some(v) => v,
        None => {
            println("xhci: READ CAPACITY FAILED");
            probe_line(idx, vid, pid, "storage-capacity-fail");
            return ProbeOutcome::None;
        }
    };
    let cap_sectors = (last_lba as u64) + 1;
    print("xhci: capacity sectors=");
    print_u64(cap_sectors);
    print(" blk=");
    print_u64(blk as u64);
    println("");

    if !hc.scsi_read10(0, 1) {
        println("xhci: READ(10) sector 0 FAILED");
        return ProbeOutcome::None;
    }
    let sector = unsafe { core::slice::from_raw_parts(hc.data_va as *const u8, 512) };
    let is_test = is_test_sig(sector, &SIG);
    let sig_kind = if is_test {
        "test"
    } else {
        boot_sig_kind(sector)
    };

    print("USB1 xhci OK, cap=");
    print_u64(cap_sectors);
    print(", sector0=");
    print(sig_kind);
    println("");

    if !attach_to_block(cap_sectors) {
        println("xhci: attach to block_srv FAILED (send refused), idle");
        probe_line(idx, vid, pid, "attach-fail");
        return ProbeOutcome::None;
    }

    probe_line(idx, vid, pid, "storage");
    ProbeOutcome::Storage(StorageDev { hc, cap_sectors })
}

/// 域 19 — xhci_srv：多控制器 xHCI 驱动（USB 存储 + HID 键盘）。
pub fn run() {
    let mut storage: Option<StorageDev> = None;
    let mut keyboard: Option<KeyboardDev> = None;

    let mut slot = 0u32;
    while slot < libdevice::grant::DEVICE_MULTI_SLOTS {
        let g = DeviceGrant::load_at(slot);
        if !g.is_valid() {
            slot += 1;
            continue;
        }
        match probe_controller(&g, slot) {
            ProbeOutcome::Storage(s) => {
                if storage.is_none() {
                    storage = Some(s);
                }
            }
            ProbeOutcome::Keyboard(k) => {
                if keyboard.is_none() {
                    keyboard = Some(k);
                }
            }
            ProbeOutcome::None => {}
        }
        slot += 1;
    }

    if storage.is_none() && keyboard.is_none() {
        println("xhci: no supported USB device (storage/keyboard), idle");
        idle();
    }

    // 键盘：压入第一笔等待数据的传输。
    if let Some(k) = keyboard.as_mut() {
        k.arm();
    }

    // 只有键盘（无存储）：纯轮询 HID。
    let Some(st) = storage.as_mut() else {
        loop {
            if let Some(k) = keyboard.as_mut() {
                k.poll();
            }
            sys_sleep(5);
        }
    };
    let _ = st.cap_sectors;

    // 服务循环：每轮先收 HID 报告，再非阻塞收 block_srv 的读/写请求。
    loop {
        if let Some(k) = keyboard.as_mut() {
            k.poll();
        }
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        let tag = sys_try_recv(&mut msg as *mut Message as *mut u8);
        if tag == u64::MAX {
            sys_sleep(1);
            continue;
        }
        if msg.tag != BLOCK_REQ_TAG {
            sys_reply(0);
            continue;
        }
        let req: BlockReq =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const BlockReq) };
        let ok = match (req.op & 0xFF) as u8 {
            BLOCK_OP_READ => serve_rw(&mut st.hc, true, req.lba, req.count),
            BLOCK_OP_WRITE => serve_rw(&mut st.hc, false, req.lba, req.count),
            _ => false,
        };
        sys_reply(if ok { 1 } else { 0 });
    }
}

// ---------------------------------------------------------------------------
// 卷层后端（03c：block_srv 经 IPC 转发读/写）
// ---------------------------------------------------------------------------

/// block_srv 同址共享给本域的传输暂存页（见 block_srv 顶部的用户态地址分区表）。
const XHCI_SCRATCH_VADDR: u64 = 0x0000_0080_0016_4000;
/// 单笔转发的扇区上限（= 私有数据页 4096 B / 512）。超出部分由 block_srv 的 `backend_rw` 切分。
const XHCI_MAX_SECTORS: u64 = 8;

/// 服务一笔来自 block_srv 的读/写（经 BOT + SCSI READ(10) / WRITE(10)）。
///
/// `lba`：盘内绝对 LBA（卷偏移已由 block_srv 合并）；`count`：扇区数（≤ `XHCI_MAX_SECTORS`）；
/// 数据在共享暂存页与私有数据页之间互拷。返回是否成功。
fn serve_rw(hc: &mut Xhci, is_read: bool, lba: u64, count: u64) -> bool {
    if count == 0 || count > XHCI_MAX_SECTORS {
        return false;
    }
    let bytes = (count * SECTOR as u64) as usize;
    if is_read {
        if !hc.scsi_read10(lba as u32, count as u16) {
            return false;
        }
        copy_buf(XHCI_SCRATCH_VADDR, hc.data_va, bytes);
        true
    } else {
        copy_buf(hc.data_va, XHCI_SCRATCH_VADDR, bytes);
        hc.scsi_write10(lba as u32, count as u16)
    }
}

/// 异步通知 block_srv「把这支 U 盘挂进卷层」（`BLOCK_OP_ATTACH`，`count` = 容量扇区）。
///
/// 用 send 而非 call：block_srv 收到后会**回调**本驱动做读写校验（经 USB 后端），
/// 若这里同步等回复就会自己把自己锁死（回调无人应答）。故异步通知，随后立即进服务循环。
fn attach_to_block(cap_sectors: u64) -> bool {
    let req = BlockReq {
        op: BLOCK_OP_ATTACH as u64,
        lba: 0,
        count: cap_sectors,
        buf: 0,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const BlockReq as *const u8,
            core::mem::size_of::<BlockReq>(),
        )
    };
    sys_send_payload(BLOCK_DOMAIN, BLOCK_REQ_TAG, payload) == 1
}

// ---------------------------------------------------------------------------
// 输入上下文装配
// ---------------------------------------------------------------------------

/// 把输入上下文清零，并返回 (slot 上下文地址, EP 上下文地址 = f(dci))。
fn clear_input(hc: &Xhci) -> (u64, u64) {
    zero_bytes(hc.input_va as *mut u8, PAGE as usize);
    let cs = hc.ctx_size;
    let slot_ctx = hc.input_va + cs;
    let ep_base = slot_ctx + cs; // DCI 1（EP0）就在 slot 之后
    (slot_ctx, ep_base)
}

/// 地址到某个 DCI 的 EP 上下文（输入上下文里第 dci 个 EP 上下文的地址）。
fn input_ep_ctx(hc: &Xhci, ep_base: u64, dci: u32) -> u64 {
    ep_base + ((dci as u64) - 1) * hc.ctx_size
}

/// 把 Address Device 时保存的 slot 上下文原样回填到输入上下文，只改 Context Entries。
fn restore_slot_ctx(hc: &Xhci, entries: u32) {
    let slot_ctx = hc.input_va + hc.ctx_size;
    let mut i = 0usize;
    while i < 8 {
        wr32(slot_ctx + (i as u64) * 4, hc.slot_ctx[i]);
        i += 1;
    }
    let d0 = (hc.slot_ctx[0] & !(0x1F << 27)) | ((entries & 0x1F) << 27);
    wr32(slot_ctx, d0);
}

/// Address Device：装配 slot + EP0 上下文并发命令。
fn address_device(hc: &mut Xhci, slot: u32, speed: u32, port: u32, mps: u16) -> bool {
    let (slot_ctx, ep_base) = clear_input(hc);
    // 每次（重新）寻址都把 EP0 传输环清零重来：上一次失败的控制传输 TD 可能还挂在
    // 环里，新 slot 的 EP0 从环首执行时读到的是**旧的** SETUP/DATA/STATUS —— 表现就是
    // 首笔 GET_DESCRIPTOR 永远等不到完成事件（真机实测 `EP0 req=6 no completion event`）。
    zero_bytes(hc.ep0.va as *mut u8, PAGE as usize);
    hc.ep0.enq = 0;
    hc.ep0.pcs = true;
    hc.ep0.write_link();
    // 输入控制上下文：Add flags = A0(slot) | A1(EP0)。
    wr32(hc.input_va + 4, 0b11);
    // Slot 上下文 DWORD0：Context Entries=1（只配 EP0）+ Speed（bits 23:20）。
    // Slot 上下文 DWORD1：Root Hub Port Number 在 bits 23:16（**不是** bits 15:8，
    // 15:8 是 TT Port Number）。写错则 QEMU 读到端口 0 → Address Device 回 TRB Error。
    let mut ctx = [0u32; 8];
    ctx[0] = (1 << 27) | (speed << 20);
    ctx[1] = port << 16;
    let mut i = 0usize;
    while i < 8 {
        wr32(slot_ctx + (i as u64) * 4, ctx[i]);
        i += 1;
    }
    // EP0 上下文：DWORD0（EP State / Interval）留 0；字段全在 DWORD1：
    //   MPS bits 31:16 | MaxBurst bits 15:8 | EP Type bits 5:3 | CErr bits 2:1。
    // （与 Linux `xhci.h`：MAX_PACKET(p)=((p&0xffff)<<16)、EP_TYPE(p)=(p<<3)、
    //   ERROR_COUNT(p)=((p&0x3)<<1) 一致。）DEQ|DCS 在 DWORD2。
    let ep0 = input_ep_ctx(hc, ep_base, 1);
    wr32(ep0 + 4, ((mps as u32) << 16) | (4 << 3) | (3 << 1));
    wr64(ep0 + 8, hc.ep0.pa | 1);

    match hc.context_cmd(TRB_ADDRESS_DEVICE, slot) {
        Some(e) => {
            let cc = trb_cc(&e);
            if cc == CC_SUCCESS {
                hc.slot_ctx = ctx;
            } else {
                // 完成码是唯一能区分「上下文写错(5 TRB Error / 17 Parameter Error)」与
                // 「真的没收到事件」的证据, 真机没串口只能靠它定位。
                print("xhci: Address Device cc=");
                print_u64(cc as u64);
                println("");
            }
            cc == CC_SUCCESS
        }
        None => {
            println("xhci: Address Device: no completion event");
            false
        }
    }
}

/// Evaluate Context：只改 EP0 的 Max Packet Size。
///
/// USB 2.0 §9.2.6.3 要求主机先读设备描述符的前 8 字节拿到 `bMaxPacketSize0`，
/// 再按它更新默认控制管道的包长。真机上跳过这步会出现
/// `cc=3 (Babble) residual=10`（设备按 8 字节包应答，控制器按 64 字节的 TD 判违规）。
fn evaluate_ep0_mps(hc: &mut Xhci, slot: u32, mps: u16) -> bool {
    let (_, ep_base) = clear_input(hc);
    wr32(hc.input_va + 4, 0b11); // A0 slot + A1 EP0
    restore_slot_ctx(hc, 1);
    let ep0 = input_ep_ctx(hc, ep_base, 1);
    wr32(ep0 + 4, ((mps as u32) << 16) | (4 << 3) | (3 << 1));
    // ⚠️ 必须交回**当前**入队位置：写环首会让控制器重放已完成的 TRB。
    wr64(ep0 + 8, hc.ep0.deq_pa());
    match hc.context_cmd(TRB_EVALUATE_CONTEXT, slot) {
        Some(e) => trb_cc(&e) == CC_SUCCESS,
        None => false,
    }
}

/// Configure Endpoint：把两条 Bulk 端点加进设备上下文。
fn configure_endpoints(hc: &mut Xhci, slot: u32) -> bool {
    let (_, ep_base) = clear_input(hc);
    let highest = if hc.bulkin_dci > hc.bulkout_dci {
        hc.bulkin_dci
    } else {
        hc.bulkout_dci
    };
    // Add flags：slot + 两条 Bulk 的 DCI 位（EP0 已配，不再 add）。
    let add = (1u32 << 0) | (1u32 << hc.bulkin_dci) | (1u32 << hc.bulkout_dci);
    wr32(hc.input_va + 4, add);
    // Slot 上下文：原样回填（含 Device Address / Speed / Port），只改 Context Entries。
    restore_slot_ctx(hc, highest);

    // Bulk-In：DWORD1 = MPS(bits 31:16) | EP Type(bits 5:3)=6 | CErr(bits 2:1)=3。
    let epin = input_ep_ctx(hc, ep_base, hc.bulkin_dci);
    wr32(
        epin + 4,
        ((hc.bulkin_mps as u32) << 16) | (6 << 3) | (3 << 1),
    );
    wr64(epin + 8, hc.bulkin.pa | 1);
    // Bulk-Out：EP Type=2。
    let epout = input_ep_ctx(hc, ep_base, hc.bulkout_dci);
    wr32(
        epout + 4,
        ((hc.bulkout_mps as u32) << 16) | (2 << 3) | (3 << 1),
    );
    wr64(epout + 8, hc.bulkout.pa | 1);

    match hc.context_cmd(TRB_CONFIGURE_ENDPOINT, slot) {
        Some(e) => trb_cc(&e) == CC_SUCCESS,
        None => false,
    }
}
