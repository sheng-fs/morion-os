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

// PORTSC 位
const PORT_CCS: u32 = 1 << 0;
const PORT_PED: u32 = 1 << 1;
const PORT_PR: u32 = 1 << 4;
const PORT_PP: u32 = 1 << 9;
/// 所有 RW1C 变化位（写回时须屏蔽，否则会把未处理的变化位清掉）。
const PORT_CHANGE: u32 =
    (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21) | (1 << 22) | (1 << 23);

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
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
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

    /// 复位并找到第一个在位的端口，返回 (端口号, 速度)。
    fn find_port(&self) -> Option<(u32, u32)> {
        let mut p = 1u32;
        while p <= self.num_ports {
            let reg = self.op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
            let mut sc = rd32(reg);
            // 上电（PP）。
            if sc & PORT_PP == 0 {
                wr32(reg, (sc & !PORT_CHANGE) | PORT_PP);
                let mut t = 0u32;
                while t < 100 {
                    sys_sleep(1);
                    t += 1;
                }
                sc = rd32(reg);
            }
            if sc & PORT_CCS == 0 {
                p += 1;
                continue;
            }
            // 触发端口复位，等 PR 自清。
            wr32(reg, (sc & !PORT_CHANGE) | PORT_PR);
            let mut t = 0u32;
            while t < POLL_MAX {
                sys_sleep(1);
                sc = rd32(reg);
                if sc & PORT_PR == 0 {
                    break;
                }
                t += 1;
            }
            // 清 PRC（端口复位变化），再读终态。
            sc = rd32(reg);
            wr32(reg, (sc & !PORT_CHANGE) | (1 << 20));
            let sc = rd32(reg);
            if sc & PORT_PED == 0 {
                p += 1;
                continue;
            }
            let speed = (sc >> 10) & 0xF;
            return Some((p, speed));
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
            Some(e) => trb_cc(&e) == CC_SUCCESS,
            None => false,
        }
    }

    /// 读设备描述符（前 8 字节）——Address Device 后确认链路通。
    fn get_device_descriptor(&mut self, want: u16) -> bool {
        self.control(0x80, 6, 1 << 8, 0, want, true, self.data_pa)
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
    fn scsi_write10(&mut self, lba: u32, count: u16) -> bool {
        let mut cb = [0u8; 16];
        cb[0] = 0x2A;
        cb[2] = (lba >> 24) as u8;
        cb[3] = (lba >> 16) as u8;
        cb[4] = (lba >> 8) as u8;
        cb[5] = lba as u8;
        cb[7] = (count >> 8) as u8;
        cb[8] = count as u8;
        self.scsi_cmd(&cb, (count as u32) * SECTOR, false)
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
// 主入口
// ---------------------------------------------------------------------------

/// 域 19 — xhci_srv：xHCI 驱动 + 只读自测（03c）。
pub fn run() {
    let g = DeviceGrant::load();
    if !g.is_valid() {
        println("xhci: no device grant (no xHCI), idle");
        idle();
    }
    if g.dma_bytes < DMA_PAGES * PAGE {
        println("xhci: device grant DMA too small, aborting");
        idle();
    }
    let mmio = g.bar_vaddr;
    let dma_va = g.dma_vaddr;
    let dma_pa = g.dma_paddr;

    // 读取能力寄存器并算出各寄存器块基址。
    let caplength = rd8(mmio + CAP_CAPLENGTH) as u64;
    let op = mmio + caplength;
    let hcs1 = rd32(mmio + CAP_HCSPARAMS1);
    let hcs2 = rd32(mmio + CAP_HCSPARAMS2);
    let hcc1 = rd32(mmio + CAP_HCCPARAMS1);
    let rt = mmio + (rd32(mmio + CAP_RTSOFF) & !0x1F) as u64;
    let db = mmio + (rd32(mmio + CAP_DBOFF) & !0x3) as u64;
    let max_slots = hcs1 & 0xFF;
    let num_ports = (hcs1 >> 24) & 0xFF;
    // CSZ (HCCPARAMS1 bit2): 上下文大小 = 64 B (1) 或 32 B (0)。
    let ctx_size: u64 = if hcc1 & (1 << 2) != 0 { 64 } else { 32 };
    // Scratchpad 数 = HCSPARAMS2 的 [31:27] 为低 5 位、[25:21] 为高 5 位。
    let scratchpad = ((hcs2 >> 27) & 0x1F) | (((hcs2 >> 21) & 0x1F) << 5);

    print("xhci: caplen=");
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
        bulkin_dci: 0,
        bulkout_dci: 0,
        bulkin_mps: 512,
        bulkout_mps: 512,
        tag: 0,
    };

    // 清零所有 DMA 结构页（ERST / 事件环 / 命令环 / DCBAA / 上下文 / 环 / 数据）。
    zero_bytes(dma_va as *mut u8, (DMA_PAGES * PAGE) as usize);

    if !hc.reset() {
        println("xhci: controller reset FAILED (CNR still set)");
        idle();
    }

    // 复位后事件环 / 环的 Link TRB 内容被清零，重新写一次。
    hc.cmd.write_link();
    hc.ep0.write_link();
    hc.bulkin.write_link();
    hc.bulkout.write_link();

    // 清掉复位后可能残留的 RW1C 状态位（HSE/EINT/PCD/SRE/HCE）——
    // ⚠️ 端口在复位时已连接，而事件环尚未建立，QEMU 会因「事件投递时 ERST 无效」置 HCE。
    wr32(op + OP_USBSTS, 0x105C);

    // 清掉复位后端口变化位（CSC/PLC 等 RW1C）—— 免得刚起步就被复位时产生的
    // Port Status Change 事件占满事件环（后续端口复位仍会正常产生事件）。
    let mut p = 1u32;
    while p <= num_ports {
        let reg = op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
        let sc = rd32(reg);
        wr32(reg, sc); // 写 1 清 RW1C 变化位
        p += 1;
    }

    // ERST：1 段，段表项 = {环基址, 环 TRB 数}。
    wr32(rt + RT_IR0 + IR_ERSTSZ, 1);
    wr64(base(ERST_PAGE), pbase(EVT_PAGE));
    wr64(base(ERST_PAGE) + 8, RING_TRBS as u64 & 0xFFFF);
    wr64(rt + RT_IR0 + IR_ERSTBA, pbase(ERST_PAGE));
    wr64(rt + RT_IR0 + IR_ERDP, pbase(EVT_PAGE));

    // DCBAA：数组首项留给 scratchpad 数组（没有则 0），其余槽位在启用时填。
    if scratchpad > 0 {
        if scratchpad > 4 {
            print("xhci: scratchpad too many (");
            print_u64(scratchpad as u64);
            println("), aborting");
            idle();
        }
        wr64(hc.dcbaa_va, base(SCRATCH_ARR_PAGE));
        let mut i = 0u64;
        while i < scratchpad as u64 {
            wr64(base(SCRATCH_ARR_PAGE) + i * 8, pbase(SCRATCH_PAGE0 + i));
            i += 1;
        }
    }
    wr64(op + OP_DCBAAP, hc.dcbaa_pa);

    // CONFIG.MaxSlotsEn。
    let cfg = rd32(op + OP_CONFIG);
    wr32(op + OP_CONFIG, (cfg & !0xFF) | max_slots);

    // CRCR：命令环基址 + RCS=1。
    wr64(op + OP_CRCR, hc.cmd.pa | 1);

    hc.start();

    // ---- 端口复位 ----
    let (port, speed) = match hc.find_port() {
        Some(v) => v,
        None => {
            println("xhci: no connected USB device on any port, idle");
            idle();
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
            idle();
        }
    };
    if trb_cc(&ev) != CC_SUCCESS {
        print("xhci: Enable Slot FAILED cc=");
        print_u64(trb_cc(&ev) as u64);
        println("");
        idle();
    }
    let slot = (ev.control >> 24) & 0xFF;
    hc.slot = slot;
    // DCBAA[slot] = 本设备的上下文。
    wr64(hc.dcbaa_va + (slot as u64) * 8, hc.devctx_pa);
    print("xhci: slot ");
    print_u64(slot as u64);
    println(" enabled");

    // ---- Address Device ----
    // 速度默认最大包：1=Full(8) 2=Low(8) 3=High(64) 4=Super(512)。
    let default_mps: u16 = match speed {
        1 | 2 => 8,
        4 | 5 => 512,
        _ => 64,
    };
    if !address_device(&mut hc, slot, speed, port, default_mps) {
        println("xhci: Address Device FAILED");
        idle();
    }
    hc.log("device addressed");

    // ---- 读设备描述符（18 字节）----
    if !hc.get_device_descriptor(18) {
        println("xhci: GET_DESCRIPTOR(device) FAILED");
        idle();
    }
    let bmax0 = rd8(hc.data_va + 7) as u16;
    print("xhci: device descriptor vid=0x");
    print_hex(((rd8(hc.data_va + 9) as u64) << 8) | rd8(hc.data_va + 8) as u64);
    print(" pid=0x");
    print_hex(((rd8(hc.data_va + 11) as u64) << 8) | rd8(hc.data_va + 10) as u64);
    print(" bMaxPacket0=");
    print_u64(bmax0 as u64);
    println("");

    // ---- 读配置描述符（先 9 字节拿 wTotalLength，再读全量）----
    if !hc.control(0x80, 6, 2 << 8, 0, 9, true, hc.data_pa) {
        println("xhci: GET_DESCRIPTOR(config,9) FAILED");
        idle();
    }
    let total = (rd8(hc.data_va + 2) as u32) | ((rd8(hc.data_va + 3) as u32) << 8);
    let want = if total > 512 { 512 } else { total };
    if want < 9 || !hc.control(0x80, 6, 2 << 8, 0, want as u16, true, hc.data_pa) {
        println("xhci: GET_DESCRIPTOR(config,full) FAILED");
        idle();
    }
    let (bin, bout) = match parse_bulk_endpoints(hc.data_va, want) {
        Some(v) => v,
        None => {
            println("xhci: no bulk endpoints in config descriptor");
            idle();
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

    // ---- SET_CONFIGURATION(1) ----
    if !hc.control(0x00, 9, 1, 0, 0, false, 0) {
        println("xhci: SET_CONFIGURATION FAILED");
        idle();
    }

    // ---- Configure Endpoint（两条 Bulk）----
    if !configure_endpoints(&mut hc, slot, speed, port) {
        println("xhci: Configure Endpoint FAILED");
        idle();
    }
    hc.log("bulk endpoints configured");

    // ---- SCSI 自测：INQUIRY → READ CAPACITY → READ(10) 扇区 0 ----
    if !hc.scsi_inquiry() {
        println("xhci: INQUIRY FAILED");
        idle();
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
            idle();
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
        idle();
    }
    let mut sig = [0u8; 16];
    let mut sig_ok = true;
    let mut i = 0u64;
    while i < 16 {
        sig[i as usize] = rd8(hc.data_va + i);
        if sig[i as usize] != SIG[i as usize] {
            sig_ok = false;
        }
        i += 1;
    }

    // marker：只读链路的端到端取证（枚举 + BOT + SCSI READ(10)）。
    print("USB1 xhci OK, cap=");
    print_u64(cap_sectors);
    print(", sector0 sig=");
    print_sig(&sig);
    print(", sig=");
    print(if sig_ok { "ok" } else { "BAD" });
    println("");

    // 03c：把 U 盘挂进 block_srv 的卷层（异步通知，随后 block_srv 回调做读写校验）。
    if !attach_to_block(cap_sectors) {
        println("xhci: attach to block_srv FAILED (send refused), idle");
        idle();
    }

    // 服务循环：接收 block_srv 转来的 `BlockReq`（读/写），每笔经 BOT+SCSI 完成后回复。
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);

        if msg.tag != BLOCK_REQ_TAG {
            sys_reply(0);
            continue;
        }
        let req: BlockReq =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const BlockReq) };
        let ok = match (req.op & 0xFF) as u8 {
            BLOCK_OP_READ => serve_rw(&mut hc, true, req.lba, req.count),
            BLOCK_OP_WRITE => serve_rw(&mut hc, false, req.lba, req.count),
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

/// Address Device：装配 slot + EP0 上下文并发命令。
fn address_device(hc: &mut Xhci, slot: u32, speed: u32, port: u32, mps: u16) -> bool {
    let (slot_ctx, ep_base) = clear_input(hc);
    // 输入控制上下文：Add flags = A0(slot) | A1(EP0)。
    wr32(hc.input_va + 4, 0b11);
    // Slot 上下文 DWORD0：Context Entries=1（只配 EP0）+ Speed（bits 23:20）。
    wr32(slot_ctx, (1 << 27) | (speed << 20));
    // Slot 上下文 DWORD1：Root Hub Port Number 在 bits 23:16（**不是** bits 15:8，
    // 15:8 是 TT Port Number）。写错则 QEMU 读到端口 0 → Address Device 回 TRB Error。
    wr32(slot_ctx + 4, port << 16);
    // EP0 上下文：DWORD0（EP State / Interval）留 0；字段全在 DWORD1：
    //   MPS bits 31:16 | MaxBurst bits 15:8 | EP Type bits 5:3 | CErr bits 2:1。
    // （与 Linux `xhci.h`：MAX_PACKET(p)=((p&0xffff)<<16)、EP_TYPE(p)=(p<<3)、
    //   ERROR_COUNT(p)=((p&0x3)<<1) 一致。）DEQ|DCS 在 DWORD2。
    let ep0 = input_ep_ctx(hc, ep_base, 1);
    wr32(ep0 + 4, ((mps as u32) << 16) | (4 << 3) | (3 << 1));
    wr64(ep0 + 8, hc.ep0.pa | 1);

    match hc.context_cmd(TRB_ADDRESS_DEVICE, slot) {
        Some(e) => trb_cc(&e) == CC_SUCCESS,
        None => false,
    }
}

/// Configure Endpoint：把两条 Bulk 端点加进设备上下文。
fn configure_endpoints(hc: &mut Xhci, slot: u32, speed: u32, port: u32) -> bool {
    let (slot_ctx, ep_base) = clear_input(hc);
    let highest = if hc.bulkin_dci > hc.bulkout_dci {
        hc.bulkin_dci
    } else {
        hc.bulkout_dci
    };
    // Add flags：slot + 两条 Bulk 的 DCI 位（EP0 已配，不再 add）。
    let add = (1u32 << 0) | (1u32 << hc.bulkin_dci) | (1u32 << hc.bulkout_dci);
    wr32(hc.input_va + 4, add);
    // Slot 上下文：Context Entries = 最高 DCI（Configure 时也要带，保持速度/端口一致）。
    wr32(slot_ctx, (highest << 27) | (speed << 20));
    wr32(slot_ctx + 4, port << 16);

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
