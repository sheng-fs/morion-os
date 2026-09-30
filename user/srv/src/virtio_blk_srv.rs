//! 域 17 — 第二个真实驱动：**virtio-blk**（驱动路线 **D3**；**D2b** 起传输层与 vring 用 `libdevice::virtio`）。
//!
//! 目的：拿 D1 的**通用设备授权**（`DeviceGrant`）去驱动一台**新类型**的设备，全程**不改
//! 内核设备逻辑** —— 内核只按类找到 virtio-blk、交出 BAR + 连续 DMA 块 + MSI-X 参数，设备
//! 协议（virtio-blk 的请求链）完全在本域。
//!
//! 与 [`crate::net_srv`]（virtio-net）共享 [`libdevice::virtio`] 的传输层（能力解析 / common
//! cfg / 复位协商 / 队列配置 / avail·used 环）；区别只在**设备语义**：virtio-blk 只有**一个**
//! virtqueue，每个请求是一条**三段式描述符链**（header 16B → data 512B → status 1B）。
//!
//! 自测：读扇区 0 校验签名（宿主造盘时写入 `MORION-VBLK-TST!`）→ 写扇区 1 再读回校验 →
//! 串口打 marker。这一步同时验证 **DMA 描述符链 + avail/used 环 + MSI-X 中断** 整条链路。

use libdevice::grant::DeviceGrant;
use libdevice::mmio::{rd64, rd8, wr32, wr64, wr8};
use libdevice::msix;
use libdevice::virtio::{self, Vq};
use morion::syscall::*;

/// virtio-blk 请求类型（spec：`VIRTIO_BLK_T_*`）。
const BLK_T_IN: u32 = 0; // 读（设备 → 内存）
const BLK_T_OUT: u32 = 1; // 写（内存 → 设备）

/// 请求完成状态字节：0 = OK。
const BLK_S_OK: u8 = 0;

// ===========================================================================
// 环与缓冲布局（本驱动自己决定，内核不参与）
// ===========================================================================

const PAGE: u64 = virtio::PAGE;
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

/// 宿主造盘时写进扇区 0 的签名（自测据此确认"读到的确实是那块盘"）。
const SIG: [u8; 16] = *b"MORION-VBLK-TST!";

/// 驱动运行时状态：一个请求队列 + 复用的请求缓冲 + 中断掩码。
struct Blk {
    q: Vq,
    c: virtio::Caps,
    hdr_pa: u64,
    hdr_va: u64,
    data_pa: u64,
    data_va: u64,
    status_pa: u64,
    status_va: u64,
    /// 本设备向量对应的中断掩码（0 = 走轮询）。
    irq_mask: u64,
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
        self.q.set_desc(0, self.hdr_pa, 16, virtio::DESC_F_NEXT, 1);
        let data_flags = if req_type == BLK_T_IN {
            virtio::DESC_F_NEXT | virtio::DESC_F_WRITE
        } else {
            virtio::DESC_F_NEXT
        };
        self.q
            .set_desc(1, self.data_pa, SECTOR as u32, data_flags, 2);
        self.q
            .set_desc(2, self.status_pa, 1, virtio::DESC_F_WRITE, 0);

        // 挂上 avail 环（desc 头 = 0）→ 门铃。
        let before = self.q.used_idx();
        self.q.avail_push(0);
        self.q.kick(&self.c, 0);

        // 等完成：有中断走中断（快路径 poll + 阻塞 wait），否则让出 CPU 轮询。
        let mut tries = 0u32;
        while self.q.used_idx() == before && tries < 100 {
            if self.irq_mask != 0 {
                if sys_irq_poll(self.irq_mask) == 0 {
                    sys_irq_wait(self.irq_mask, IRQ_WAIT_MS);
                }
                // 读 ISR 以应答并清除设备侧中断（virtio 规范要求）。
                let _ = self.c.read_isr();
            } else {
                sys_sleep(1);
            }
            tries += 1;
        }
        // 回收 used 环：used 元素下标 = (used.idx - 1) % size。
        let used_idx = self.q.used_idx();
        let slot = (used_idx.wrapping_sub(1) as u64 % self.q.size as u64) as u16;
        let _ = self.q.used_elem(slot);
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

    let caps =
        match virtio::discover_caps(g.bar_vaddr, |off| sys_device_config_read(off as u64) as u32) {
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
    virtio::zero_page(g.dma_vaddr + REQ_RING_PAGE * PAGE);

    // 复位 + 特性协商：virtio-blk 的设备特性（RO / BLK_SIZE / FLUSH…）全不要，word0 取 0。
    match virtio::negotiate(&caps, |_| 0) {
        Ok(_) => {}
        Err(virtio::NegError::NoVersion1) => {
            println("vblk: device lacks VIRTIO_F_VERSION_1 (legacy), aborting");
            caps.failed();
            idle();
        }
        Err(virtio::NegError::FeaturesRejected) => {
            println("vblk: device rejected FEATURES_OK, aborting");
            caps.failed();
            idle();
        }
    }

    // 读设备配置：队列数 + 容量（单位 = 512B 扇区）。
    let num_queues = caps.num_queues();
    let capacity = rd64(caps.device);

    // MSI-X 判定：内核给了向量段 + 表窗口才走中断（单队列 → 1 条向量）。
    let want_irq = g.msix_vector_base != 0 && g.msix_table_vaddr != 0 && g.msix_vector_count >= 1;
    let irq_base = g.msix_vector_base as u16;
    let q_msix = if want_irq { 0 } else { virtio::NO_VECTOR };
    caps.set_config_msix(virtio::NO_VECTOR);

    // 建请求队列（队列 0）。
    let (qsize, noff) = virtio::setup_queue(
        &caps,
        0,
        Q_SIZE,
        q_msix,
        g.dma_paddr + REQ_RING_PAGE * PAGE,
        g.dma_paddr + REQ_RING_PAGE * PAGE + virtio::OFF_AVAIL,
        g.dma_paddr + REQ_RING_PAGE * PAGE + virtio::OFF_USED,
    );
    if qsize == 0 {
        println("vblk: virtqueue setup failed, aborting");
        caps.failed();
        idle();
    }
    let q = Vq::new(g.dma_vaddr, REQ_RING_PAGE, qsize, noff);

    print("vblk: virtio-blk up num_queues=");
    print_u64(num_queues as u64);
    print(" qsize=");
    print_u64(qsize as u64);
    print(" cap=");
    print_u64(capacity);
    println(" sectors");

    // MSI-X：写表项 → 注册向量 → 请内核打开 MSI-X。任一环节不成就退回轮询。
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

    // DRIVER_OK：驱动就绪，设备开始处理请求。
    caps.driver_ok();

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

    // 自测 A：读扇区 0，校验签名。
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

    // 自测 B：往扇区 1 写一个可复算的花纹，再读回校验。
    let mut k = 0u64;
    while k < SECTOR {
        wr8(
            blk.data_va + k,
            (k as u8).wrapping_mul(7).wrapping_add(0x33),
        );
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

    // marker：整条链路（描述符链 + 环 + 中断/轮询）的端到端取证。
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
