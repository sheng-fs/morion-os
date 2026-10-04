//! 共享的 **Intel 8254x** 网卡驱动核心（e1000 = 82540EM / e1000e = 82574L）。
//!
//! 目的：拿 D1 的**通用设备授权**（`DeviceGrant`）驱动一台与 virtio-net **硬件模型完全不同**
//! 的网卡 —— 内核只按类/型号找到网卡、交出 BAR0 + 连续 DMA 块（无 virtio 能力链表），设备协议
//! （MMIO 寄存器 + 传统 RX/TX 描述符环）完全在本域。这正证明"**驱动 ≠ 栈**"：上层协议栈经同一套
//! 帧级 IPC（`NetReq`：收发裸以太帧 / 取 MAC）即可换用任意网卡。
//!
//! 82540EM 与 82574L 同属 8254x 家族、寄存器模型一致，故本模块被 `e1000_srv` 与 `e1000e_srv`
//! **共用**（差异只在 PCI 型号 / 日志 marker，由调用方传入 `name` / `marker`）。
//!
//! 第一版**全轮询**（本内核只有 MSI-X 通路，8254x 常态用 INTx/MSI，故不申请向量）。
//!
//! 自测：复位 → 读 MAC（EEPROM，回落 RAL/RAH）→ 建 RX/TX 描述符环 → 广播 ARP 请求问网关
//! → 收 ARP 应答（端到端取证 TX + RX 整条链路）→ marker `<marker> OK …`。

use libdevice::grant::DeviceGrant;
use libdevice::mmio::{fence, rd32, rd8, wr16, wr32, wr8};
use morion::syscall::*;
// 显式从公共模块引入 `PAYLOAD_LEN`（不依赖 `crate::common` 的私有 glob 再导出）。
use morion::syscall::PAYLOAD_LEN;

use crate::common::{
    Message, NetReq, NET_FRAME_MAX, NET_OP_INFO, NET_OP_RX, NET_OP_TX, NET_REQ_TAG,
};

// ===========================================================================
// 寄存器偏移（BAR0 MMIO）与位定义（Intel 8254x 数据手册）
// ===========================================================================

const REG_CTRL: u64 = 0x0000;
const REG_STATUS: u64 = 0x0008;
const REG_EERD: u64 = 0x0014; // EEPROM 读寄存器
const REG_IMC: u64 = 0x00D8; // 中断屏蔽（写 1 关对应中断）
const REG_RCTL: u64 = 0x0100; // 接收控制
const REG_TCTL: u64 = 0x0400; // 发送控制
const REG_RDBAL: u64 = 0x2800;
const REG_RDBAH: u64 = 0x2804;
const REG_RDLEN: u64 = 0x2808;
const REG_RDH: u64 = 0x2810;
const REG_RDT: u64 = 0x2818;
const REG_TDBAL: u64 = 0x3800;
const REG_TDBAH: u64 = 0x3804;
const REG_TDLEN: u64 = 0x3808;
const REG_TDH: u64 = 0x3810;
const REG_TDT: u64 = 0x3818;
const REG_MTA: u64 = 0x5200; // 多播地址表（128 × u32）
const REG_RAL0: u64 = 0x5400; // 接收地址低 32 位
const REG_RAH0: u64 = 0x5404; // 接收地址高 16 位 + AV

const CTRL_RST: u32 = 1 << 26;
const CTRL_SLU: u32 = 1 << 6; // Set Link Up（82540EM 不置可能不发帧）
const RAH_AV: u32 = 1 << 31; // 地址有效
const RCTL_EN: u32 = 1 << 1;
const RCTL_BAM: u32 = 1 << 15; // 接收广播
const RCTL_SECRC: u32 = 1 << 26; // 剥掉 CRC
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3; // pad 短帧
const TCTL_CT: u32 = 0x10 << 4; // 冲突阈值
const TCTL_COLD: u32 = 0x40 << 12; // 冲突距离
const EERD_START: u32 = 1 << 0;
const EERD_DONE: u32 = 1 << 1;

/// 传统描述符（16 字节）：TX 的 cmd / RX 的 status 字段。
const TX_CMD_EOP: u8 = 1 << 0;
const TX_CMD_IFCS: u8 = 1 << 1; // 硬件补 FCS
const TX_CMD_RS: u8 = 1 << 3; // 完成后回写 status
const DESC_DD: u8 = 1 << 0; // done
const DESC_EOP: u8 = 1 << 1; // end of packet（RX）

// ===========================================================================
// 环与缓冲布局（本驱动自己决定，内核不参与）
// ===========================================================================

const PAGE: u64 = 4096;
/// 描述符数量（取 2 的幂；RX/TX 各一环）。
const Q_SIZE: u64 = 8;
/// 每个描述符 16 字节。
const DESC_SZ: u64 = 16;
/// RX 环页；TX 环页。
const RX_RING_PAGE: u64 = 0;
const TX_RING_PAGE: u64 = 1;
/// RX 缓冲起点页；每格 `BUF_SZ` 字节（8 × 2048 = 16 KiB = 4 页）。
const RX_BUF_PAGE: u64 = 2;
const BUF_SZ: u64 = 2048;
/// TX 缓冲页（发 ARP / 转发帧用）。
const TX_BUF_PAGE: u64 = 6;
/// 需要的 DMA 页数（与 `main.rs` 里给网卡声明的 `dma_pages: 8` 一致）。
const DMA_PAGES: u64 = 8;

const ETH_ARP: u16 = 0x0806;
/// 本机静态地址（与 virtio-net 侧的隐含约定一致：slirp guest `10.0.2.15` / 网关 `10.0.2.2`）。
const OUR_IP: [u8; 4] = [10, 0, 2, 15];
const GW_IP: [u8; 4] = [10, 0, 2, 2];

// ---------------------------------------------------------------------------
// 打印助手
// ---------------------------------------------------------------------------

const HEX: &[u8; 16] = b"0123456789abcdef";
fn put_byte_hex(b: u8) {
    let d = [HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]];
    print(unsafe { core::str::from_utf8_unchecked(&d) });
}
/// 按 `aa:bb:cc:dd:ee:ff` 打印 MAC（低字节 = 首字节）。
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
// 驱动状态
// ---------------------------------------------------------------------------

/// 一台 8254x 网卡：寄存器窗口 + DMA 窗口 + 环游标。
struct Nic {
    bar: u64,
    dma_vaddr: u64,
    dma_paddr: u64,
    mac: u64,
    /// 下一个要填的 TX 描述符下标。
    tx_tail: u64,
    /// 下一个要检查/回收的 RX 描述符下标。
    rx_next: u64,
}

fn idle() -> ! {
    loop {
        sys_sleep(500);
    }
}

/// 从 EEPROM 读一个 16 位字（地址 `addr`）；超时返回 0。
fn eeprom_read(bar: u64, addr: u8) -> u16 {
    wr32(bar + REG_EERD, ((addr as u32) << 8) | EERD_START);
    let mut spins = 0u32;
    loop {
        let v = rd32(bar + REG_EERD);
        if (v & EERD_DONE) != 0 {
            return ((v >> 16) & 0xFFFF) as u16;
        }
        spins += 1;
        if spins > 100_000 {
            return 0;
        }
    }
}

/// 从 EEPROM 前三个字拼出 MAC（低字节 = 首字节）。
fn read_mac_eeprom(bar: u64) -> u64 {
    let w0 = eeprom_read(bar, 0) as u64;
    let w1 = eeprom_read(bar, 1) as u64;
    let w2 = eeprom_read(bar, 2) as u64;
    w0 | (w1 << 16) | (w2 << 32)
}

/// 从 RAL0/RAH0 读 MAC（复位后由硬件按配置装载，是权威来源）。
fn read_mac_ral(bar: u64) -> u64 {
    let ral = rd32(bar + REG_RAL0) as u64;
    let rah = rd32(bar + REG_RAH0) as u64 & 0xFFFF;
    ral | (rah << 32)
}

/// MAC 是否可用（非全 0、非全 1）。
fn mac_ok(mac: u64) -> bool {
    let m = mac & 0xFFFF_FFFF_FFFF;
    m != 0 && m != 0xFFFF_FFFF_FFFF
}

impl Nic {
    /// 复位 + 读 MAC + 建 RX/TX 环。失败返回 `None`。
    fn attach(g: &DeviceGrant) -> Option<Nic> {
        if g.dma_bytes < DMA_PAGES * PAGE {
            return None;
        }
        let bar = g.bar_vaddr;

        // 1. 关全部中断（本驱动轮询），软复位并等 RST 自清。
        wr32(bar + REG_IMC, 0xFFFF_FFFF);
        wr32(bar + REG_CTRL, rd32(bar + REG_CTRL) | CTRL_RST);
        let mut spins = 0u32;
        loop {
            if (rd32(bar + REG_CTRL) & CTRL_RST) == 0 {
                break;
            }
            spins += 1;
            if spins > 1_000_000 {
                break;
            }
        }

        // 2. 复位后置 SLU (Set Link Up)：82540EM 不置可能丢弃首帧。
        wr32(bar + REG_CTRL, rd32(bar + REG_CTRL) | CTRL_SLU);

        // 3. 读 MAC：RAL/RAH 权威（复位后由硬件按配置装载），无效则回落 EEPROM 前三个字。
        let mut mac = read_mac_ral(bar);
        if !mac_ok(mac) {
            mac = read_mac_eeprom(bar);
        }
        if !mac_ok(mac) {
            return None;
        }

        // 4. 接收地址过滤（本机单播）+ 清多播表。
        wr32(bar + REG_RAL0, (mac & 0xFFFF_FFFF) as u32);
        wr32(bar + REG_RAH0, ((mac >> 32) as u32 & 0xFFFF) | RAH_AV);
        let mut i = 0u64;
        while i < 128 {
            wr32(bar + REG_MTA + i * 4, 0);
            i += 1;
        }

        let nic = Nic {
            bar,
            dma_vaddr: g.dma_vaddr,
            dma_paddr: g.dma_paddr,
            mac,
            tx_tail: 0,
            rx_next: 0,
        };

        // 5. RX 环：每个描述符指向独立缓冲；RDT 指向最后一格（其余交给硬件）。
        let rx_ring = nic.dma_vaddr + RX_RING_PAGE * PAGE;
        let rx_ring_pa = nic.dma_paddr + RX_RING_PAGE * PAGE;
        let mut k = 0u64;
        while k < Q_SIZE {
            let d = rx_ring + k * DESC_SZ;
            let buf_pa = nic.dma_paddr + RX_BUF_PAGE * PAGE + k * BUF_SZ;
            wr32(d, buf_pa as u32);
            wr32(d + 4, (buf_pa >> 32) as u32);
            wr16(d + 8, 0); // length（硬件回填）
            wr8(d + 12, 0); // status
            wr8(d + 13, 0); // errors
            k += 1;
        }
        wr32(bar + REG_RDBAL, rx_ring_pa as u32);
        wr32(bar + REG_RDBAH, (rx_ring_pa >> 32) as u32);
        wr32(bar + REG_RDLEN, (Q_SIZE * DESC_SZ) as u32);
        wr32(bar + REG_RDH, 0);
        wr32(bar + REG_RDT, (Q_SIZE - 1) as u32);
        wr32(bar + REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);

        // 6. TX 环：描述符先清零，TDT=TDH=0，最后开 TCTL.EN。
        let tx_ring = nic.dma_vaddr + TX_RING_PAGE * PAGE;
        let tx_ring_pa = nic.dma_paddr + TX_RING_PAGE * PAGE;
        let mut j = 0u64;
        while j < Q_SIZE {
            let d = tx_ring + j * DESC_SZ;
            let mut w = 0u64;
            while w < DESC_SZ {
                wr8(d + w, 0);
                w += 1;
            }
            j += 1;
        }
        wr32(bar + REG_TDBAL, tx_ring_pa as u32);
        wr32(bar + REG_TDBAH, (tx_ring_pa >> 32) as u32);
        wr32(bar + REG_TDLEN, (Q_SIZE * DESC_SZ) as u32);
        wr32(bar + REG_TDH, 0);
        wr32(bar + REG_TDT, 0);
        wr32(bar + REG_TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD);

        Some(nic)
    }

    fn mac(&self) -> u64 {
        self.mac
    }

    /// 发一帧（帧已在 DMA 缓冲 `buf_off` 处，长 `len`）；等描述符 DD 回写。
    fn send(&mut self, buf_off: u64, len: u64) -> bool {
        let idx = self.tx_tail;
        let d = self.dma_vaddr + TX_RING_PAGE * PAGE + idx * DESC_SZ;
        let pa = self.dma_paddr + buf_off;
        wr32(d, pa as u32);
        wr32(d + 4, (pa >> 32) as u32);
        wr16(d + 8, len as u16);
        wr8(d + 10, 0); // cso
        wr8(d + 11, TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS);
        wr8(d + 12, 0); // status（硬件回写 DD）
        wr8(d + 13, 0);
        fence();
        self.tx_tail = (idx + 1) % Q_SIZE;
        wr32(self.bar + REG_TDT, self.tx_tail as u32);
        // 等硬件消费完（DD），保证缓冲可复用。
        let mut spins = 0u32;
        loop {
            if (rd8(d + 12) & DESC_DD) != 0 {
                return true;
            }
            spins += 1;
            if spins > 10_000_000 {
                return false;
            }
        }
    }

    /// 排空 RX：有一帧则拷进 `dst_va`，返回帧长；无帧返回 `None`。
    fn poll_rx(&mut self, dst_va: u64) -> Option<u64> {
        let idx = self.rx_next;
        let d = self.dma_vaddr + RX_RING_PAGE * PAGE + idx * DESC_SZ;
        let status = rd8(d + 12);
        if (status & DESC_DD) == 0 {
            return None;
        }
        let len = rd16_len(d);
        let errors = rd8(d + 13);
        // 归还描述符（硬件可复用），推进软游标。
        wr8(d + 12, 0);
        self.rx_next = (idx + 1) % Q_SIZE;
        wr32(self.bar + REG_RDT, idx as u32);
        if (status & DESC_EOP) == 0 || errors != 0 || len == 0 {
            return None; // 坏帧 / 非尾包：丢弃（已回收）。
        }
        let n = if len > BUF_SZ { BUF_SZ } else { len };
        let src = self.dma_vaddr + RX_BUF_PAGE * PAGE + idx * BUF_SZ;
        unsafe {
            core::ptr::copy_nonoverlapping(src as *const u8, dst_va as *mut u8, n as usize);
        }
        Some(n)
    }
}

/// 读 RX 描述符 `length` 字段（+8，16 位）。
fn rd16_len(d: u64) -> u64 {
    ((rd8(d + 8) as u64) | ((rd8(d + 9) as u64) << 8)) & 0xFFFF
}

// ---------------------------------------------------------------------------
// ARP（自测 + 供协议栈学习网关）
// ---------------------------------------------------------------------------

/// 在 `dst_va` 拼一个广播 ARP 请求（问 `GW_IP` 的 MAC），返回帧长。
fn build_arp_request(dst_va: u64, our_mac: u64) -> u64 {
    let mut mac = [0u8; 6];
    let mut i = 0u64;
    while i < 6 {
        mac[i as usize] = ((our_mac >> (8 * i)) & 0xff) as u8;
        i += 1;
    }
    // 以太头：目的 = 广播，源 = 本机，类型 = 0x0806。
    let mut j = 0u64;
    while j < 6 {
        wr8(dst_va + j, 0xff);
        wr8(dst_va + 6 + j, mac[j as usize]);
        j += 1;
    }
    wr8(dst_va + 12, (ETH_ARP >> 8) as u8);
    wr8(dst_va + 13, ETH_ARP as u8);
    // ARP：Ethernet/IPv4，oper=1，sha/spa = 本机，tpa = 网关。
    let a = dst_va + 14;
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
    14 + 28
}

// ---------------------------------------------------------------------------
// 帧级 IPC 服务（供协议栈经 NIC 表把它当一条出口）
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

/// 把共享页里的裸以太帧拷进 TX 缓冲发出。
fn ipc_tx(nic: &mut Nic, req: &NetReq) -> u64 {
    if req.buf == 0 || req.len == 0 || req.len > NET_FRAME_MAX || sys_virt_to_phys(req.buf) == 0 {
        return 0;
    }
    let tx_va = nic.dma_vaddr + TX_BUF_PAGE * PAGE;
    unsafe {
        core::ptr::copy_nonoverlapping(req.buf as *const u8, tx_va as *mut u8, req.len as usize);
    }
    if nic.send(TX_BUF_PAGE * PAGE, req.len) {
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
    nic.poll_rx(req.buf).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 通用驱动入口：`name` = 日志前缀（型号名），`marker` = 自测 marker 前缀（如 `NET9 e1000e`）。
pub fn run(name: &str, marker: &str) {
    let g = DeviceGrant::load();
    if !g.is_valid() {
        print(name);
        print(": no device grant (no ");
        print(name);
        println("), idle");
        idle();
    }

    let mut nic = match Nic::attach(&g) {
        Some(n) => n,
        None => {
            print(name);
            println(": bring-up failed (MAC/ring), idle");
            idle();
        }
    };

    print(name);
    print(": up, BAR0=0x");
    print_hex(g.bar_paddr);
    print(", MAC=");
    print_mac(nic.mac());
    println("");

    // 自测：广播 ARP 请求问网关 → 收 ARP 应答（端到端取证 TX + RX）。
    // 复位后**首帧可能被硬件丢弃**，故像协议栈的 `arp_learn_gw` 一样**重试**几次。
    let mut answered = false;
    let mut tx_ok = true;
    let mut attempt = 0u64;
    let mut burst = [0u8; NET_FRAME_MAX as usize];
    while attempt < 10 && !answered {
        let arp_len = build_arp_request(nic.dma_vaddr + TX_BUF_PAGE * PAGE, nic.mac());
        if !nic.send(TX_BUF_PAGE * PAGE, arp_len) {
            tx_ok = false;
            break;
        }
        let mut waited = 0u64;
        while waited < 100 {
            if let Some(n) = nic.poll_rx(burst.as_mut_ptr() as u64) {
                // 以太类型 0x0806 + oper=2（应答）+ 发送方 IP = 网关。
                if n >= 42 && burst[12] == 0x08 && burst[13] == 0x06 {
                    let oper = ((burst[20] as u16) << 8) | burst[21] as u16;
                    let sip = [burst[28], burst[29], burst[30], burst[31]];
                    if oper == 2 && sip == GW_IP {
                        answered = true;
                        break;
                    }
                }
            }
            sys_sleep(10);
            waited += 10;
        }
        attempt += 1;
    }
    if !tx_ok {
        print(marker);
        println(" FAILED (TX bring-up)");
    } else if answered {
        print(marker);
        print(" OK, MAC=");
        print_mac(nic.mac());
        println(", ARP reply OK");
    } else {
        print(marker);
        print(" OK, MAC=");
        print_mac(nic.mac());
        println(", ARP reply timeout");
    }

    // 服务帧级 IPC（协议栈据此把本网卡当一条出口）。RX 帧留给 `NET_OP_RX` 请求取走。
    loop {
        serve_net_ipc(&mut nic);
        sys_sleep(10);
    }
}
