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
// USB2 Hub（class 09）
// ---------------------------------------------------------------------------
const USB_DT_HUB: u8 = 0x29;
/// Hub Class 请求：SET_FEATURE / CLEAR_FEATURE / GET_PORT_STATUS。
const HUB_SET_FEATURE: u8 = 3;
const HUB_CLEAR_FEATURE: u8 = 1;
const HUB_GET_PORT_STATUS: u8 = 0;
/// Hub Class 端口特性号。
const HUB_PORT_POWER: u16 = 8;
const HUB_PORT_RESET: u16 = 4;
const HUB_C_PORT_CONNECTION: u16 = 16;
const HUB_C_PORT_ENABLE: u16 = 17;
const HUB_C_PORT_RESET: u16 = 20;
/// wPortStatus（GET_PORT_STATUS 返回的低 16 位）。
const HUB_CCS: u32 = 1 << 0;
const HUB_ENABLE: u32 = 1 << 1;
const HUB_LOW_SPEED: u32 = 1 << 9;
const HUB_HIGH_SPEED: u32 = 1 << 10;
/// 变化位（高 16 位）。
const HUB_C_CONNECTION: u32 = 1 << 16;
const HUB_C_ENABLE: u32 = 1 << 17;
const HUB_C_RESET: u32 = 1 << 20;

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
// DMA 页布局（内核授权 24 页；每页 4 KiB）
//
// 0..12 控制器级（ERST / 事件环 / 命令环 / DCBAA / 输入上下文 / BOT / scratchpad
// 数组 + 最多 5 页 scratchpad），12..22 两组 slot 页（每组 devctx + EP0 + Bin + Bout +
// data 各 1 页），22..24 预留。真机 scratchpad = 2（占页 7、8）。
// ---------------------------------------------------------------------------
const DMA_PAGES: u64 = 24;
const ERST_PAGE: u64 = 0;
const EVT_PAGE: u64 = 1;
const CMD_PAGE: u64 = 2;
const DCBAA_PAGE: u64 = 3;
const INPUT_PAGE: u64 = 4;
const BOT_PAGE: u64 = 5;
const SCRATCH_ARR_PAGE: u64 = 6;
const SCRATCH_PAGE0: u64 = 7; // 7..12 共 5 页作 scratchpad（真机 2）

/// 每台控制器最多同时保留的 slot 数（一个存储 + 一个 HID）。
const SLOT_GROUPS: usize = 2;
const SLOT_DEVCTX_PAGES: [u64; SLOT_GROUPS] = [12, 17];
const SLOT_EP0_PAGES: [u64; SLOT_GROUPS] = [13, 18];
const SLOT_BIN_PAGES: [u64; SLOT_GROUPS] = [14, 19];
const SLOT_BOUT_PAGES: [u64; SLOT_GROUPS] = [15, 20];
const SLOT_DATA_PAGES: [u64; SLOT_GROUPS] = [16, 21];

/// 每条环的 TRB 数（末位留作 Link TRB，故可用 63 个）。
const RING_TRBS: usize = 64;

/// 轮询上限（每次让出 1 tick ≈ 1 ms，故上界 ≈ 2 s）。
const POLL_MAX: u32 = 2000;

/// 空轮询时每轮让出时间片前允许的**有界自旋**次数。
///
/// 传输完成事件通常在几十微秒内到达；`sys_sleep(1)` 的粒度是 1 ms，
/// 每笔传输 3–5 次等待就是 3–5 ms 的固定开销。512 次自旋在真机上约几十微秒，
/// 既能把事件及时收走，又不会长时间霸占 CPU。
const SPIN_BUDGET: u32 = 512;

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
    dma_va: u64,
    dma_pa: u64,

    cmd: Ring,
    evt: EventRing,

    dcbaa_va: u64,
    dcbaa_pa: u64,
    input_va: u64,
    input_pa: u64,
    bot_va: u64,
    bot_pa: u64,

    ctx_size: u64,
    max_slots: u32,
    num_ports: u32,

    tag: u32,
    /// 服务某条传输时被 `wait_event` 丢弃的**其它 slot** 的传输事件。
    ///
    /// 事件环由同一控制器的所有 slot 共享：存储传输期间到达的 HID 完成事件若直接丢掉，
    /// 键盘那笔 Interrupt IN 就永远等不到完成、也不会重新压入。暂存在这里由
    /// `KeyboardDev::poll` 取走（同一时刻至多一笔未完成的 HID 传输，故 4 格足够）。
    pending: [Option<Trb>; 4],

    /// 已占用的 slot 页组位图（bit g = 第 g 组已被某台设备占用）。
    /// 运行期热插拔只在这里找空位 —— 启动时由 `probe_started` 填好。
    hotplug_used: u32,
    /// 每个根端口上一次看到的 CCS（bit = 端口号 - 1）。用来区分「新接入」与
    /// 「复位完成只置了 PRC」—— 只看 CSC/PRC 会把已配置的设备重新枚举一遍。
    port_ccs: [u32; 8],
    /// 是否已拍下端口基线（首次热插拔轮询时惰性完成，不改变启动路径）。
    hotplug_synced: bool,
}

/// 一台已寻址设备（slot）的私有状态：上下文快照、三条传输环与私有数据页。
///
/// 控制器级资源（命令环 / 事件环 / DCBAA / 输入上下文 / BOT 页）留在 [`Xhci`]，
/// 每台设备只持有自己的页，于是同一控制器可同时保留存储 + HID 两个 slot。
struct Slot {
    slot: u32,
    devctx_pa: u64,
    /// Address Device 时写进输入上下文的 slot 上下文（8 个 DWORD）。
    /// Configure Endpoint 必须原样回填 —— xHC 会整份替换自己的 slot 上下文，
    /// 其中 **Device Address (DWORD3 bits 7:0)** 被清 0 后，后续所有传输都会发往
    /// 地址 0（真机全部 cc=4；QEMU 不校验所以看不出来）。
    slot_ctx: [u32; 8],
    ep0: Ring,
    bulkin: Ring,
    bulkout: Ring,
    bulkin_dci: u32,
    bulkout_dci: u32,
    bulkin_mps: u16,
    bulkout_mps: u16,
    data_va: u64,
    data_pa: u64,
}

impl Slot {
    /// 用第 `group` 组 slot 页构造一个（尚未寻址的）设备状态。
    fn new(hc: &Xhci, group: usize) -> Self {
        let va = |page: u64| hc.dma_va + page * PAGE;
        let pa = |page: u64| hc.dma_pa + page * PAGE;
        Slot {
            slot: 0,
            devctx_pa: pa(SLOT_DEVCTX_PAGES[group]),
            slot_ctx: [0; 8],
            ep0: Ring::new(va(SLOT_EP0_PAGES[group]), pa(SLOT_EP0_PAGES[group])),
            bulkin: Ring::new(va(SLOT_BIN_PAGES[group]), pa(SLOT_BIN_PAGES[group])),
            bulkout: Ring::new(va(SLOT_BOUT_PAGES[group]), pa(SLOT_BOUT_PAGES[group])),
            bulkin_dci: 0,
            bulkout_dci: 0,
            bulkin_mps: 512,
            bulkout_mps: 512,
            data_va: va(SLOT_DATA_PAGES[group]),
            data_pa: pa(SLOT_DATA_PAGES[group]),
        }
    }
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
// 多控制器链路训练：所有控制器共享一轮全局轮询
// ---------------------------------------------------------------------------

/// 单台控制器在链路训练等待中的累计状态。
#[derive(Clone, Copy)]
struct LinkWait {
    saw: bool,
    last: u32,
    stable: u32,
    settled: bool,
}

/// 等所有已初始化控制器的端口完成链路训练，**共享一轮轮询**。
///
/// 判定规则与原先逐台调用 `wait_connect` 时逐条一致：没有设备时等
/// `LINK_WAIT_EMPTY_MS` 就收工；看到设备后至少等 `LINK_WAIT_MIN_MS`，并且连接数稳定
/// 150 ms、没有端口还在训练才收工；总上限 `LINK_WAIT_MAX_MS`。差别只在于这些等待在
/// 13 台控制器之间**重叠**，而不是各等各的 250/600 ms 串行叠加。
fn wait_connect_all(ctrls: &mut [Option<Xhci>; libdevice::grant::DEVICE_MULTI_SLOTS as usize]) {
    let mut st: [LinkWait; libdevice::grant::DEVICE_MULTI_SLOTS as usize] =
        core::array::from_fn(|_| LinkWait {
            saw: false,
            last: 0,
            stable: 0,
            settled: false,
        });
    let mut t = 0u32;
    while t < LINK_WAIT_MAX_MS {
        let mut all_settled = true;
        for (i, c) in ctrls.iter().enumerate() {
            let Some(hc) = c else { continue };
            if st[i].settled {
                continue;
            }
            let mut n = 0u32;
            let mut training = false;
            let mut p = 1u32;
            while p <= hc.num_ports {
                let reg = hc.op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
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
            let w = &mut st[i];
            if n > 0 {
                w.saw = true;
            }
            if n == w.last {
                w.stable += LINK_POLL_MS;
            } else {
                w.last = n;
                w.stable = 0;
            }
            // 没有设备：很快放弃；有设备：至少等 LINK_WAIT_MIN_MS，再等
            // “连接数稳定 + 没有端口还在训练”。
            let empty_done = !w.saw && t >= LINK_WAIT_EMPTY_MS;
            let link_done = w.saw && t >= LINK_WAIT_MIN_MS && w.stable >= 150 && !training;
            if empty_done || link_done {
                w.settled = true;
            } else {
                all_settled = false;
            }
        }
        if all_settled {
            break;
        }
        sys_sleep(LINK_POLL_MS as u64);
        t += LINK_POLL_MS;
    }
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
    ///
    /// 空轮询**先有界自旋再让出时间片**：批量传输的完成事件通常几十微秒内就到，
    /// 直接 `sys_sleep(1)` 会把每笔传输硬拖 1 ms —— 一个 4 KiB 读要 3 次等待，
    /// 等于凭空多出 3 ms，吞吐被压到 1 MB/s 量级。自旋仍受 `POLL_MAX` 的超时上限约束。
    fn wait_event(&mut self, kind: u32, slot: u32, dci: u32) -> Option<Trb> {
        let erdp = self.rt + RT_IR0 + IR_ERDP;
        let mut ticks = 0u32;
        let mut iters = 0u32;
        let mut spins = 0u32;
        // iters 上限按「超时毫秒数 × 每毫秒自旋预算」放宽，避免自旋把总等待时间
        // 提前截断（真正的超时判据仍是 ticks）。
        while ticks < POLL_MAX && iters < POLL_MAX * SPIN_BUDGET + 10_000 {
            iters += 1;
            match self.evt.poll(erdp) {
                Some(e) => {
                    spins = 0;
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
                    // 同一控制器上另一台设备（HID）的完成事件：不能丢，留给自己 poll。
                    self.stash(e);
                }
                None => {
                    if spins < SPIN_BUDGET {
                        spins += 1;
                        core::hint::spin_loop();
                    } else {
                        spins = 0;
                        sys_sleep(1);
                        ticks += 1;
                    }
                }
            }
        }
        None
    }

    /// 暂存一个不属于当前等待者的传输事件（供 `KeyboardDev::poll` 取走）。
    fn stash(&mut self, e: Trb) {
        if let Some(cell) = self.pending.iter_mut().find(|p| p.is_none()) {
            *cell = Some(e);
        }
    }

    /// 取走暂存里属于 `slot`/`dci` 的传输事件。
    fn take_pending(&mut self, slot: u32, dci: u32) -> Option<Trb> {
        let idx = self.pending.iter().position(|p| match p {
            Some(e) => {
                trb_type(e) == TRB_TRANSFER_EVENT
                    && ((e.control >> 24) & 0xFF) == slot
                    && ((e.control >> 16) & 0x1F) == dci
            }
            None => false,
        })?;
        self.pending[idx].take()
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
    fn bulk_xfer(&mut self, s: &mut Slot, which: u8, pa: u64, len: u32, dir_in: bool) -> bool {
        let dci = match which {
            EP_EP0 => 1,
            EP_BULK_IN => s.bulkin_dci,
            _ => s.bulkout_dci,
        };
        let trb = Trb {
            param: pa,
            status: len & 0x1FFFF,
            control: (TRB_NORMAL << 10) | (1 << 5) | if dir_in { 1 << 16 } else { 0 },
        };
        match which {
            EP_EP0 => s.ep0.push(trb),
            EP_BULK_IN => s.bulkin.push(trb),
            _ => s.bulkout.push(trb),
        }
        fence();
        wr32(self.db + (s.slot as u64) * 4, dci);
        match self.wait_event(TRB_TRANSFER_EVENT, s.slot, dci) {
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
        s: &mut Slot,
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
        s.ep0.push(Trb {
            param,
            status: 8,
            control: (TRB_SETUP << 10) | (1 << 6) | (trt << 16),
        });
        if len != 0 {
            s.ep0.push(Trb {
                param: data_pa,
                status: len as u32,
                control: (TRB_DATA << 10) | if dir_in { 1 << 16 } else { 0 },
            });
        }
        // Status Stage：数据阶段为 IN 时状态阶段是 OUT（DIR=0），否则 IN（DIR=1）。
        let status_dir = !(dir_in && len != 0);
        s.ep0.push(Trb {
            param: 0,
            status: 0,
            control: (TRB_STATUS << 10) | if status_dir { 1 << 16 } else { 0 } | (1 << 5),
        });
        fence();
        wr32(self.db + (s.slot as u64) * 4, 1); // DCI 1 = EP0
        match self.wait_event(TRB_TRANSFER_EVENT, s.slot, 1) {
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
    fn get_device_descriptor(&mut self, s: &mut Slot, want: u16) -> bool {
        sys_sleep(50);
        let mut attempt = 0u32;
        while attempt < 3 {
            if self.control(s, 0x80, 6, 1 << 8, 0, want, true, s.data_pa) {
                return true;
            }
            attempt += 1;
            if attempt < 3 {
                sys_sleep(50);
            }
        }
        false
    }

    // -----------------------------------------------------------------------
    // USB2 Hub（class 09）
    // -----------------------------------------------------------------------

    /// 读 Hub 描述符（type 0x29，9 字节）。
    fn hub_descriptor(&mut self, s: &mut Slot) -> Option<HubDesc> {
        if !self.control(s, 0xA0, 6, (USB_DT_HUB as u16) << 8, 0, 9, true, s.data_pa) {
            return None;
        }
        Some(HubDesc {
            ports: rd8(s.data_va + 2),
            chars: (rd8(s.data_va + 3) as u16) | ((rd8(s.data_va + 4) as u16) << 8),
            pwr_good: rd8(s.data_va + 5),
        })
    }

    /// Hub class SET_FEATURE（端口特性）。
    fn hub_set_feature(&mut self, s: &mut Slot, port: u16, feature: u16) -> bool {
        self.control(s, 0x23, HUB_SET_FEATURE, feature, port, 0, false, 0)
    }

    /// Hub class CLEAR_FEATURE（端口变化位）。
    fn hub_clear_feature(&mut self, s: &mut Slot, port: u16, feature: u16) -> bool {
        self.control(s, 0x23, HUB_CLEAR_FEATURE, feature, port, 0, false, 0)
    }

    /// GET_PORT_STATUS（4 字节；低 16 位 status、高 16 位变化位）。
    fn hub_port_status(&mut self, s: &mut Slot, port: u16) -> Option<u32> {
        if !self.control(s, 0xA3, HUB_GET_PORT_STATUS, 0, port, 4, true, s.data_pa) {
            return None;
        }
        Some(rd32(s.data_va))
    }

    /// 复位 Hub 的下游端口并清掉变化位，返回终态 wPortStatus。
    fn hub_reset_port(&mut self, s: &mut Slot, port: u8) -> Option<u32> {
        let p = port as u16;
        if !self.hub_set_feature(s, p, HUB_PORT_RESET) {
            return None;
        }
        let mut st = 0u32;
        let mut t = 0u32;
        while t < 500 {
            if let Some(v) = self.hub_port_status(s, p) {
                st = v;
                if v & HUB_ENABLE != 0 {
                    break;
                }
            }
            sys_sleep(1);
            t += 1;
        }
        if st & HUB_C_RESET != 0 {
            let _ = self.hub_clear_feature(s, p, HUB_C_PORT_RESET);
        }
        if st & HUB_C_CONNECTION != 0 {
            let _ = self.hub_clear_feature(s, p, HUB_C_PORT_CONNECTION);
        }
        if st & HUB_C_ENABLE != 0 {
            let _ = self.hub_clear_feature(s, p, HUB_C_PORT_ENABLE);
        }
        self.hub_port_status(s, p)
    }

    /// 用 Evaluate Context 补写 Hub 的 slot 上下文：Hub/MTT 位、Number of Ports、
    /// TT Think Time。Address Device 时还读不到 Hub 描述符，故只能这时候补。
    fn update_hub_ctx(&mut self, s: &mut Slot, proto: u8, ports: u8, think: u8) -> bool {
        let (slot_ctx, _) = clear_input(self);
        wr32(self.input_va + 4, 1); // A0: 只评估 slot 上下文
        let mut ctx = s.slot_ctx;
        ctx[0] = (ctx[0] | (1 << 26)) & !(1 << 25);
        if proto == 2 {
            ctx[0] |= 1 << 25;
        }
        ctx[1] = (ctx[1] & !(0xFF << 24)) | ((ports as u32) << 24);
        ctx[2] = (ctx[2] & !(0x3 << 16)) | (((think & 3) as u32) << 16);
        let mut i = 0usize;
        while i < 8 {
            wr32(slot_ctx + (i as u64) * 4, ctx[i]);
            i += 1;
        }
        match self.context_cmd(TRB_EVALUATE_CONTEXT, s.slot) {
            Some(e) if trb_cc(&e) == CC_SUCCESS => {
                s.slot_ctx = ctx;
                true
            }
            Some(e) => {
                print("xhci: hub Evaluate Context cc=");
                print_u64(trb_cc(&e) as u64);
                println("");
                false
            }
            None => false,
        }
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
    fn bot_once(&mut self, s: &mut Slot, cb: &[u8], data_len: u32, dir_in: bool) -> BotResult {
        self.tag = self.tag.wrapping_add(1);
        let tag = self.tag;

        // ① CBW（31 字节）经 Bulk-Out 发出。
        let mut cbw = [0u8; 31];
        build_cbw(&mut cbw, tag, data_len, dir_in, cb);
        copy_buf(self.bot_va, cbw.as_ptr() as u64, 31);
        if !self.bulk_xfer(s, EP_BULK_OUT, self.bot_pa, 31, false) {
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
            if !self.bulk_xfer(s, which, s.data_pa, len, dir_in) {
                err_once("data bulk transfer failed");
                return BotResult {
                    ok: false,
                    status: 0xFF,
                };
            }
        }

        // ③ CSW（13 字节）从 Bulk-In 读回。
        if !self.bulk_xfer(s, EP_BULK_IN, self.bot_pa + 64, 13, true) {
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
    fn scsi_cmd(&mut self, s: &mut Slot, cb: &[u8], data_len: u32, dir_in: bool) -> bool {
        let mut attempt = 0u32;
        while attempt < 3 {
            let r = self.bot_once(s, cb, data_len, dir_in);
            if !r.ok {
                return false;
            }
            if r.status == 0 {
                return true;
            }
            // CSW status != 0：取 sense（UNIT ATTENTION 等）后重试。
            self.request_sense(s);
            attempt += 1;
        }
        false
    }

    /// REQUEST SENSE（0x03）：18 字节，结果丢弃（只为清掉 UNIT ATTENTION）。
    fn request_sense(&mut self, s: &mut Slot) {
        let cb = [0x03u8, 0, 0, 0, 18, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let _ = self.bot_once(s, &cb, 18, true);
    }

    /// INQUIRY（0x12）：取 36 字节识别信息。
    fn scsi_inquiry(&mut self, s: &mut Slot) -> bool {
        let cb = [0x12u8, 0, 0, 0, 36, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        self.scsi_cmd(s, &cb, 36, true)
    }

    /// READ CAPACITY(10)（0x25）：返回 (最后 LBA, 块字节数)。
    fn scsi_read_capacity(&mut self, s: &mut Slot) -> Option<(u32, u32)> {
        let cb = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        if !self.scsi_cmd(s, &cb, 8, true) {
            return None;
        }
        let mut buf = [0u8; 8];
        copy_buf(buf.as_mut_ptr() as u64, s.data_va, 8);
        let last_lba = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let blk = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        Some((last_lba, blk))
    }

    /// READ(10)（0x28）：读 `count` 个扇区到私有数据页。
    fn scsi_read10(&mut self, s: &mut Slot, lba: u32, count: u16) -> bool {
        let mut cb = [0u8; 16];
        cb[0] = 0x28;
        cb[2] = (lba >> 24) as u8;
        cb[3] = (lba >> 16) as u8;
        cb[4] = (lba >> 8) as u8;
        cb[5] = lba as u8;
        cb[7] = (count >> 8) as u8;
        cb[8] = count as u8;
        self.scsi_cmd(s, &cb, (count as u32) * SECTOR, true)
    }

    /// WRITE(10)（0x2A）：把私有数据页的 `count` 个扇区写入 `lba`（安全门由调用方把关）。
    ///
    /// 写完立刻发 SYNCHRONIZE CACHE(10)：U 盘自己的写缓存不刷到闪存的话，关机/拔盘后
    /// 文件会"写成功但不见了"（真机 `/usb` 空分区的根因之一）。
    fn scsi_write10(&mut self, s: &mut Slot, lba: u32, count: u16) -> bool {
        let mut cb = [0u8; 16];
        cb[0] = 0x2A;
        cb[2] = (lba >> 24) as u8;
        cb[3] = (lba >> 16) as u8;
        cb[4] = (lba >> 8) as u8;
        cb[5] = lba as u8;
        cb[7] = (count >> 8) as u8;
        cb[8] = count as u8;
        if !self.scsi_cmd(s, &cb, (count as u32) * SECTOR, false) {
            return false;
        }
        // 不支持该命令的盘（少数）只记一次日志，不让整笔写失败。
        if !self.scsi_sync_cache(s) {
            err_once("SYNCHRONIZE CACHE(10) failed (write may not be durable)");
        }
        true
    }

    /// SYNCHRONIZE CACHE(10)（0x35）：把 U 盘内部写缓存刷到闪存。
    fn scsi_sync_cache(&mut self, s: &mut Slot) -> bool {
        let cb = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        self.scsi_cmd(s, &cb, 0, false)
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

/// 一台控制器的探测结果：可同时带回存储与键盘（顺序不限）。
#[derive(Default)]
struct Probed {
    storage: Option<StorageDev>,
    keyboard: Option<KeyboardDev>,
}

/// 已挂进 block_srv 卷层的 USB 存储：只记控制器下标 + 自己的 slot。
struct StorageDev {
    ctrl: usize,
    /// 这台设备占用的 slot 页组（热插拔找空位 / 复用断开的槽用）。
    group: usize,
    /// 挂接的根端口（拔出的判定依据；Hub 下游设备记它所属的根端口）。
    port: u32,
    slot: Slot,
    cap_sectors: u64,
    /// 端口已断开：保留 slot 状态，但不再向它发请求（不做复杂回收）。
    dead: bool,
}

/// 一台已配置好的 USB HID 键盘：控制器状态由 `run()` 的 `ctrls` 共享持有。
struct KeyboardDev {
    ctrl: usize,
    group: usize,
    port: u32,
    slot: Slot,
    dead: bool,
    dci: u32,
    /// 上一次的 6 个按键码（boot report 的 byte2..7），用于只上报"新按下"。
    prev: [u8; 6],
    /// 是否已压入一笔等待数据的 Interrupt IN 传输。
    armed: bool,
}

impl KeyboardDev {
    /// 在 HID 中断端点上压一笔 8 字节输入传输（无数据时控制器会一直 NAK，不产生完成事件）。
    fn arm(&mut self, hc: &mut Xhci) {
        let trb = Trb {
            param: self.slot.data_pa,
            status: 8,
            control: (TRB_NORMAL << 10) | (1 << 5) | (1 << 16),
        };
        self.slot.bulkin.push(trb);
        fence();
        wr32(hc.db + (self.slot.slot as u64) * 4, self.dci);
        self.armed = true;
    }

    /// 非阻塞收 HID 报告；收到就把新按下的键推进内核键队列，并重新压一笔传输。
    fn poll(&mut self, hc: &mut Xhci) {
        let erdp = hc.rt + RT_IR0 + IR_ERDP;
        // 存储传输期间完成的事件被 `wait_event` 暂存在控制器上，这里一并收走。
        let mut got = hc.take_pending(self.slot.slot, self.dci).is_some();
        while let Some(e) = hc.evt.poll(erdp) {
            if trb_type(&e) != TRB_TRANSFER_EVENT {
                continue; // 端口状态变化等无关事件
            }
            let slot = (e.control >> 24) & 0xFF;
            let dci = (e.control >> 16) & 0x1F;
            if slot != self.slot.slot || dci != self.dci {
                continue;
            }
            got = true;
        }
        if got {
            let mut rep = [0u8; 8];
            let mut i = 0usize;
            while i < 8 {
                rep[i] = rd8(self.slot.data_va + i as u64);
                i += 1;
            }
            self.emit(&rep);
            if self.armed {
                self.armed = false;
                self.arm(hc);
            }
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
fn log_product_string(hc: &mut Xhci, s: &mut Slot, idx: u8) {
    if idx == 0 {
        return;
    }
    if !hc.control(
        s,
        0x80,
        6,
        (3u16 << 8) | idx as u16,
        0x0409,
        128,
        true,
        s.data_pa,
    ) {
        return;
    }
    let len = rd8(s.data_va) as u64;
    if len < 4 {
        return;
    }
    print("xhci: product=\"");
    let mut i = 2u64;
    while i + 1 < len && i < 128 {
        let c = rd8(s.data_va + i); // UTF-16LE 的低字节；ASCII 足够识别型号
        if (0x20..0x7f).contains(&c) {
            let one = [c];
            print(unsafe { core::str::from_utf8_unchecked(&one) });
        }
        i += 2;
    }
    println("\"");
}

/// 配置 HID 的 Interrupt IN 端点（EP Type = 7）。
fn configure_hid(hc: &mut Xhci, s: &mut Slot, dci: u32, mps: u16, interval: u8) -> bool {
    let (_, ep_base) = clear_input(hc);
    wr32(hc.input_va + 4, 1 | (1 << dci));
    // Slot 上下文原样回填（Device Address 不能丢），只改 Context Entries。
    restore_slot_ctx(hc, &s.slot_ctx, dci);
    let ep = input_ep_ctx(hc, ep_base, dci);
    // EP 上下文 DWORD0：Interval 在 bits 23:16。
    wr32(ep, (interval as u32) << 16);
    // DWORD1：MPS | EP Type=7(Interrupt IN) | CErr=3。
    wr32(ep + 4, ((mps as u32) << 16) | (7 << 3) | (3 << 1));
    wr64(ep + 8, s.bulkin.pa | 1);
    match hc.context_cmd(TRB_CONFIGURE_ENDPOINT, s.slot) {
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

/// 一次启动的 USB 普查汇总（真机没有串口/lsusb，最后一行就是整机拓扑的答案）。
#[derive(Default)]
struct Census {
    ctrls: u32,
    ports: u32,
    connected: u32,
    storage: u32,
    hid: u32,
    hub: u32,
    other: u32,
    errors: u32,
}

/// 一台已寻址设备的描述信息（census 与正式探测共用同一条路径）。
struct Enumerated {
    port: u32,
    /// 设备在 xHCI 拓扑里的 Route String（根端口设备为 0；Hub 下游为 Hub 端口号）。
    route: u32,
    /// 最终挂接的 Root Hub 端口号（Route String 不含根端口）。
    root_port: u32,
    /// 是否经 Hub 接入（只影响 census 的展示）。
    via_hub: bool,
    speed: u32,
    bmax0: u16,
    dev_class: u8,
    /// 设备描述符 bDeviceProtocol（Hub 用它区分 FS / HS / MTT）。
    dev_proto: u8,
    prod_idx: u8,
    vid: u16,
    pid: u16,
    iface: IfaceInfo,
}

/// Address Device 要写进 slot 上下文的拓扑信息。
#[derive(Clone, Copy, Default)]
struct Topo {
    /// Route String（20 位）。
    route: u32,
    /// Root Hub Port Number（DWORD1 bits 23:16）。
    root_port: u32,
    /// Hub 的 Number of Ports（DWORD1 bits 31:24；非 Hub 为 0）。
    num_ports: u8,
    /// Slot Context 的 Hub 位（bit26）。
    hub: bool,
    /// Slot Context 的 MTT 位（bit25）。
    mtt: bool,
    /// FS/LS 设备挂在 HS Hub 下时的 TT 三字段：(Hub Slot ID, 下游端口, Think Time)。
    tt: Option<(u8, u8, u8)>,
    via_hub: bool,
}

/// USB2 Hub 描述符（type 0x29）里本驱动用到的字段。
struct HubDesc {
    ports: u8,
    chars: u16,
    /// bPwrOn2PwrGood（单位 2 ms）。
    pwr_good: u8,
}

/// 对一个**已就绪**的端口执行 Enable Slot → Address Device（含 MPS 重试）→ 读设备/配置
/// 描述符 → 解析接口。失败返回 `None`（失败阶段已打印），并把 slot 释放掉。
///
/// 成功返回时 `s` 的 slot 仍处于 enabled，由调用方决定是继续配置（存储/键盘）还是 Disable。
///
/// `port` 是用于展示的端口号（Hub 下游时是 Hub 的端口号）；`topo` 里带 Address Device 真正
/// 要写的 Root Hub Port / Route String；`hub_reset` 给出「失败重试时该复位哪个 Hub 端口」。
fn enumerate_port(
    hc: &mut Xhci,
    s: &mut Slot,
    port: u32,
    speed: u32,
    topo: Topo,
    mut hub_reset: Option<(&mut Slot, u8)>,
) -> Option<Enumerated> {
    let ev = match hc.simple_cmd(TRB_ENABLE_SLOT, 0, 0) {
        Some(e) => e,
        None => {
            println("xhci: Enable Slot: no command completion event");
            return None;
        }
    };
    if trb_cc(&ev) != CC_SUCCESS {
        print("xhci: Enable Slot FAILED cc=");
        print_u64(trb_cc(&ev) as u64);
        println("");
        return None;
    }
    s.slot = (ev.control >> 24) & 0xFF;
    wr64(hc.dcbaa_va + (s.slot as u64) * 8, s.devctx_pa);

    let default_mps: u16 = match speed {
        1 | 2 => 8,
        4 | 5 => 512,
        _ => 64,
    };
    // EP0 的最大包长可能和「按端口速度推断」的不一致：真机 cc=3 (Babble) + residual=10
    // 说明设备只回了 8 字节（全速默认包长），而我们按高速声明了 64 → 控制器报 babble。
    // 故按 默认 → 8 → 16 → 32 依次重试，每次失败都复位端口、换新 slot 重来。
    let mut mps_list = [default_mps; 4];
    let mut n_mps = 1usize;
    for m in [8u16, 16, 32] {
        if m < default_mps {
            mps_list[n_mps] = m;
            n_mps += 1;
        }
    }
    let mut ready = false;
    let mut ci = 0usize;
    while ci < n_mps {
        let mps = mps_list[ci];
        ci += 1;
        if address_device(hc, s, speed, mps, topo) {
            hc.log("device addressed");
            // USB 2.0 §9.2.6.3：第一次只读 8 字节（设备保证一个包就够），拿到
            // bMaxPacketSize0 后按它更新 EP0，再读完整的 18 字节。跳过这一步，
            // 真机会在 18 字节那次传输上报 cc=3 (Babble)。
            if hc.get_device_descriptor(s, 8) {
                let b0 = rd8(s.data_va + 7) as u16;
                if matches!(b0, 8 | 16 | 32 | 64) && b0 != mps {
                    if evaluate_ep0_mps(hc, s, b0) {
                        print("xhci: EP0 mps -> ");
                        print_u64(b0 as u64);
                        println("");
                    } else {
                        println("xhci: EP0 mps update FAILED (continuing)");
                    }
                }
                if hc.get_device_descriptor(s, 18) {
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
        if let Some((hub, hub_port)) = hub_reset.as_mut() {
            let _ = hc.hub_reset_port(hub, *hub_port);
        } else {
            let sc = hc.reset_port(port);
            hc.log_port(port, "retry", sc);
        }
        let _ = hc.simple_cmd(TRB_DISABLE_SLOT, s.slot, 0);
        match hc.simple_cmd(TRB_ENABLE_SLOT, 0, 0) {
            Some(ev) if trb_cc(&ev) == CC_SUCCESS => {
                s.slot = (ev.control >> 24) & 0xFF;
                wr64(hc.dcbaa_va + (s.slot as u64) * 8, s.devctx_pa);
            }
            _ => break,
        }
        sys_sleep(50);
    }
    if !ready {
        let _ = hc.simple_cmd(TRB_DISABLE_SLOT, s.slot, 0);
        return None;
    }

    let bmax0 = rd8(s.data_va + 7) as u16;
    // ⚠️ 必须在读配置描述符**之前**取：两次读都用 s.data_va，后者会覆盖前者。
    let dev_class = rd8(s.data_va + 4);
    let dev_proto = rd8(s.data_va + 6);
    let prod_idx = rd8(s.data_va + 15);
    let vid = ((rd8(s.data_va + 9) as u16) << 8) | rd8(s.data_va + 8) as u16;
    let pid = ((rd8(s.data_va + 11) as u16) << 8) | rd8(s.data_va + 10) as u16;

    if !hc.control(s, 0x80, 6, 2 << 8, 0, 9, true, s.data_pa) {
        println("xhci: GET_DESCRIPTOR(config,9) FAILED");
        let _ = hc.simple_cmd(TRB_DISABLE_SLOT, s.slot, 0);
        return None;
    }
    let total = (rd8(s.data_va + 2) as u32) | ((rd8(s.data_va + 3) as u32) << 8);
    let want = if total > 512 { 512 } else { total };
    if want < 9 || !hc.control(s, 0x80, 6, 2 << 8, 0, want as u16, true, s.data_pa) {
        println("xhci: GET_DESCRIPTOR(config,full) FAILED");
        let _ = hc.simple_cmd(TRB_DISABLE_SLOT, s.slot, 0);
        return None;
    }
    let iface = parse_config(s.data_va, want);

    Some(Enumerated {
        port,
        route: topo.route,
        root_port: topo.root_port,
        via_hub: topo.via_hub,
        speed,
        bmax0,
        dev_class,
        dev_proto,
        prod_idx,
        vid,
        pid,
        iface,
    })
}

/// 打印一行普查结果（每台设备一行，真机拍照就能还原整棵 USB 树）。
fn census_line(idx: u32, d: &Enumerated, kind: &str) {
    print("xhci: census ctrl#");
    print_u64(idx as u64);
    print(" port=");
    print_u64(d.port as u64);
    print(" route=0x");
    print_hex(d.route as u64);
    if d.via_hub {
        print(" via=hub root=");
        print_u64(d.root_port as u64);
    }
    print(" spd=");
    print_u64(d.speed as u64);
    print(" vid=0x");
    print_hex(d.vid as u64);
    print(" pid=0x");
    print_hex(d.pid as u64);
    print(" class=0x");
    print_hex(d.dev_class as u64);
    print(" ifaces=");
    print_u64(d.iface.n_ifaces as u64);
    print(" bulk=");
    print_u64((d.iface.bulk_in.is_some() && d.iface.bulk_out.is_some()) as u64);
    print(" hid=");
    print_u64(d.iface.hid.is_some() as u64);
    print(" result=");
    println(kind);
}

/// 把一个设备归到 census 的类别里。
fn census_kind(d: &Enumerated) -> &'static str {
    if d.dev_class == 0x09 || d.iface.class == 0x09 {
        "hub"
    } else if d.iface.hid.is_some() {
        "hid"
    } else if d.iface.bulk_in.is_some() && d.iface.bulk_out.is_some() {
        "storage"
    } else {
        "other"
    }
}

/// 初始化一台控制器：读能力寄存器 → 复位 → 配环/ERST/DCBAAP → 启动 → 端口上电。
///
/// **不含**链路训练等待：所有控制器初始化完后由 [`wait_connect_all`] 统一轮询，
/// 避免 13 台空控制器把 250 ms 的空等串行叠加成三秒多。
fn init_controller(g: &DeviceGrant, idx: u32) -> Option<Xhci> {
    if g.dma_bytes < DMA_PAGES * PAGE {
        println("xhci: device grant DMA too small, aborting");
        return None;
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
        dma_va,
        dma_pa,
        cmd: Ring::new(base(CMD_PAGE), pbase(CMD_PAGE)),
        evt: EventRing::new(base(EVT_PAGE), pbase(EVT_PAGE)),
        dcbaa_va: base(DCBAA_PAGE),
        dcbaa_pa: pbase(DCBAA_PAGE),
        input_va: base(INPUT_PAGE),
        input_pa: pbase(INPUT_PAGE),
        bot_va: base(BOT_PAGE),
        bot_pa: pbase(BOT_PAGE),
        ctx_size,
        max_slots,
        num_ports,
        tag: 0,
        pending: [None; 4],
        hotplug_used: 0,
        port_ccs: [0; 8],
        hotplug_synced: false,
    };

    zero_bytes(dma_va as *mut u8, (DMA_PAGES * PAGE) as usize);

    // 先夺回 "USB Legacy Support" 所有权, 再复位控制器 —— 顺序不能反。
    hc.take_legacy();

    if !hc.reset() {
        println("xhci: controller reset FAILED (CNR still set)");
        return None;
    }

    hc.cmd.write_link();

    wr32(op + OP_USBSTS, 0x105C);

    // ERST：1 段，段表项 = {环基址, 环 TRB 数}。
    wr32(rt + RT_IR0 + IR_ERSTSZ, 1);
    wr64(base(ERST_PAGE), pbase(EVT_PAGE));
    wr64(base(ERST_PAGE) + 8, RING_TRBS as u64 & 0xFFFF);
    wr64(rt + RT_IR0 + IR_ERSTBA, pbase(ERST_PAGE));
    wr64(rt + RT_IR0 + IR_ERDP, pbase(EVT_PAGE));

    if scratchpad > 0 {
        if scratchpad > 5 {
            print("xhci: scratchpad too many (");
            print_u64(scratchpad as u64);
            println("), aborting");
            return None;
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

    // 端口上电。**必须在 RS=1 之后**：控制器停机时端口状态机不跑，SS 口会一直停在
    // RxDetect(CCS=0)，之前的 10 ms 上电等待等于没等。链路训练的等待不在这里做 ——
    // 所有控制器由 [`wait_connect_all`] 共享一轮轮询，避免空控制器逐台串行空等。
    let mut p = 1u32;
    while p <= num_ports {
        let reg = op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
        let sc = rd32(reg);
        if sc & PORT_PP == 0 {
            port_write(reg, sc, PORT_PP);
        }
        p += 1;
    }

    Some(hc)
}

/// 扫描一层 USB2 Hub 的下游端口：上电 → 等连接 → 复位 → 枚举下游设备。
///
/// Hub 自己占 `hs` 这个 slot；下游设备用 `group` 这组页。只支持一层（下游再遇到 Hub 只
/// 记 census 不继续扫），Hub 中断端点也不启用 —— 端口变化全靠这里的有界轮询。
fn scan_hub(
    hc: &mut Xhci,
    idx: u32,
    hs: &mut Slot,
    d: &Enumerated,
    group: usize,
    census: &mut Census,
    out: &mut Probed,
) {
    let Some(hd) = hc.hub_descriptor(hs) else {
        println("xhci: hub descriptor FAILED");
        return;
    };
    print("xhci: hub ports=");
    print_u64(hd.ports as u64);
    print(" chars=0x");
    print_hex(hd.chars as u64);
    print(" pwr2good=");
    print_u64(hd.pwr_good as u64);
    print(" proto=");
    print_u64(d.dev_proto as u64);
    println("");

    let think = ((hd.chars >> 4) & 0x3) as u8;
    if !hc.update_hub_ctx(hs, d.dev_proto, hd.ports, think) {
        println("xhci: hub Evaluate Context FAILED (continuing)");
    }

    // 给所有端口上电，然后等 bPwrOn2PwrGood * 2 ms（上限 1 s）。
    let mut q = 1u8;
    while q <= hd.ports {
        let _ = hc.hub_set_feature(hs, q as u16, HUB_PORT_POWER);
        q += 1;
    }
    let mut wait_ms = (hd.pwr_good as u32) * 2;
    if wait_ms > 1000 {
        wait_ms = 1000;
    }
    if wait_ms > 0 {
        sys_sleep(wait_ms as u64);
    }

    // Route String 每层只有 4 位：下游端口号 > 15 的 Hub 不支持（本里程碑足够）。
    let nports = if hd.ports > 15 { 15 } else { hd.ports };
    let mut q = 1u8;
    while q <= nports {
        // 上电后设备可能几百毫秒才出现；空端口不打印，避免 8 行噪音。
        let mut st = 0u32;
        let mut seen = false;
        let mut t = 0u32;
        while t < 200 {
            if let Some(v) = hc.hub_port_status(hs, q as u16) {
                st = v;
                seen = true;
                if v & HUB_CCS != 0 {
                    break;
                }
            }
            sys_sleep(1);
            t += 1;
        }
        if !seen || st & HUB_CCS == 0 {
            q += 1;
            continue;
        }
        census.connected += 1;
        print("xhci: hub port ");
        print_u64(q as u64);
        print(" connected status=0x");
        print_hex(st as u64);
        println("");

        let Some(st2) = hc.hub_reset_port(hs, q) else {
            print("xhci: hub port ");
            print_u64(q as u64);
            println(" reset FAILED");
            census.errors += 1;
            q += 1;
            continue;
        };
        if st2 & HUB_ENABLE == 0 {
            print("xhci: hub port ");
            print_u64(q as u64);
            print(" not enabled after reset status=0x");
            print_hex(st2 as u64);
            println("");
            census.errors += 1;
            q += 1;
            continue;
        }
        let speed = if st2 & HUB_LOW_SPEED != 0 {
            2 // xHCI Speed ID: 2 = Low Speed
        } else if st2 & HUB_HIGH_SPEED != 0 {
            3 // 3 = High Speed
        } else {
            1 // 1 = Full Speed
        };
        print("xhci: hub port ");
        print_u64(q as u64);
        print(" speed=");
        print_u64(speed as u64);
        println("");

        // Route String 只描述根端口之下的 Hub 路径：一层 Hub 时就是下游端口号。
        // 根端口本身在 DWORD1 的 Root Hub Port Number 里，不进入 Route String。
        let route = (q as u32) & 0xF;
        // FS/LS 设备挂在 HS Hub 下必须给 TT 三字段；HS 设备留 0。
        let tt = if d.dev_proto != 0 && (speed == 1 || speed == 2) {
            Some((hs.slot as u8, q, think))
        } else {
            None
        };
        let topo = Topo {
            route,
            root_port: d.port,
            num_ports: 0,
            hub: false,
            mtt: false,
            tt,
            via_hub: true,
        };

        let mut ds = Slot::new(hc, group);
        match enumerate_port(hc, &mut ds, q as u32, speed, topo, Some((hs, q))) {
            Some(dd) => {
                let kind = census_kind(&dd);
                census_line(idx, &dd, kind);
                match kind {
                    "storage" => census.storage += 1,
                    "hid" => census.hid += 1,
                    "hub" => census.hub += 1,
                    _ => census.other += 1,
                }
                log_product_string(hc, &mut ds, dd.prod_idx);
                print_diag(&dd);
                if kind == "hub" {
                    println("xhci: nested hub behind hub - not scanned");
                }
                let mut cap_sectors = 0u64;
                let mut hid_dci = 0u32;
                let mut keep_storage = false;
                let mut keep_hid = false;
                if kind == "storage" && out.storage.is_none() {
                    if let Some(cap) = config_storage(hc, &mut ds, idx, &dd) {
                        cap_sectors = cap;
                        keep_storage = true;
                    }
                } else if kind == "hid" && out.keyboard.is_none() {
                    if let Some(dci) = config_keyboard(hc, &mut ds, idx, &dd) {
                        hid_dci = dci;
                        keep_hid = true;
                    }
                }
                match (keep_storage, keep_hid) {
                    (true, _) => {
                        out.storage = Some(StorageDev {
                            ctrl: idx as usize,
                            group,
                            port: dd.root_port,
                            slot: ds,
                            cap_sectors,
                            dead: false,
                        });
                    }
                    (_, true) => {
                        out.keyboard = Some(KeyboardDev {
                            ctrl: idx as usize,
                            group,
                            port: dd.root_port,
                            slot: ds,
                            dead: false,
                            dci: hid_dci,
                            prev: [0u8; 6],
                            armed: false,
                        });
                    }
                    _ => {
                        let _ = hc.simple_cmd(TRB_DISABLE_SLOT, ds.slot, 0);
                    }
                }
            }
            None => {
                print("xhci: census ctrl#");
                print_u64(idx as u64);
                print(" port=");
                print_u64(q as u64);
                print(" route=0x");
                print_hex(route as u64);
                println(" result=enum-fail");
                census.errors += 1;
                probe_line_port(idx, q as u32, speed, "desc-fail");
            }
        }
        q += 1;
    }
}

/// 链路训练完成之后的后半段：打印端口 → 逐端口寻址/普查 → 识别并配置存储 / HID。
///
/// 每台控制器最多保留两个 slot（一个存储 + 一个 HID），两者都在同一次端口遍历里配好，
/// 不再"普查一次、再挑一个端口重配一次"。census 行与汇总计数保持不变。
fn probe_started(hc: &mut Xhci, idx: u32, census: &mut Census) -> Probed {
    let op = hc.op;
    let num_ports = hc.num_ports;
    // 端口数要在“本控制器有没有插设备”之前统计：空控制器的端口也属于整机拓扑。
    census.ports += num_ports;

    let mut present = false;
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
        return Probed::default();
    }

    // 全端口普查：真机只能靠这几行看清 SuperSpeed / USB2 两条 lane 的分布
    // （USB3 连接器 = 1 个 SS 口 + 1 个 USB2 伴随口，两者会同时报 CCS）。
    let mut cp = 1u32;
    while cp <= num_ports {
        let reg = op + OP_PORT_BASE + ((cp as u64 - 1) * OP_PORT_STRIDE);
        hc.log_port(cp, "census", rd32(reg));
        cp += 1;
    }

    let mut out = Probed::default();
    // 已经保留的 slot 组数（每组页只给一台设备用）；用满后不再寻址新设备。
    let mut next_group = 0usize;
    let mut cp = 1u32;
    while cp <= num_ports {
        let reg = op + OP_PORT_BASE + ((cp as u64 - 1) * OP_PORT_STRIDE);
        let sc = rd32(reg);
        if sc & PORT_CCS == 0 {
            cp += 1;
            continue;
        }
        census.connected += 1;
        let mut sc = sc;
        if !port_ready(sc) {
            sc = hc.reset_port(cp);
        }
        if !port_ready(sc) {
            print("xhci: census ctrl#");
            print_u64(idx as u64);
            print(" port=");
            print_u64(cp as u64);
            println(" result=link-fail");
            census.errors += 1;
            cp += 1;
            continue;
        }
        let speed = (sc >> 10) & 0xF;
        if next_group >= SLOT_GROUPS {
            // 本里程碑只支持每台控制器 2 台设备：再多也不寻址（避免破坏已配置的 slot）。
            print("xhci: census ctrl#");
            print_u64(idx as u64);
            print(" port=");
            print_u64(cp as u64);
            println(" result=no-free-slot");
            census.other += 1;
            cp += 1;
            continue;
        }

        print("xhci: port ");
        print_u64(cp as u64);
        print(" speed=");
        print_u64(speed as u64);
        print(" portsc=0x");
        print_hex(sc as u64);
        println(" connected");

        let topo = Topo {
            route: 0,
            root_port: cp,
            ..Default::default()
        };
        let mut s = Slot::new(hc, next_group);
        match enumerate_port(hc, &mut s, cp, speed, topo, None) {
            Some(d) => {
                let kind = census_kind(&d);
                census_line(idx, &d, kind);
                match kind {
                    "storage" => census.storage += 1,
                    "hid" => census.hid += 1,
                    "hub" => census.hub += 1,
                    _ => census.other += 1,
                }
                log_product_string(hc, &mut s, d.prod_idx);
                print_diag(&d);
                if kind == "hub" {
                    // Hub 自己占一组 slot 页，下游设备再占一组；只有一层。
                    if next_group + 2 <= SLOT_GROUPS {
                        scan_hub(hc, idx, &mut s, &d, next_group + 1, census, &mut out);
                        next_group += 2;
                    } else {
                        println("xhci: hub found but no free slot pair for downstream");
                        let _ = hc.simple_cmd(TRB_DISABLE_SLOT, s.slot, 0);
                    }
                    cp += 1;
                    continue;
                }
                let mut cap_sectors = 0u64;
                let mut hid_dci = 0u32;
                let mut keep_storage = false;
                let mut keep_hid = false;
                if kind == "storage" && out.storage.is_none() {
                    if let Some(cap) = config_storage(hc, &mut s, idx, &d) {
                        cap_sectors = cap;
                        keep_storage = true;
                    }
                } else if kind == "hid" && out.keyboard.is_none() {
                    if let Some(dci) = config_keyboard(hc, &mut s, idx, &d) {
                        hid_dci = dci;
                        keep_hid = true;
                    }
                }
                match (keep_storage, keep_hid) {
                    (true, _) => {
                        out.storage = Some(StorageDev {
                            ctrl: idx as usize,
                            group: next_group,
                            port: cp,
                            slot: s,
                            cap_sectors,
                            dead: false,
                        });
                        next_group += 1;
                    }
                    (_, true) => {
                        out.keyboard = Some(KeyboardDev {
                            ctrl: idx as usize,
                            group: next_group,
                            port: cp,
                            slot: s,
                            dead: false,
                            dci: hid_dci,
                            prev: [0u8; 6],
                            armed: false,
                        });
                        next_group += 1;
                    }
                    _ => {
                        let _ = hc.simple_cmd(TRB_DISABLE_SLOT, s.slot, 0);
                    }
                }
            }
            None => {
                print("xhci: census ctrl#");
                print_u64(idx as u64);
                print(" port=");
                print_u64(cp as u64);
                println(" result=enum-fail");
                census.errors += 1;
                probe_line_port(idx, cp, speed, "desc-fail");
            }
        }
        cp += 1;
    }

    if out.storage.is_none() && out.keyboard.is_none() {
        print("xhci: ctrl#");
        print_u64(idx as u64);
        println(": no supported USB device");
        probe_line(idx, 0, 0, "no-device");
    }
    // 记下本控制器已占用的 slot 组，供运行期热插拔找空位。
    hc.hotplug_used = next_group as u32;
    out
}

/// 运行期热插拔：根端口 PORTSC 轮询 + 新设备枚举 / 拔出标记。
///
/// 只在两笔 block 请求之间调用（不进 `serve_rw`），故不会打断正在进行的存储传输。
/// 不消费事件环 —— 端口事件的清理由这里读 PORTSC 后写 CSC/PRC 完成。
impl Xhci {
    fn port_ccs_get(&self, p: u32) -> bool {
        let i = ((p - 1) / 32) as usize;
        (self.port_ccs[i] >> ((p - 1) % 32)) & 1 != 0
    }

    fn port_ccs_set(&mut self, p: u32, on: bool) {
        let i = ((p - 1) / 32) as usize;
        let bit = 1u32 << ((p - 1) % 32);
        if on {
            self.port_ccs[i] |= bit;
        } else {
            self.port_ccs[i] &= !bit;
        }
    }

    /// 拍下所有根端口的当前 CCS，作为「什么算变化」的基线（不打印）。
    ///
    /// 顺手清掉启动期遗留的 CSC/PRC：这些变化对应的设备已经在 `probe_started` 里处理过，
    /// 不清的话第一次轮询会把全部端口当成"刚变化"各打印一行噪音。
    fn hotplug_baseline(&mut self) {
        self.port_ccs = [0; 8];
        let mut p = 1u32;
        while p <= self.num_ports && p <= 256 {
            let reg = self.op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
            let sc = rd32(reg);
            let ccs = sc & PORT_CCS != 0;
            self.port_ccs_set(p, ccs);
            if sc & (PORT_CSC | PORT_PRC) != 0 {
                port_write(reg, sc, PORT_CSC | PORT_PRC);
            }
            p += 1;
        }
        self.hotplug_synced = true;
    }

    /// 打印一行 `xhci: hotplug ctrl#N port=P ccs=0/1`（验收日志格式）。
    fn hotplug_log(&self, idx: u32, p: u32, ccs: bool) {
        print("xhci: hotplug ctrl#");
        print_u64(idx as u64);
        print(" port=");
        print_u64(p as u64);
        print(" ccs=");
        print_u64(ccs as u64);
        println("");
    }

    /// 打印一行热插拔结论（`removed` / `result=...`）。
    fn hotplug_note(&self, idx: u32, p: u32, note: &str) {
        print("xhci: hotplug ctrl#");
        print_u64(idx as u64);
        print(" port=");
        print_u64(p as u64);
        print(" ");
        println(note);
    }

    /// 找一组空闲 slot 页。没有空位时只复用本控制器上已断开（dead）设备的组 ——
    /// 不做复杂的 slot 资源回收。
    fn hotplug_alloc_group(
        &mut self,
        idx: u32,
        storage: &mut Option<StorageDev>,
        keyboard: &mut Option<KeyboardDev>,
    ) -> Option<usize> {
        let mut g = 0usize;
        while g < SLOT_GROUPS {
            if self.hotplug_used & (1 << g) == 0 {
                self.hotplug_used |= 1 << g;
                return Some(g);
            }
            g += 1;
        }
        let mut reuse = None;
        if let Some(k) = keyboard.as_ref() {
            if k.dead && k.ctrl == idx as usize {
                reuse = Some(k.group);
            }
        }
        if let Some(g) = reuse {
            *keyboard = None;
            return Some(g);
        }
        if let Some(s) = storage.as_ref() {
            if s.dead && s.ctrl == idx as usize {
                reuse = Some(s.group);
            }
        }
        if let Some(g) = reuse {
            *storage = None;
            return Some(g);
        }
        None
    }

    /// 释放刚分配但最终没用上的组（禁用 slot + 清位图）。
    fn hotplug_release(&mut self, g: usize, s: &mut Slot) {
        let _ = self.simple_cmd(TRB_DISABLE_SLOT, s.slot, 0);
        self.hotplug_used &= !(1u32 << g);
    }

    /// 新设备接入：复位端口 → 完整枚举 → 按类别配置存储 / 键盘。
    fn hotplug_attach(
        &mut self,
        idx: u32,
        p: u32,
        storage: &mut Option<StorageDev>,
        keyboard: &mut Option<KeyboardDev>,
    ) {
        let reg = self.op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
        let mut sc = rd32(reg);
        if !port_ready(sc) {
            sc = self.reset_port(p);
        }
        if !port_ready(sc) {
            self.hotplug_note(idx, p, "result=link-fail");
            return;
        }
        let speed = (sc >> 10) & 0xF;
        let Some(g) = self.hotplug_alloc_group(idx, storage, keyboard) else {
            self.hotplug_note(idx, p, "result=no-free-slot");
            return;
        };
        let topo = Topo {
            route: 0,
            root_port: p,
            ..Default::default()
        };
        let mut s = Slot::new(self, g);
        let Some(d) = enumerate_port(self, &mut s, p, speed, topo, None) else {
            self.hotplug_used &= !(1u32 << g);
            self.hotplug_note(idx, p, "result=enum-fail");
            return;
        };
        match census_kind(&d) {
            "storage" if storage.is_none() => {
                if let Some(cap_sectors) = config_storage(self, &mut s, idx, &d) {
                    *storage = Some(StorageDev {
                        ctrl: idx as usize,
                        group: g,
                        port: p,
                        slot: s,
                        cap_sectors,
                        dead: false,
                    });
                    self.hotplug_note(idx, p, "result=storage");
                } else {
                    self.hotplug_release(g, &mut s);
                    self.hotplug_note(idx, p, "result=storage-fail");
                }
            }
            "hid" if keyboard.is_none() => {
                if let Some(dci) = config_keyboard(self, &mut s, idx, &d) {
                    let mut k = KeyboardDev {
                        ctrl: idx as usize,
                        group: g,
                        port: p,
                        slot: s,
                        dead: false,
                        dci,
                        prev: [0u8; 6],
                        armed: false,
                    };
                    k.arm(self);
                    *keyboard = Some(k);
                    self.hotplug_note(idx, p, "result=hid");
                } else {
                    self.hotplug_release(g, &mut s);
                    self.hotplug_note(idx, p, "result=hid-fail");
                }
            }
            "hub" => {
                self.hotplug_release(g, &mut s);
                self.hotplug_note(idx, p, "result=hub-unsupported");
            }
            _ => {
                self.hotplug_release(g, &mut s);
                self.hotplug_note(idx, p, "result=other");
            }
        }
    }

    /// 设备拔出：标记正在服务的设备为 dead（后续请求直接失败，不 panic）。
    fn hotplug_detach(
        &mut self,
        idx: u32,
        p: u32,
        storage: &mut Option<StorageDev>,
        keyboard: &mut Option<KeyboardDev>,
    ) {
        if let Some(k) = keyboard.as_mut() {
            if k.ctrl == idx as usize && k.port == p {
                k.dead = true;
            }
        }
        if let Some(s) = storage.as_mut() {
            if s.ctrl == idx as usize && s.port == p {
                s.dead = true;
            }
        }
        self.hotplug_note(idx, p, "removed");
    }

    /// 轮询一台控制器的所有根端口：CSC/PRC 有变化就打印并清除；
    /// CCS 0→1 枚举新设备，1→0 标记拔出。
    fn hotplug_poll(
        &mut self,
        idx: u32,
        storage: &mut Option<StorageDev>,
        keyboard: &mut Option<KeyboardDev>,
    ) {
        if !self.hotplug_synced {
            self.hotplug_baseline();
        }
        let mut p = 1u32;
        while p <= self.num_ports && p <= 256 {
            let reg = self.op + OP_PORT_BASE + ((p as u64 - 1) * OP_PORT_STRIDE);
            let sc = rd32(reg);
            let ccs = sc & PORT_CCS != 0;
            let changed = sc & (PORT_CSC | PORT_PRC) != 0;
            let last = self.port_ccs_get(p);
            if !changed && ccs == last {
                p += 1;
                continue;
            }
            self.hotplug_log(idx, p, ccs);
            port_write(reg, sc, PORT_CSC | PORT_PRC);
            self.port_ccs_set(p, ccs);
            if ccs == last {
                // 只是复位完成（PRC）或残留变化位，设备身份没有变化。
                p += 1;
                continue;
            }
            if ccs {
                self.hotplug_attach(idx, p, storage, keyboard);
            } else {
                self.hotplug_detach(idx, p, storage, keyboard);
            }
            p += 1;
        }
    }
}

/// 打印一台设备的描述符 / 配置摘要（真机没有 `lsusb`，这几行就是拓扑证据）。
fn print_diag(d: &Enumerated) {
    print("xhci: device descriptor vid=0x");
    print_hex(d.vid as u64);
    print(" pid=0x");
    print_hex(d.pid as u64);
    print(" bMaxPacket0=");
    print_u64(d.bmax0 as u64);
    println("");
    print("xhci: config ifaces=");
    print_u64(d.iface.n_ifaces as u64);
    print(" class=0x");
    print_hex(d.iface.class as u64);
    print(" sub=0x");
    print_hex(d.iface.subclass as u64);
    print(" proto=0x");
    print_hex(d.iface.proto as u64);
    print(" devclass=0x");
    print_hex(d.dev_class as u64);
    print(" bulk_in=");
    print_u64(d.iface.bulk_in.is_some() as u64);
    print(" bulk_out=");
    print_u64(d.iface.bulk_out.is_some() as u64);
    print(" hid=");
    print_u64(d.iface.hid.is_some() as u64);
    println("");
}

/// 把一台已寻址的存储设备配置成 BOT + SCSI 并挂进 block_srv；成功返回容量扇区数。
fn config_storage(hc: &mut Xhci, s: &mut Slot, idx: u32, d: &Enumerated) -> Option<u64> {
    let (bin, bout) = match (d.iface.bulk_in, d.iface.bulk_out) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            println("xhci: device ignored (unsupported interface class)");
            probe_line(idx, d.vid, d.pid, "other");
            return None;
        }
    };
    s.bulkin_dci = bin.dci;
    s.bulkout_dci = bout.dci;
    s.bulkin_mps = bin.mps;
    s.bulkout_mps = bout.mps;
    print("xhci: bulk-in dci=");
    print_u64(bin.dci as u64);
    print(" mps=");
    print_u64(bin.mps as u64);
    print(" out dci=");
    print_u64(bout.dci as u64);
    print(" mps=");
    print_u64(bout.mps as u64);
    println("");

    if !hc.control(s, 0x00, 9, 1, 0, 0, false, 0) {
        println("xhci: SET_CONFIGURATION FAILED");
        probe_line(idx, d.vid, d.pid, "storage-setcfg-fail");
        return None;
    }
    if !configure_endpoints(hc, s) {
        println("xhci: Configure Endpoint FAILED");
        probe_line(idx, d.vid, d.pid, "storage-cfgep-fail");
        return None;
    }
    hc.log("bulk endpoints configured");

    if !hc.scsi_inquiry(s) {
        println("xhci: INQUIRY FAILED");
        probe_line(idx, d.vid, d.pid, "storage-inquiry-fail");
        return None;
    }
    print("xhci: inquiry vendor=");
    print_sig(&{
        let mut v = [b' '; 16];
        let mut i = 0usize;
        while i < 8 {
            v[i] = rd8(s.data_va + 8 + i as u64);
            i += 1;
        }
        v
    });
    println("");

    let (last_lba, blk) = match hc.scsi_read_capacity(s) {
        Some(v) => v,
        None => {
            println("xhci: READ CAPACITY FAILED");
            probe_line(idx, d.vid, d.pid, "storage-capacity-fail");
            return None;
        }
    };
    let cap_sectors = (last_lba as u64) + 1;
    print("xhci: capacity sectors=");
    print_u64(cap_sectors);
    print(" blk=");
    print_u64(blk as u64);
    println("");

    if !hc.scsi_read10(s, 0, 1) {
        println("xhci: READ(10) sector 0 FAILED");
        return None;
    }
    let sector = unsafe { core::slice::from_raw_parts(s.data_va as *const u8, 512) };
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
        probe_line(idx, d.vid, d.pid, "attach-fail");
        return None;
    }

    probe_line(idx, d.vid, d.pid, "storage");
    Some(cap_sectors)
}

/// 把一台已寻址的 HID 设备配置成键盘（Interrupt IN + boot 协议）；成功返回 DCI。
fn config_keyboard(hc: &mut Xhci, s: &mut Slot, idx: u32, d: &Enumerated) -> Option<u32> {
    let (dci, mps, interval) = d.iface.hid?;
    if !d.iface.hid_boot {
        println("xhci: HID is not boot subtype - trying anyway");
    }
    if !hc.control(s, 0x00, 9, 1, 0, 0, false, 0) {
        println("xhci: kbd SET_CONFIGURATION FAILED");
        probe_line(idx, d.vid, d.pid, "hid-setcfg-fail");
        return None;
    }
    if !configure_hid(hc, s, dci, mps, interval) {
        println("xhci: kbd Configure Endpoint FAILED");
        probe_line(idx, d.vid, d.pid, "hid-cfgep-fail");
        return None;
    }
    // SET_PROTOCOL(boot)；个别键盘不认，仍按 boot report 尝试。
    if !hc.control(s, 0x21, 0x0B, 0, 0, 0, false, 0) {
        println("xhci: kbd SET_PROTOCOL FAILED (still trying report format)");
    }
    print("xhci: USB HID keyboard ready (port=");
    print_u64(d.port as u64);
    print(", slot=");
    print_u64(s.slot as u64);
    print(", dci=");
    print_u64(dci as u64);
    print(") interval=");
    print_u64(interval as u64);
    println("");
    probe_line(idx, d.vid, d.pid, "hid-keyboard");
    Some(dci)
}

/// 域 19 — xhci_srv：多控制器 xHCI 驱动（USB 存储 + HID 键盘）。
pub fn run() {
    let mut storage: Option<StorageDev> = None;
    let mut keyboard: Option<KeyboardDev> = None;
    let mut census = Census::default();

    // 阶段一：把所有有效控制器的复位/启动/端口上电做完（彼此不等链路训练）。
    let mut ctrls: [Option<Xhci>; libdevice::grant::DEVICE_MULTI_SLOTS as usize] =
        core::array::from_fn(|_| None);
    let mut slot = 0u32;
    while slot < libdevice::grant::DEVICE_MULTI_SLOTS {
        let g = DeviceGrant::load_at(slot);
        if !g.is_valid() {
            slot += 1;
            continue;
        }
        census.ctrls += 1;
        ctrls[slot as usize] = init_controller(&g, slot);
        slot += 1;
    }

    // 阶段二：所有控制器的链路训练等待合并成一轮全局轮询。
    wait_connect_all(&mut ctrls);

    // 阶段三：逐个做端口普查 / 设备识别。控制器状态留在 `ctrls` 里 ——
    // 同一台控制器上的存储与键盘共享它（事件环 / 命令环 / DCBAA）。
    let mut slot = 0u32;
    while slot < libdevice::grant::DEVICE_MULTI_SLOTS {
        if let Some(hc) = ctrls[slot as usize].as_mut() {
            let p = probe_started(hc, slot, &mut census);
            if storage.is_none() {
                storage = p.storage;
            }
            if keyboard.is_none() {
                keyboard = p.keyboard;
            }
        }
        slot += 1;
    }

    // 整机 USB 拓扑汇总（拍照即可，无需串口）。
    if census.ctrls > 0 {
        print("USB REPORT: ctrls=");
        print_u64(census.ctrls as u64);
        print(" ports=");
        print_u64(census.ports as u64);
        print(" connected=");
        print_u64(census.connected as u64);
        print(" storage=");
        print_u64(census.storage as u64);
        print(" hid=");
        print_u64(census.hid as u64);
        print(" hub=");
        print_u64(census.hub as u64);
        print(" other=");
        print_u64(census.other as u64);
        print(" errors=");
        print_u64(census.errors as u64);
        println("");
    }

    if storage.is_none() && keyboard.is_none() {
        println("xhci: no supported USB device (storage/keyboard), idle");
        // 一台控制器都没有就没有可轮询的根端口；控制器存在时仍进服务循环，
        // 这样空控制器上后插的设备也能被发现（热插拔不依赖启动时已有设备）。
        if ctrls.iter().all(|c| c.is_none()) {
            idle();
        }
    }

    // 键盘：压入第一笔等待数据的传输。
    if let Some(k) = keyboard.as_mut() {
        if let Some(hc) = ctrls[k.ctrl].as_mut() {
            k.arm(hc);
        }
    }

    // 服务循环：每轮先收 HID 报告，再在**两笔 block 请求之间**做一次根端口热插拔检查，
    // 最后非阻塞收 block_srv 的读/写请求。只有键盘（无存储）时也走这里，空闲睡久一点。
    loop {
        if let Some(k) = keyboard.as_mut() {
            if !k.dead {
                if let Some(hc) = ctrls[k.ctrl].as_mut() {
                    k.poll(hc);
                }
            }
        }
        // 热插拔检查不进入 serve_rw：读 PORTSC / 清 CSC/PRC，新设备在这里完成枚举。
        let mut ci = 0usize;
        while ci < ctrls.len() {
            if let Some(hc) = ctrls[ci].as_mut() {
                hc.hotplug_poll(ci as u32, &mut storage, &mut keyboard);
            }
            ci += 1;
        }
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        let tag = sys_try_recv(&mut msg as *mut Message as *mut u8);
        if tag == u64::MAX {
            sys_sleep(if storage.is_some() { 1 } else { 5 });
            continue;
        }
        if msg.tag != BLOCK_REQ_TAG {
            sys_reply(0);
            continue;
        }
        let req: BlockReq =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const BlockReq) };
        let ok = match storage.as_mut() {
            // 已拔出的存储：不再向它发请求，直接回失败（不 panic）。
            Some(st) if !st.dead => {
                let _ = st.cap_sectors;
                match (req.op & 0xFF) as u8 {
                    BLOCK_OP_READ => match ctrls[st.ctrl].as_mut() {
                        Some(hc) => serve_rw(hc, &mut st.slot, true, req.lba, req.count),
                        None => false,
                    },
                    BLOCK_OP_WRITE => match ctrls[st.ctrl].as_mut() {
                        Some(hc) => serve_rw(hc, &mut st.slot, false, req.lba, req.count),
                        None => false,
                    },
                    _ => false,
                }
            }
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
fn serve_rw(hc: &mut Xhci, s: &mut Slot, is_read: bool, lba: u64, count: u64) -> bool {
    if count == 0 || count > XHCI_MAX_SECTORS {
        return false;
    }
    let bytes = (count * SECTOR as u64) as usize;
    if is_read {
        if !hc.scsi_read10(s, lba as u32, count as u16) {
            return false;
        }
        copy_buf(XHCI_SCRATCH_VADDR, s.data_va, bytes);
        true
    } else {
        copy_buf(s.data_va, XHCI_SCRATCH_VADDR, bytes);
        hc.scsi_write10(s, lba as u32, count as u16)
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
fn restore_slot_ctx(hc: &Xhci, saved: &[u32; 8], entries: u32) {
    let slot_ctx = hc.input_va + hc.ctx_size;
    let mut i = 0usize;
    while i < 8 {
        wr32(slot_ctx + (i as u64) * 4, saved[i]);
        i += 1;
    }
    let d0 = (saved[0] & !(0x1F << 27)) | ((entries & 0x1F) << 27);
    wr32(slot_ctx, d0);
}

/// Address Device：装配 slot + EP0 上下文并发命令。
fn address_device(hc: &mut Xhci, s: &mut Slot, speed: u32, mps: u16, topo: Topo) -> bool {
    let (slot_ctx, ep_base) = clear_input(hc);
    // 每次（重新）寻址都把 EP0 传输环清零重来：上一次失败的控制传输 TD 可能还挂在
    // 环里，新 slot 的 EP0 从环首执行时读到的是**旧的** SETUP/DATA/STATUS —— 表现就是
    // 首笔 GET_DESCRIPTOR 永远等不到完成事件（真机实测 `EP0 req=6 no completion event`）。
    zero_bytes(s.ep0.va as *mut u8, PAGE as usize);
    s.ep0.enq = 0;
    s.ep0.pcs = true;
    s.ep0.write_link();
    // 输入控制上下文：Add flags = A0(slot) | A1(EP0)。
    wr32(hc.input_va + 4, 0b11);
    // Slot 上下文 DWORD0：Context Entries=1（只配 EP0）+ Speed（bits 23:20）+
    // Route String（bits 19:0）+ MTT（bit25）+ Hub（bit26）。
    // DWORD1：Root Hub Port Number 在 bits 23:16（**不是** bits 15:8，
    // 15:8 是 TT Port Number）；Number of Ports 在 bits 31:24。
    // DWORD2：TT Hub Slot ID(bits 7:0) / TT Port Number(bits 15:8) / TT Think Time(bits 17:16)。
    // 写错端口则 QEMU 读到端口 0 → Address Device 回 TRB Error。
    let mut ctx = [0u32; 8];
    ctx[0] = (1 << 27)
        | (speed << 20)
        | (topo.route & 0x000F_FFFF)
        | (u32::from(topo.mtt) << 25)
        | (u32::from(topo.hub) << 26);
    ctx[1] = (topo.root_port << 16) | ((topo.num_ports as u32) << 24);
    let d2 = match topo.tt {
        Some((hub_slot, tt_port, think)) => {
            (hub_slot as u32) | ((tt_port as u32) << 8) | (((think & 3) as u32) << 16)
        }
        None => 0,
    };
    ctx[2] = d2;
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
    wr64(ep0 + 8, s.ep0.pa | 1);

    match hc.context_cmd(TRB_ADDRESS_DEVICE, s.slot) {
        Some(e) => {
            let cc = trb_cc(&e);
            if cc == CC_SUCCESS {
                s.slot_ctx = ctx;
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
fn evaluate_ep0_mps(hc: &mut Xhci, s: &mut Slot, mps: u16) -> bool {
    let (_, ep_base) = clear_input(hc);
    wr32(hc.input_va + 4, 0b11); // A0 slot + A1 EP0
    restore_slot_ctx(hc, &s.slot_ctx, 1);
    let ep0 = input_ep_ctx(hc, ep_base, 1);
    wr32(ep0 + 4, ((mps as u32) << 16) | (4 << 3) | (3 << 1));
    // ⚠️ 必须交回**当前**入队位置：写环首会让控制器重放已完成的 TRB。
    wr64(ep0 + 8, s.ep0.deq_pa());
    match hc.context_cmd(TRB_EVALUATE_CONTEXT, s.slot) {
        Some(e) => trb_cc(&e) == CC_SUCCESS,
        None => false,
    }
}

/// Configure Endpoint：把两条 Bulk 端点加进设备上下文。
fn configure_endpoints(hc: &mut Xhci, s: &mut Slot) -> bool {
    let (_, ep_base) = clear_input(hc);
    let highest = if s.bulkin_dci > s.bulkout_dci {
        s.bulkin_dci
    } else {
        s.bulkout_dci
    };
    // Add flags：slot + 两条 Bulk 的 DCI 位（EP0 已配，不再 add）。
    let add = (1u32 << 0) | (1u32 << s.bulkin_dci) | (1u32 << s.bulkout_dci);
    wr32(hc.input_va + 4, add);
    // Slot 上下文：原样回填（含 Device Address / Speed / Port），只改 Context Entries。
    restore_slot_ctx(hc, &s.slot_ctx, highest);

    // Bulk-In：DWORD1 = MPS(bits 31:16) | EP Type(bits 5:3)=6 | CErr(bits 2:1)=3。
    let epin = input_ep_ctx(hc, ep_base, s.bulkin_dci);
    wr32(
        epin + 4,
        ((s.bulkin_mps as u32) << 16) | (6 << 3) | (3 << 1),
    );
    wr64(epin + 8, s.bulkin.pa | 1);
    // Bulk-Out：EP Type=2。
    let epout = input_ep_ctx(hc, ep_base, s.bulkout_dci);
    wr32(
        epout + 4,
        ((s.bulkout_mps as u32) << 16) | (2 << 3) | (3 << 1),
    );
    wr64(epout + 8, s.bulkout.pa | 1);

    match hc.context_cmd(TRB_CONFIGURE_ENDPOINT, s.slot) {
        Some(e) => trb_cc(&e) == CC_SUCCESS,
        None => false,
    }
}
