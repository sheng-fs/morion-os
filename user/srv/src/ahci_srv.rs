//! 域 18 — 真机存储驱动 `ahci_srv`（驱动路线 **D4**：SATA/AHCI）。
//!
//! 目的：像 [`crate::virtio_blk_srv`] 一样，拿 D1 的**通用设备授权**（声明式 PCI 查找 +
//! BAR 映射 + 物理连续 DMA）驱动一台**新类型**设备，全程不改内核设备逻辑。
//!
//! 与 virtio-blk 的区别：AHCI 是"寄存器 + 系统内存结构"式控制器 —— 命令列表 / 命令表 /
//! Received FIS 都由驱动自己在 DMA 块里排版，且必须满足 **命令表 1 KiB、命令列表 1 KiB、
//! FIS 256 B** 对齐（页对齐天然满足，见下方页布局）。
//!
//! **中断策略**：本仓库目前只有 MSI-X 一条中断通路（无 INTx、无 MSI-X 以外的 MSI），而 AHCI
//! 常态用 INTx/MSI —— 故**全轮询**（`PxCI` / `PxIS`），不申请中断向量（内核声明里
//! `msix_vectors = 0`）。日后要走中断，需先补 MSI/INTx 通路（那要动内核中断层，不属本轮）。
//!
//! **协议**（03b：接进块服务卷层）：启动自测（`IDENTIFY` + `READ DMA EXT` 校验宿主预写签名）
//! 通过后，`BLOCK_OP_ATTACH` **异步**通知 block_srv 把这个盘登记成一个「AHCI 后端卷」；此后
//! 本驱动进入服务循环，收发 block_srv 转来的读/写请求（`BlockReq`，`count ≤ 8` 扇区 = 一页）。
//! 每笔 I/O 都自己 DMA 到私有数据页，再与 block_srv 共享进来的暂存页互拷 ——
//! 上层文件系统完全不必知道这块盘挂在 AHCI 而不是 NVMe 上。

use crate::common::*;
use libdevice::grant::DeviceGrant;
use libdevice::mmio::{fence, rd16, rd32, rd8, wr32, wr8};
use morion::syscall::*;

const PAGE: u64 = 4096;

// ---------------------------------------------------------------------------
// ABAR 通用主机控制寄存器
// ---------------------------------------------------------------------------
const HBA_CAP: u64 = 0x00;
const HBA_GHC: u64 = 0x04;
const HBA_PI: u64 = 0x0C;
/// GHC.AE (bit31): 打开 AHCI 模式。
const GHC_AE: u32 = 1 << 31;

// ---------------------------------------------------------------------------
// 端口寄存器（基址 0x100 + port*0x80）
// ---------------------------------------------------------------------------
const PORT_BASE: u64 = 0x100;
const PORT_STRIDE: u64 = 0x80;
const P_CLB: u64 = 0x00; // 命令列表基址低
const P_CLBU: u64 = 0x04; // 命令列表基址高
const P_FB: u64 = 0x08; // Received FIS 基址低
const P_FBU: u64 = 0x0C; // Received FIS 基址高
const P_IS: u64 = 0x10; // 中断状态
const P_IE: u64 = 0x14; // 中断使能
const P_CMD: u64 = 0x18; // 命令与状态
const P_TFD: u64 = 0x20; // 任务文件数据
const P_SIG: u64 = 0x24; // 设备签名（类型）
const P_SSTS: u64 = 0x28; // SATA 状态
const P_CI: u64 = 0x38; // 命令发出（bit i ↔ 命令槽 i）

const CMD_ST: u32 = 1 << 0; // 启动
const CMD_FRE: u32 = 1 << 4; // FIS 接收使能
const CMD_CR: u32 = 1 << 15; // 命令列表运行中（只读）
const CMD_FR: u32 = 1 << 14; // FIS 接收运行中（只读）

const TFD_ERR: u32 = 1 << 0;
const TFD_DF: u32 = 1 << 5;

/// 端口签名：ATA 盘 = `0101h`（ATAPI = `EB14_0101h`、端口倍增器 = `9669_0101h`，都跳过）。
const SIG_ATA: u32 = 0x0000_0101;
/// `PxSSTS.DET` (bits 3:0) == 3：设备在位且链路已建立。
const SSTS_DET_PRESENT: u32 = 3;

// ---------------------------------------------------------------------------
// ATA 命令与 FIS
// ---------------------------------------------------------------------------
const FIS_TYPE_H2D: u8 = 0x27;
const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
/// `FLUSH CACHE EXT`：把写缓存刷到介质。写请求完成后紧跟一条，保证读回能看到刚写的数据。
const ATA_FLUSH_CACHE_EXT: u8 = 0xEA;

const SECTOR: u32 = 512;
/// 单笔 I/O 的扇区上限：私有数据页只有一页（见 `DATA_PAGE`），4096 / 512 = 8。
/// block_srv 会把更大的请求按此上限切分后再发过来。
const AHCI_MAX_SECTORS: u64 = 8;
/// 轮询上限（每次让出 1 tick ≈ 10 ms，故上界 ≈ 20 s）。
const POLL_MAX: u32 = 2000;

// ---------------------------------------------------------------------------
// DMA 页布局（页对齐 ⇒ 1 KiB / 256 B 对齐自动满足）
// ---------------------------------------------------------------------------
const DMA_PAGES: u64 = 6;
const CL_PAGE: u64 = 0; // 命令列表：32×32 B = 1 KiB
const FIS_PAGE: u64 = 1; // Received FIS：256 B
const CT_PAGE: u64 = 2; // 命令表：CFIS 64 B + PRDT（项 0 在 0x80）
const DATA_PAGE: u64 = 3; // 数据缓冲：一个扇区

/// 宿主造盘时写进扇区 0 的签名（自测据此确认"读到的确实是那块盘"）。
const SIG: [u8; 16] = *b"MORION-AHCI-TST!";

/// AHCI 端口运行时状态。
struct Ahci {
    abar: u64,
    cl_pa: u64,
    cl_va: u64,
    fis_pa: u64,
    ct_pa: u64,
    ct_va: u64,
    data_pa: u64,
    data_va: u64,
    port: u64,
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

/// 在 HBA 的已实现端口里挑第一个"在位且是 ATA 盘"的端口。
///
/// 先看 `PxSSTS.DET == 3`（设备在位 + 链路建立），再看 `PxSIG` 是否为 ATA 签名 ——
/// 端口上挂的也可能是 ATAPI（光驱）或端口倍增器，非磁盘一律跳过。
fn pick_port(abar: u64) -> Option<u64> {
    let pi = rd32(abar + HBA_PI);
    let mut p = 0u32;
    while p < 32 {
        if pi & (1u32 << p) != 0 {
            let base = abar + PORT_BASE + (p as u64) * PORT_STRIDE;
            if (rd32(base + P_SSTS) & 0xF) == SSTS_DET_PRESENT && rd32(base + P_SIG) == SIG_ATA {
                return Some(p as u64);
            }
        }
        p += 1;
    }
    None
}

impl Ahci {
    /// 某端口寄存器的绝对地址。
    fn reg(&self, off: u64) -> u64 {
        self.abar + PORT_BASE + self.port * PORT_STRIDE + off
    }

    /// 清 `PxCMD.bit` 并等待其"运行中"位（`CR`/`FR`）清零。
    fn stop_bit(&self, bit: u32, busy: u32) {
        let cmd = self.reg(P_CMD);
        wr32(cmd, rd32(cmd) & !bit);
        let mut t = 0u32;
        while rd32(cmd) & busy != 0 && t < POLL_MAX {
            sys_sleep(1);
            t += 1;
        }
    }

    /// 初始化选中端口：停端口 → 写命令列表/FIS 基址 → 关端口中断（轮询）→ 启动。
    fn start_port(&self) {
        self.stop_bit(CMD_ST, CMD_CR);
        self.stop_bit(CMD_FRE, CMD_FR);
        wr32(self.reg(P_CLB), (self.cl_pa & 0xFFFF_FFFF) as u32);
        wr32(self.reg(P_CLBU), (self.cl_pa >> 32) as u32);
        wr32(self.reg(P_FB), (self.fis_pa & 0xFFFF_FFFF) as u32);
        wr32(self.reg(P_FBU), (self.fis_pa >> 32) as u32);
        wr32(self.reg(P_IE), 0); // 全轮询：屏蔽端口中断
        wr32(self.reg(P_IS), 0xFFFF_FFFF); // 清挂起状态
        let cmd = self.reg(P_CMD);
        wr32(cmd, rd32(cmd) | CMD_FRE);
        let cmd = self.reg(P_CMD);
        wr32(cmd, rd32(cmd) | CMD_ST);
    }

    /// 用命令槽 0 发一条命令并**轮询**到完成；成功返回 `true`。
    ///
    /// `cmd`：ATA 命令码；`lba`：LBA48 起始扇区；`sectors`：扇区数；`bytes`：DMA 字节数
    /// （`0` = 无数据传输，如 `FLUSH CACHE EXT`，此时不发 PRDT）；`write`：方向位（命令头 W）。
    fn issue(&self, cmd: u8, lba: u64, sectors: u16, bytes: u32, write: bool) -> bool {
        // 清命令表 CFIS 区（64 B）与命令头槽 0（32 B）。
        let mut i = 0u64;
        while i < 16 {
            wr32(self.ct_va + i * 4, 0);
            i += 1;
        }
        let mut j = 0u64;
        while j < 8 {
            wr32(self.cl_va + j * 4, 0);
            j += 1;
        }

        // H2D Register FIS（20 B）。
        wr8(self.ct_va, FIS_TYPE_H2D);
        wr8(self.ct_va + 1, 0x80); // C=1：这是一条命令
        wr8(self.ct_va + 2, cmd);
        wr8(self.ct_va + 3, 0); // feature low
        wr8(self.ct_va + 4, lba as u8);
        wr8(self.ct_va + 5, (lba >> 8) as u8);
        wr8(self.ct_va + 6, (lba >> 16) as u8);
        // device：IDENTIFY 用 0，其余按 LBA 模式置 0x40。
        wr8(self.ct_va + 7, if cmd == ATA_IDENTIFY { 0 } else { 0x40 });
        wr8(self.ct_va + 8, (lba >> 24) as u8);
        wr8(self.ct_va + 9, (lba >> 32) as u8);
        wr8(self.ct_va + 10, (lba >> 40) as u8);
        wr8(self.ct_va + 11, 0); // feature exp
        wr8(self.ct_va + 12, sectors as u8);
        wr8(self.ct_va + 13, (sectors >> 8) as u8);
        wr8(self.ct_va + 14, 0);
        wr8(self.ct_va + 15, 0);

        // PRDT：有数据传输才建一项 (DBA 8 B + reserved 4 B + DBC 4 B，值 = 字节数-1)。
        // 无数据命令 (bytes == 0, 如 FLUSH) 时 PRDTL = 0，HBA 不读这一区 (先清零避免残留)。
        wr32(self.ct_va + 0x80, 0);
        wr32(self.ct_va + 0x84, 0);
        wr32(self.ct_va + 0x88, 0);
        wr32(self.ct_va + 0x8C, 0);
        let prdtl = if bytes == 0 {
            0u32
        } else {
            wr32(self.ct_va + 0x80, (self.data_pa & 0xFFFF_FFFF) as u32);
            wr32(self.ct_va + 0x84, (self.data_pa >> 32) as u32);
            wr32(self.ct_va + 0x8C, bytes - 1);
            1u32
        };

        // 命令头槽 0：CFL = 5（FIS 20 B / 4）| PRDTL；W = write（bit6）。
        wr32(
            self.cl_va,
            5u32 | (prdtl << 16) | if write { 1u32 << 6 } else { 0 },
        );
        wr32(self.cl_va + 0x04, 0); // PRDBC 由 HBA 回写
        wr32(self.cl_va + 0x08, (self.ct_pa & 0xFFFF_FFFF) as u32);
        wr32(self.cl_va + 0x0C, (self.ct_pa >> 32) as u32);

        // 清状态 → 置 PxCI.bit0 → 轮询到命令完成（bit0 清零）。
        wr32(self.reg(P_IS), 0xFFFF_FFFF);
        fence();
        wr32(self.reg(P_CI), 1);

        let mut t = 0u32;
        while t < POLL_MAX {
            if rd32(self.reg(P_CI)) & 1 == 0 {
                let is = rd32(self.reg(P_IS));
                let tfd = rd32(self.reg(P_TFD));
                wr32(self.reg(P_IS), 0xFFFF_FFFF);
                // TFES (bit30) = 任务文件错误；TFD.ERR / TFD.DF = 设备/传输错误。
                return is & (1 << 30) == 0 && tfd & (TFD_ERR | TFD_DF) == 0;
            }
            sys_sleep(1);
            t += 1;
        }
        false
    }
}

/// 把一段缓冲拷到另一段（`copy_nonoverlapping`；两段分属不同页，不会重叠）。
fn copy_buf(dst: u64, src: u64, bytes: usize) {
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, bytes);
    }
}

/// 只在首次读写错误时打一行（避免刷屏：回归里 I/O 很密）。
static mut RW_ERR_LOGGED: bool = false;
fn rw_err(msg: &str, lba: u64, count: u64) {
    unsafe {
        if RW_ERR_LOGGED {
            return;
        }
        RW_ERR_LOGGED = true;
    }
    print("ahci: ");
    print(msg);
    print(" lba=");
    print_u64(lba);
    print(" n=");
    print_u64(count);
    println("");
}

/// 服务一笔来自 block_srv 的读/写请求。
///
/// `lba`：盘内绝对 LBA（卷偏移已由 block_srv 合并）；`count`：扇区数（≤ `AHCI_MAX_SECTORS`）；
/// `buf`：block_srv 共享进来的暂存页（同址映射到本域）。返回是否成功。
fn serve_rw(a: &Ahci, write: bool, lba: u64, count: u64, buf: u64) -> bool {
    if count == 0 || count > AHCI_MAX_SECTORS {
        return false;
    }
    let bytes = (count as u32) * SECTOR;
    if write {
        // 暂存页 → 私有数据页 → 盘；随后 FLUSH 保证读回可见。
        copy_buf(a.data_va, buf, bytes as usize);
        if !a.issue(ATA_WRITE_DMA_EXT, lba, count as u16, bytes, true) {
            rw_err("WRITE DMA EXT failed", lba, count);
            return false;
        }
        if !a.issue(ATA_FLUSH_CACHE_EXT, 0, 0, 0, true) {
            rw_err("FLUSH CACHE EXT failed", lba, count);
            return false;
        }
        true
    } else {
        if !a.issue(ATA_READ_DMA_EXT, lba, count as u16, bytes, false) {
            rw_err("READ DMA EXT failed", lba, count);
            return false;
        }
        copy_buf(buf, a.data_va, bytes as usize);
        true
    }
}

/// 异步通知 block_srv「把这块盘挂进卷层」（`BLOCK_OP_ATTACH`，`count` = 容量扇区）。
///
/// 用 `send` 而非 `call`：block_srv 收到后会**回调**本驱动做读写校验（经 AHCI 后端），
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

/// 域 18 — ahci_srv：AHCI 驱动 + 自测（D4）+ 块服务后端（03b）。
pub fn run() {
    let g = DeviceGrant::load();
    if !g.is_valid() {
        println("ahci: no device grant (no AHCI), idle");
        idle();
    }
    if g.dma_bytes < DMA_PAGES * PAGE {
        println("ahci: device grant DMA too small, aborting");
        idle();
    }
    let abar = g.bar_vaddr;

    // GHC.AE：打开 AHCI 模式（固件未必已开）。
    wr32(abar + HBA_GHC, rd32(abar + HBA_GHC) | GHC_AE);

    print("ahci: cap=0x");
    print_hex(rd32(abar + HBA_CAP) as u64);
    print(" ports=0x");
    print_hex(rd32(abar + HBA_PI) as u64);
    println("");

    let port = match pick_port(abar) {
        Some(p) => p,
        None => {
            println("ahci: no SATA disk port (DET/SIG mismatch), idle");
            idle();
        }
    };
    print("ahci: using port ");
    print_u64(port);
    println("");

    let a = Ahci {
        abar,
        cl_pa: g.dma_paddr + CL_PAGE * PAGE,
        cl_va: g.dma_vaddr + CL_PAGE * PAGE,
        fis_pa: g.dma_paddr + FIS_PAGE * PAGE,
        ct_pa: g.dma_paddr + CT_PAGE * PAGE,
        ct_va: g.dma_vaddr + CT_PAGE * PAGE,
        data_pa: g.dma_paddr + DATA_PAGE * PAGE,
        data_va: g.dma_vaddr + DATA_PAGE * PAGE,
        port,
    };
    a.start_port();

    // IDENTIFY DEVICE：判类型 + 取容量。
    if !a.issue(ATA_IDENTIFY, 0, 0, SECTOR, false) {
        println("ahci: IDENTIFY failed");
        idle();
    }
    let is_ata = rd16(a.data_va) & 0x8000 == 0; // word 0 bit15: 0 = ATA 设备
    let lba48 = rd16(a.data_va + 166) & (1 << 10) != 0; // word 83 bit10
    let cap_sectors = if lba48 {
        (rd16(a.data_va + 200) as u64) | ((rd16(a.data_va + 202) as u64) << 16)
    } else {
        (rd16(a.data_va + 120) as u64) | ((rd16(a.data_va + 122) as u64) << 16)
    };
    print("ahci: identify ata=");
    print_u64(is_ata as u64);
    print(" lba48=");
    print_u64(lba48 as u64);
    print(" sectors=");
    print_u64(cap_sectors);
    println("");

    // 自测：以 LBA48 READ DMA EXT 读扇区 0，校验宿主预写签名。
    if !a.issue(ATA_READ_DMA_EXT, 0, 1, SECTOR, false) {
        println("ahci: READ DMA EXT sector 0 failed");
        idle();
    }
    let mut sig = [0u8; 16];
    let mut sig_ok = true;
    let mut i = 0u64;
    while i < 16 {
        sig[i as usize] = rd8(a.data_va + i);
        if sig[i as usize] != SIG[i as usize] {
            sig_ok = false;
        }
        i += 1;
    }

    // marker：只读链路的端到端取证（IDENTIFY + LBA48 DMA READ）。
    print("AHCI1 ahci OK, cap=");
    print_u64(cap_sectors);
    print(", sector0 sig=");
    print_sig(&sig);
    print(", sig=");
    print(if sig_ok { "ok" } else { "BAD" });
    println("");

    // 03b：把盘挂进 block_srv 的卷层（异步通知，随后回调做读写校验）。
    if !attach_to_block(cap_sectors) {
        println("ahci: attach to block_srv FAILED (send refused), idle");
        idle();
    }

    // 服务循环：接收 block_srv 转来的 `BlockReq`（读/写），每笔轮询完成后再回复。
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
            BLOCK_OP_READ => serve_rw(&a, false, req.lba, req.count, req.buf),
            BLOCK_OP_WRITE => serve_rw(&a, true, req.lba, req.count, req.buf),
            _ => false,
        };
        sys_reply(if ok { 1 } else { 0 });
    }
}
