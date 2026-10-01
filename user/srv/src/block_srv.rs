use crate::common::*;
use libdevice::grant::DeviceGrant;
use libdevice::mmio::{rd32, rd64, wr32, wr64};
use morion::syscall::*;

// ===========================================================================
// 域 5 — NVMe 块设备驱动服务 (文件系统阶段 1)
// ===========================================================================

/// 页大小 (字节)。
const PAGE: u64 = 4096;

/// **本驱动自己**在 DMA 块里的队列排版 (每项一页), 内核不再规定 —— 设备专属布局留在驱动域。
const DMA_OFF_ASQ: u64 = 0;
const DMA_OFF_ACQ: u64 = 1;
const DMA_OFF_ISQ: u64 = 2;
const DMA_OFF_ICQ: u64 = 3;
const DMA_OFF_ISQ2: u64 = 4;
const DMA_OFF_ICQ2: u64 = 5;
const DMA_OFF_DATA: u64 = 6;
/// 本驱动需要的 DMA 页数 (与上面 7 个偏移一致)。
const DMA_PAGES: u64 = 7;
/// admin / I/O 队列深度 (条目数, 取 2 的幂; 64 条 SQE 正好填满一页)。
const ADMIN_QDEPTH: u16 = 64;
const IO_QDEPTH: u16 = 64;

/// 驱动**本地**的运行期配置: 由 `DeviceGrant` 推导 (队列地址 = DMA 块基址 + 页偏移)。
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct NvmeConfig {
    magic: u64,
    bar0_paddr: u64,
    mmio_vaddr: u64,
    asq_paddr: u64,
    acq_paddr: u64,
    isq_paddr: u64,
    icq_paddr: u64,
    /// 第二条 I/O 队列 (qid 2) 的 SQ/CQ 物理地址 —— 多队列 + 多向量 (阶段 40)。
    isq2_paddr: u64,
    icq2_paddr: u64,
    data_paddr: u64,
    asq_vaddr: u64,
    acq_vaddr: u64,
    isq_vaddr: u64,
    icq_vaddr: u64,
    isq2_vaddr: u64,
    icq2_vaddr: u64,
    data_vaddr: u64,
    admin_qdepth: u16,
    io_qdepth: u16,
    page_size: u32,
    /// MSI-X **向量段基址** (0 = 未启用 MSI-X, 本驱动走轮询)。
    ///
    /// 完成队列 `i` 用向量 `msix_vector + i`, 也对应掩码里的位 `i` —— 于是「哪条队列
    /// 完成了」既能从 `SYS_IRQ_WAIT` 返回的向量号看出来, 也能直接从队列下标对上。
    msix_vector: u32,
    /// MSI-X 表相对**其所在 BAR** 的字节偏移。
    msix_table_offset: u32,
    /// MSI-X 表所在 BAR 映射到本域的虚拟地址 (NVMe 的表在 BAR0, 故等于 `mmio_vaddr`)。
    msix_table_vaddr: u64,
    /// MSI-X 中断消息地址 (低 32 位; 物理目的模式, 高 32 位恒 0)。
    msix_addr: u32,
    /// 内核分配的 MSI-X 向量条数 (0 = 未启用)。
    msix_vector_count: u32,
}

/// 由内核的 [`DeviceGrant`] 推导本驱动本地布局。
///
/// 队列地址 = DMA 块基址 + 页偏移 (本驱动的约定), 而不是内核写死的字段 —— 这正是 D1 把
/// 设备专属知识搬进驱动域的地方。
fn config_from_grant(g: &DeviceGrant) -> NvmeConfig {
    let d = |off: u64| g.dma_paddr + off * PAGE;
    let v = |off: u64| g.dma_vaddr + off * PAGE;
    NvmeConfig {
        magic: g.magic,
        bar0_paddr: g.bar_paddr,
        mmio_vaddr: g.bar_vaddr,
        asq_paddr: d(DMA_OFF_ASQ),
        acq_paddr: d(DMA_OFF_ACQ),
        isq_paddr: d(DMA_OFF_ISQ),
        icq_paddr: d(DMA_OFF_ICQ),
        isq2_paddr: d(DMA_OFF_ISQ2),
        icq2_paddr: d(DMA_OFF_ICQ2),
        data_paddr: d(DMA_OFF_DATA),
        asq_vaddr: v(DMA_OFF_ASQ),
        acq_vaddr: v(DMA_OFF_ACQ),
        isq_vaddr: v(DMA_OFF_ISQ),
        icq_vaddr: v(DMA_OFF_ICQ),
        isq2_vaddr: v(DMA_OFF_ISQ2),
        icq2_vaddr: v(DMA_OFF_ICQ2),
        data_vaddr: v(DMA_OFF_DATA),
        admin_qdepth: ADMIN_QDEPTH,
        io_qdepth: IO_QDEPTH,
        page_size: g.page_size,
        msix_vector: g.msix_vector_base,
        msix_table_offset: g.msix_table_offset,
        msix_table_vaddr: g.msix_table_vaddr,
        msix_addr: g.msix_msg_addr,
        msix_vector_count: g.msix_vector_count,
    }
}

/// I/O 队列数 (qid 1..=IO_QUEUES; qid 0 是 admin 队列)。
const IO_QUEUES: usize = 2;

/// 本驱动支持的 MSI-X 向量数上限 (与 `main.rs` 里给 NVMe 声明的 `msix_vectors: 3` 一致)。
///
/// 完成队列 `i` ↔ 表项 `i` ↔ 向量 `msix_vector + i` ↔ 等待掩码位 `i`, 故它同时也是
/// 「本驱动支持的完成队列数上限」。集群小于实际分配数时按实际值用。
const NVME_MSIX_MAX: usize = 3;

/// 向量数必须盖住 admin + 全部 I/O 队列 —— 少了就没法给每条队列一条独立向量。
const _: () = assert!(IO_QUEUES < NVME_MSIX_MAX);

// NVMe 控制器寄存器偏移 (相对 BAR0, 见 NVMe 规范)。
const REG_CAP: u64 = 0x00;
const REG_VS: u64 = 0x08;
const REG_CC: u64 = 0x14;
const REG_CSTS: u64 = 0x1C;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;
const DOORBELL_BASE: u64 = 0x1000;

// Admin / I/O 命令操作码。
const OP_CREATE_IO_SQ: u8 = 0x01;
// NVM I/O 命令 Write (0x01)。与 Admin 的 Create I/O SQ 同码, 但用于 I/O 队列。
const OP_WRITE: u8 = 0x01;
const OP_READ: u8 = 0x02;
const OP_CREATE_IO_CQ: u8 = 0x05;
const OP_IDENTIFY: u8 = 0x06;

/// 提交队列条目 (SQE, 64 字节)。
#[repr(C)]
#[derive(Clone, Copy)]
struct Sqe {
    opcode: u8,
    flags: u8, // FUSE/PSDT (PRP 时恒 0)
    cid: u16,
    nsid: u32,
    _rsvd1: u32,
    _rsvd2: u32,
    mptr: u64,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}

impl Sqe {
    fn zero() -> Self {
        Sqe {
            opcode: 0,
            flags: 0,
            cid: 0,
            nsid: 0,
            _rsvd1: 0,
            _rsvd2: 0,
            mptr: 0,
            prp1: 0,
            prp2: 0,
            cdw10: 0,
            cdw11: 0,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        }
    }
}

/// 完成队列条目 (CQE, 16 字节)。
#[repr(C)]
#[derive(Clone, Copy)]
struct Cqe {
    dw0: u32,
    dw1: u32,
    sqhd: u16,
    sqid: u16,
    cid: u16, // bytes 12-13: command id (与 Linux/QEMU 布局一致)
    sf: u16,  // bytes 14-15: status field, bit0 = phase, bit1.. = status code
}

// 易失 MMIO 读/写 (`rd32` / `rd64` / `wr32` / `wr64`) 统一来自 libdevice (D2)。

/// 写 MSI-X 表项 `entry`: 消息地址 + 数据 (= 向量) + 清屏蔽位。
///
/// 完成队列 `i` 用表项 `i` 与向量 `msix_vector + i` —— 表项下标与 `Create I/O CQ` 里的
/// IV 字段必须对上, 于是「哪条队列完成」直接由投递的向量区分 (阶段 40 多向量)。
///
/// 表所在 BAR 已由内核非缓存地映射给本域 (`msix_table_vaddr`; NVMe 的表在 BAR0, 故与
/// `mmio_vaddr` 相同), 由**本驱动**写。内核负责的是写完之后打开 MSI-X (配置空间) 与 LAPIC/向量段。
fn write_msix_table_entry(cfg: &NvmeConfig, entry: usize, vector: u64) {
    let base = cfg.msix_table_vaddr + cfg.msix_table_offset as u64 + entry as u64 * 16;
    wr32(base, cfg.msix_addr); // 消息地址 (低 32 位)
    wr32(base + 4, 0); // 消息地址 (高 32 位); 物理目的模式恒 0
    wr32(base + 8, vector as u32); // 消息数据 = 中断向量
    wr32(base + 12, 0); // 向量控制: bit0=1 屏蔽 → 0 = 不屏蔽
    print("nvme: MSI-X table[");
    print_u64(entry as u64);
    print("] programmed addr=0x");
    print_hex(cfg.msix_addr as u64);
    print(" data=0x");
    print_hex(vector);
    println("");
}

// ---------------------------------------------------------------------------
// 完成路径: 中断驱动 (MSI-X) 与轮询回退
// ---------------------------------------------------------------------------

/// 中断模式下「等中断」的口径: 每次阻塞最多 `NVME_IRQ_WAIT_TIMEOUT_MS` 毫秒,
/// 最多等 `NVME_IRQ_WAIT_ROUNDS` 轮 (合计约 160 ms)。
///
/// 等不到中断的唯一解释是 MSI-X 没能真正投递中断 (配置被拒 / 设备未投), 那时永久
/// 回退轮询 —— 一次 I/O 不能把整个块服务卡死。这里的轮数/超时就是那个看门狗。
const NVME_IRQ_WAIT_TIMEOUT_MS: u64 = 10;
const NVME_IRQ_WAIT_ROUNDS: u32 = 16;

/// 中断模式是否启用 (粘性: 一旦回退就不再回到中断路径)。
static mut NVME_IRQ_MODE: bool = false;
/// 中断模式下向量段的**基址** (= 完成队列 0 的向量, 来自内核配置结构)。
static mut NVME_IRQ_VECTOR: u64 = 0;
/// 见过哪些向量的中断 (位 `i` ↔ 完成队列 `i`) —— 「多向量真的分发到了」的证据。
static mut NVME_IRQ_VEC_MASK: u64 = 0;

/// 运行期计数: 「本次运行真的走了哪条完成路径」的证据 (每 `NVME_STATS_EVERY` 条打印一次)。
static mut NVME_IRQ_OBSERVED: u64 = 0; // 取到中断的次数
static mut NVME_IRQ_CMDS: u64 = 0; // 中断路径完成的命令数
static mut NVME_POLL_CMDS: u64 = 0; // 轮询路径完成的命令数
static mut NVME_CMDS_TOTAL: u64 = 0;
/// 计数打印间隔 (证据行不能刷屏: 一整轮自测约 2 万条命令)。
const NVME_STATS_EVERY: u64 = 4096;

/// 中断模式是否处于启用状态。
fn nvme_irq_mode() -> bool {
    unsafe { NVME_IRQ_MODE }
}

/// 打印一次完成路径计数 (证据行)。
fn nvme_stats_print(prefix: &str) {
    unsafe {
        print(prefix);
        print("cmds=");
        print_u64(NVME_CMDS_TOTAL);
        print(" irq_cmds=");
        print_u64(NVME_IRQ_CMDS);
        print(" poll_cmds=");
        print_u64(NVME_POLL_CMDS);
        print(" irqs=");
        print_u64(NVME_IRQ_OBSERVED);
        print(" vecs=0x");
        print_hex(NVME_IRQ_VEC_MASK);
        print(" mode=");
        println(if NVME_IRQ_MODE { "irq" } else { "poll" });
    }
}

/// 计数并周期打印「两条完成路径各自走了多少条命令」。
fn nvme_stats_tick() {
    unsafe {
        NVME_CMDS_TOTAL += 1;
        if !NVME_CMDS_TOTAL.is_multiple_of(NVME_STATS_EVERY) {
            return;
        }
    }
    nvme_stats_print("nvme: stats ");
}

/// 尝试从完成队列取一条 CQE。
///
/// 取到则推进 head / 翻转 phase / 敲 CQ 门铃, 返回 `Some(状态码是否为 0)`;
/// 队列里还没有新 CQE 返回 `None`。中断路径与轮询路径共用它 —— 两条路径的完成判定
/// 与出错打印必须完全一致, 否则「换了路径」就变成「换了语义」。
fn try_complete(
    cq_vaddr: u64,
    cq_doorbell: u64,
    qdepth: u32,
    sqe: &Sqe,
    head: &mut u32,
    phase: &mut u32,
) -> Option<bool> {
    let idx = (*head % qdepth) as u64;
    let cqe: Cqe = unsafe { core::ptr::read_volatile((cq_vaddr + idx * 16) as *const Cqe) };
    if (cqe.sf & 1) as u32 != *phase {
        return None;
    }
    *head = (*head + 1) % qdepth;
    if *head == 0 {
        *phase ^= 1;
    }
    wr32(cq_doorbell, *head);
    let sc = cqe.sf >> 1;
    if sc != 0 {
        print("nvme: CQE fail sc=");
        print_u64(sc as u64);
        print(" cid=");
        print_u64(cqe.cid as u64);
        print(" sqid=");
        print_u64(cqe.sqid as u64);
        print(" op=");
        print_u64(sqe.opcode as u64);
        print(" cdw10=");
        print_u64(sqe.cdw10 as u64);
        print(" cdw11=");
        print_u64(sqe.cdw11 as u64);
        print(" nlb-1=");
        print_u64(sqe.cdw12 as u64);
        print(" nsid=");
        print_u64(sqe.nsid as u64);
        print(" prp1=");
        print_u64(sqe.prp1);
        println("");
    }
    Some(sc == 0)
}

/// 向指定队列提交一条命令并等其完成。返回状态码是否为 0 (成功)。
///
/// `sq_vaddr`/`cq_vaddr` 为队列内存虚拟地址, `sq_doorbell`/`cq_doorbell`
/// 为门铃寄存器虚拟地址 (含 stride), `qdepth` 为队列深度。
///
/// `wait_mask` 是等待中断用的**向量掩码** (位 `i` ↔ 完成队列 `i`): 提交到 I/O 队列时
/// 用「全部 I/O 队列」的掩码 —— 哪条队列先完成都算数, 返回值还告诉我们是哪一条。
///
/// 完成等待有两条路径: MSI-X 中断驱动 (`nvme_main` 里注册成功后启用) 与轮询 ——
/// 轮询既是中断不可用时的保底, 也是中断路径等不到中断时的回退。
#[allow(clippy::too_many_arguments)]
fn submit_wait(
    sq_vaddr: u64,
    cq_vaddr: u64,
    sq_doorbell: u64,
    cq_doorbell: u64,
    qdepth: u32,
    mmio: u64,
    wait_mask: u64,
    sqe: Sqe,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) -> bool {
    let idx = (*tail % qdepth) as u64;
    unsafe {
        core::ptr::write_volatile((sq_vaddr + idx * 64) as *mut Sqe, sqe);
    }
    *tail = (*tail + 1) % qdepth;
    wr32(sq_doorbell, *tail);

    // 路径一: 中断驱动。设备 post CQE 后会投递一条 MSI-X 中断; 内核处理器只置
    // 「待处理位」并唤醒本域 (不投 IPC —— 那会与块请求混在同一个邮箱里, 还会改写内核
    // 记录的回复目标)。顺序是**先等中断, 再查 CQE**: 设备保证「先写 CQE 再发中断」,
    // 所以中断到了就一定有完成可取。
    if nvme_irq_mode() {
        let base = unsafe { NVME_IRQ_VECTOR };
        let mut rounds = NVME_IRQ_WAIT_ROUNDS;
        loop {
            // 快路径: 非阻塞取位即可命中 (设备 post CQE 与投中断都在门铃那次 MMIO exit
            // 之后就完成了), 命中就不必真睡; 未命中才阻塞等下一次中断 (本域进入阻塞态,
            // CPU 交给别的域, 不空转)。返回的是**命中的向量号** —— 相减即完成队列下标。
            let hit = sys_irq_poll(wait_mask);
            let hit = if hit != 0 {
                hit
            } else {
                sys_irq_wait(wait_mask, NVME_IRQ_WAIT_TIMEOUT_MS)
            };
            if hit != 0 {
                unsafe {
                    NVME_IRQ_OBSERVED += 1;
                    NVME_IRQ_VEC_MASK |= 1 << (hit - base);
                }
                if let Some(ok) = try_complete(cq_vaddr, cq_doorbell, qdepth, &sqe, head, phase) {
                    unsafe { NVME_IRQ_CMDS += 1 };
                    nvme_stats_tick();
                    return ok;
                }
                // 陈旧中断 (其 CQE 已被取走): 继续等本命令自己的中断。
                continue;
            }
            rounds -= 1;
            if rounds == 0 {
                // 等不到任何中断: 粘性回退 (本次运行内不再走中断路径)。
                unsafe { NVME_IRQ_MODE = false };
                println("nvme: irq wait timed out, fallback to polling");
                break;
            }
        }
    }

    // 路径二: 轮询完成队列。QEMU 用 timer 异步投递 CQE, 需要其主循环运行才会 post;
    // 而 guest 在 KVM 里纯轮询不会触发 VM exit, 主循环被阻塞。故每次迭代读一次
    // CSTS (MMIO) 强制 VM exit, 让 QEMU 主循环有机会 post CQE。
    for _ in 0..NVME_POLL_LIMIT {
        if let Some(ok) = try_complete(cq_vaddr, cq_doorbell, qdepth, &sqe, head, phase) {
            unsafe { NVME_POLL_CMDS += 1 };
            nvme_stats_tick();
            return ok;
        }
        // 读 CSTS 触发 VM exit (无副作用, 只读状态寄存器)。
        let _ = rd32(mmio + REG_CSTS);
    }
    // 轮询超时: 打印队列状态与命令 id, 便于定位。
    print("nvme: CQE timeout head=");
    print_u64(*head as u64);
    print(" tail=");
    print_u64(*tail as u64);
    print(" phase=");
    print_u64(*phase as u64);
    print("sqe_cid=");
    print_u64(sqe.cid as u64);
    println("");
    // 调试: 转储原始 CQ (槽 0/1) 与 SQ (槽 0/1) 的 16 字节, 判断是「从未投递」还是「phase 不符」。
    let d0 = unsafe { core::ptr::read_volatile(cq_vaddr as *const u64) };
    let d1 = unsafe { core::ptr::read_volatile((cq_vaddr + 8) as *const u64) };
    let d2 = unsafe { core::ptr::read_volatile((cq_vaddr + 16) as *const u64) };
    let d3 = unsafe { core::ptr::read_volatile((cq_vaddr + 24) as *const u64) };
    print("    CQ0 lo=");
    print_u64(d0);
    print(" hi=");
    print_u64(d1);
    println("");
    print("    CQ1 lo=");
    print_u64(d2);
    print(" hi=");
    print_u64(d3);
    println("");
    let s0 = unsafe { core::ptr::read_volatile(sq_vaddr as *const u64) };
    let s1 = unsafe { core::ptr::read_volatile((sq_vaddr + 64) as *const u64) };
    print("    SQ0 dw0=");
    print_u64(s0);
    print(" SQ1 dw0=");
    print_u64(s1);
    println("");
    false
}

/// 轮询 CQE 的迭代上限。
///
/// 每次迭代读一次 CSTS 强制 VM exit, 让 QEMU 主循环有机会投递 CQE; 上限按
/// 「最多等约几秒」设定 —— 预算过紧时, 宿主机负载稍高 (如 MFS 大量 COW 写)
/// 就会误判超时, 使一次正常的块读写失败。
const NVME_POLL_LIMIT: u32 = 1_000_000;
/// 逻辑扇区大小 (与块层约定一致)。
const NVME_SECTOR_SIZE: usize = 512;
/// 页大小 (NVMe 的 PRP 粒度)。
const NVME_PAGE_SIZE: usize = 4096;
/// 单条 NVMe 命令的扇区上限 (256 扇区 = 128 KiB, 与 `BlockReq.count` 约定一致)。
/// 更大的请求由 block_srv 拆成多条命令。
const NVME_MAX_SECTORS: u16 = 256;
/// block_srv 私有的 PRP 表页虚拟地址 (> 2 页的传输共用一页表项空间)。
/// 与 `VOL_SCRATCH_VADDR` 同理: 必须落在所有共享缓冲段之上 (见该常量处的地址分区表)。
const PRP_LIST_VADDR: u64 = 0x0000_0080_0016_1000;

/// E1c 越界 DMA 探针的 PRP: **窗口外第一个地址**。
///
/// 内核 VT-d 侧 (`kernel/src/arch/iommu.rs` 的 `TARGET_WINDOW_LIMIT`) 只允许本设备 DMA 到
/// 窗口 `[0, 3 GiB)` —— 这个数字必须与它保持一致 (本驱动是用户域, 够不到内核常量)。
/// 注意它同时必须 **< 4 GiB**: 本机 QEMU 的 intel-iommu 不翻译 ≥ 4 GiB 的 IOVA。
const IOMMU_PROBE_IOVA: u64 = 3 * (1 << 30);
/// 探针完成等待的自旋上限: 每次迭代读一次 CSTS 逼 QEMU 主循环跑起来。
///
/// 探针的完成与否**不影响任何东西** (它故意指向窗口外), 故上限取得很小 —— 正常情况下
/// 设备几个迭代内就会 post CQE, 只有"越界 DMA 让设备彻底不回应"时才会跑满。
const NVME_IOMMU_PROBE_SPINS: u32 = 50_000;

/// 当前提交所用的 I/O 队列下标 (由 `io_select_queue` 在每次请求开始时轮转设定)。
///
/// 串行提交下同一时刻只有一条命令在飞, 故一份就够; 引入并发 (多条在飞) 时才需要
/// 变成每请求一份。它决定 `nvme_rw_sectors` 用哪条队列的 SQ/CQ 内存。
static mut NVME_CUR_Q: usize = 0;

/// 取第 `q` 条 I/O 队列 (qid = `q + 1`) 的 (SQ 门铃, CQ 门铃) 地址。
///
/// 门铃区自 `DOORBELL_BASE` 起按 qid 排列: qid `n` 的 SQ 在 `2n * stride`, CQ 在 `2n+1`。
fn io_q_doorbells(mmio: u64, stride: u64, q: usize) -> (u64, u64) {
    let qid = (q + 1) as u64;
    (
        mmio + DOORBELL_BASE + 2 * qid * stride,
        mmio + DOORBELL_BASE + (2 * qid + 1) * stride,
    )
}

/// 取第 `q` 条 I/O 队列的 (SQ, CQ) 内存虚地址 —— 各占一页, 由内核分配并映射好。
fn io_q_vaddrs(cfg: &NvmeConfig, q: usize) -> (u64, u64) {
    if q == 0 {
        (cfg.isq_vaddr, cfg.icq_vaddr)
    } else {
        (cfg.isq2_vaddr, cfg.icq2_vaddr)
    }
}

/// 全部 I/O 队列的中断向量掩码 (位 `i` ↔ 完成队列 `i`; admin 是位 0, I/O 是 1..=IO_QUEUES)。
fn io_wait_mask() -> u64 {
    ((1u64 << IO_QUEUES) - 1) << 1
}

/// Admin 完成队列的中断向量掩码 (位 0 ↔ 完成队列 0 = admin)。
const ADMIN_WAIT_MASK: u64 = 1;

/// 轮转选下一条 I/O 队列, 并把「当前队列」记进 `NVME_CUR_Q`。
///
/// 返回 (队列下标, SQ 门铃, CQ 门铃) —— 调用方据此取该队列的 `io_tail[q]` / `io_head[q]` /
/// `io_phase[q]`, 三者与这里的下标必须同源。
fn io_select_queue(mmio: u64, stride: u64, rr: &mut usize) -> (usize, u64, u64) {
    let q = *rr;
    *rr = (*rr + 1) % IO_QUEUES;
    unsafe { NVME_CUR_Q = q };
    let (sq, cq) = io_q_doorbells(mmio, stride, q);
    (q, sq, cq)
}

/// 经 NVMe I/O 队列读/写 `count` 个扇区到 `buf` (页对齐的用户页)。
///
/// `buf` 为调用方共享给本域的缓冲页虚拟地址, 已映射; 先经 `sys_virt_to_phys`
/// 反查物理地址作为 NVMe DMA 的 PRP1。
///
/// **多页传输**: 单次命令可覆盖多页 (最多 `NVME_MAX_SECTORS` 扇区 = 128 KiB), 按 NVMe
/// 规范组织 PRP —— 1 页只用 PRP1; 2 页时 PRP2 直接指向第 2 页; 超过 2 页时 PRP2 指向
/// 本域私有的 **PRP 表页**, 表项依次是第 2..N 页的物理地址 (末项可指向半页, 长度由
/// NLB 决定)。因为要让每页单独反查物理地址, 调用方的缓冲必须是**逐页映射**的连续
/// 虚拟区间 (页式共享天然满足); 多页时首地址必须页对齐。
#[allow(clippy::too_many_arguments)]
fn nvme_rw_sectors(
    opcode: u8,
    nsid: u32,
    cfg: &NvmeConfig,
    mmio: u64,
    isq_doorbell: u64,
    icq_doorbell: u64,
    lba: u32,
    count: u16,
    buf: *mut u8,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) -> bool {
    if count == 0 || count > NVME_MAX_SECTORS {
        return false;
    }
    let bytes = count as usize * NVME_SECTOR_SIZE;
    let pages = bytes.div_ceil(NVME_PAGE_SIZE);
    let paddr = sys_virt_to_phys(buf as u64);
    if paddr == 0 {
        return false;
    }
    let mut prp2: u64 = 0;
    if pages > 1 {
        // 多页传输: PRP1 必须是页地址, 其余页由 PRP2 (或 PRP 表) 描述。
        if !paddr.is_multiple_of(NVME_PAGE_SIZE as u64) {
            return false;
        }
        if pages == 2 {
            let p = sys_virt_to_phys(buf as u64 + NVME_PAGE_SIZE as u64);
            if p == 0 {
                return false;
            }
            prp2 = p;
        } else {
            let lp = sys_virt_to_phys(PRP_LIST_VADDR);
            if lp == 0 {
                return false;
            }
            let list = PRP_LIST_VADDR as *mut u64;
            let mut i = 1usize;
            while i < pages {
                let p = sys_virt_to_phys(buf as u64 + (i * NVME_PAGE_SIZE) as u64);
                if p == 0 || !p.is_multiple_of(NVME_PAGE_SIZE as u64) {
                    return false;
                }
                unsafe {
                    core::ptr::write_volatile(list.add(i - 1), p);
                }
                i += 1;
            }
            prp2 = lp;
        }
    }
    let mut sqe = Sqe::zero();
    sqe.opcode = opcode;
    sqe.cid = 5;
    sqe.nsid = nsid;
    sqe.prp1 = paddr;
    sqe.prp2 = prp2;
    sqe.cdw10 = lba;
    sqe.cdw11 = 0; // SLBA 高 32 位 = 0
    sqe.cdw12 = (count as u32) - 1; // NLB (0-based)
                                    // 用**当前队列**的 SQ/CQ 内存 (与调用方传进来的门铃同源, 见 `io_select_queue`)。
    let (sq_vaddr, cq_vaddr) = io_q_vaddrs(cfg, unsafe { NVME_CUR_Q });
    submit_wait(
        sq_vaddr,
        cq_vaddr,
        isq_doorbell,
        icq_doorbell,
        cfg.io_qdepth as u32,
        mmio,
        io_wait_mask(),
        sqe,
        tail,
        head,
        phase,
    )
}

/// NVMe 枚举到的 namespace 上限 (Identify CNS=2 返回的 NSID 列表)。
const NVME_MAX_NS: usize = 8;
/// 枚举到的 namespace id 列表 (升序) 与数量, 由 `nvme_main` 初始化时填充。
/// 卷层据此逐盘扫描分区表。
static mut NVME_NSIDS: [u32; NVME_MAX_NS] = [0; NVME_MAX_NS];
static mut NVME_NS_COUNT: usize = 0;

/// 每个 namespace 的容量 (扇区数), 由 Identify Namespace (CNS=0) 的 NSZE 取得;
/// 0 = 未知 (该盘不支持查询 / 查询失败)。与 `NVME_NSIDS` 下标一一对应。
///
/// 为什么需要它: 卷层对「无分区表的整盘」记 `sectors = 0`(容量未知), 而 MFS 首次
/// 格式化要按**卷的真实几何**决定文件系统大小 —— 少了 NSZE 就只能退回写死的默认值,
/// 在一整块新盘上会格式出一个小得离谱的文件系统。
static mut NVME_NS_SECTORS: [u32; NVME_MAX_NS] = [0; NVME_MAX_NS];

/// 取 namespace `nsid` 的容量 (扇区数)。
///
/// Identify Namespace (CNS=0) 的返回数据里偏移 0 是 NSZE (u64), 在 512 B 逻辑块下
/// 即扇区数。走 **Admin 队列**: 该命令只在初始化阶段按盘各发一次, 不该占用 I/O 队列。
/// 返回值超出 u32 的容量 (≥ 2 TiB) 截断 —— 卷层的 `sectors` 本就是 u32。
#[allow(clippy::too_many_arguments)] // 与 vol_scan_namespace 等一样, 需透传队列上下文
fn nvme_ns_sectors(
    nsid: u32,
    cfg: &NvmeConfig,
    mmio: u64,
    asq_doorbell: u64,
    acq_doorbell: u64,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) -> u32 {
    let mut sqe = Sqe::zero();
    sqe.opcode = OP_IDENTIFY;
    sqe.cid = 0x100 + nsid as u16; // 与初始化阶段已用的 cid 1..4 错开
    sqe.nsid = nsid;
    sqe.prp1 = cfg.data_paddr;
    sqe.cdw10 = 0x00; // CNS=0 = Identify Namespace
    if !submit_wait(
        cfg.asq_vaddr,
        cfg.acq_vaddr,
        asq_doorbell,
        acq_doorbell,
        cfg.admin_qdepth as u32,
        mmio,
        ADMIN_WAIT_MASK,
        sqe,
        tail,
        head,
        phase,
    ) {
        return 0;
    }
    let nsze = rd64(cfg.data_vaddr);
    if nsze > u32::MAX as u64 {
        u32::MAX
    } else {
        nsze as u32
    }
}

/// 查已缓存的 namespace 容量 (扇区数); 0 = 未知。
fn nvme_ns_sectors_of(nsid: u32) -> u32 {
    let n = unsafe { NVME_NS_COUNT };
    let mut i = 0usize;
    while i < n {
        if unsafe { NVME_NSIDS[i] } == nsid {
            return unsafe { NVME_NS_SECTORS[i] };
        }
        i += 1;
    }
    0
}

/// 域 5 — NVMe 驱动服务: 复位控制器 → Admin 队列 → Identify → I/O 队列 → 块服务。
fn nvme_main() {
    // 读通用设备授权描述 (内核已映射到本域)。magic 不对 = 内核没授权 (无控制器), 优雅退出;
    // DMA 块不够本驱动排 7 页也当作没授权 —— 布局由本驱动决定, 故这里也要自己验。
    let g = DeviceGrant::load();
    if !g.is_valid() {
        println("nvme: no device grant, aborting");
        return;
    }
    if g.dma_bytes < DMA_PAGES * PAGE {
        println("nvme: device grant DMA too small, aborting");
        return;
    }
    let cfg = config_from_grant(&g);
    let mmio = cfg.mmio_vaddr;

    // 0. 选择完成路径 (阶段 39/40 MSI/MSI-X 多向量)。三步缺一不可, 任一步失败都退回轮询:
    //    ① 逐条写 MSI-X 表项 `i` = 向量 `msix_vector + i` (表在 BAR0 里, 由本域写 ——
    //       内核到不了这个 BAR);
    //    ② `sys_msix_enable` 请内核打开 MSI-X (配置空间写留在内核);
    //    ③ 逐条注册向量, 之后用 `sys_irq_poll(mask)` 取位 / `sys_irq_wait(mask, ms)` 阻塞等。
    //    msix_vector = 0 表示内核没能准备 MSI-X (无 LAPIC / 无该能力 / 表不在 BAR0)。
    //    每条队列一个向量: 完成队列 `i` ↔ 表项 `i` ↔ 向量 `msix_vector + i` ↔ 掩码位 `i`。
    if cfg.msix_vector != 0 {
        let base = cfg.msix_vector as u64;
        let count = cfg.msix_vector_count.min(NVME_MSIX_MAX as u32).max(1) as u64;
        let mut e = 0usize;
        while (e as u64) < count {
            write_msix_table_entry(&cfg, e, base + e as u64);
            e += 1;
        }
        let mut registered = 0u64;
        let mut i = 0u64;
        while i < count {
            if sys_register_irq(base + i) == 1 {
                registered += 1;
            }
            i += 1;
        }
        if sys_msix_enable() != 1 {
            println("nvme: msix_enable refused by kernel, polling mode");
        } else if registered != count {
            println("nvme: register_irq refused some vectors, polling mode");
        } else {
            unsafe {
                NVME_IRQ_VECTOR = base;
                NVME_IRQ_MODE = true;
            }
            print("nvme: irq-driven completions, vectors=0x");
            print_hex(base);
            print("..0x");
            print_hex(base + count - 1);
            println("");
        }
    } else {
        println("nvme: polling mode (kernel gave no MSI-X vector)");
    }

    // 1. 读 CAP, 计算门铃 stride (DSTRD 在 CAP 的 bits 32:35, stride = 4 << DSTRD 字节)。
    let cap = rd64(mmio + REG_CAP);
    let dstrd = ((cap >> 32) & 0xF) as u64;
    let stride = 4u64 << dstrd;

    // 2. 禁用控制器 (CC.EN=0), 等 CSTS.RDY 清零。
    wr32(mmio + REG_CC, 0);
    let mut timeout = 0u64;
    while rd32(mmio + REG_CSTS) & 1 != 0 {
        timeout += 1;
        if timeout > 1_000_000 {
            println("nvme: timeout waiting CSTS.RDY=0");
            return;
        }
    }

    // 3. 配置 Admin 队列属性 + 基地址。
    let qsize = (cfg.admin_qdepth as u32 - 1) & 0xFFF;
    wr32(mmio + REG_AQA, qsize | (qsize << 16));
    wr64(mmio + REG_ASQ, cfg.asq_paddr);
    wr64(mmio + REG_ACQ, cfg.acq_paddr);

    // 4. 使能控制器 (IOSQES=6 => 64B @bits16-19, IOCQES=4 => 16B @bits20-23, MPS=0 => 4KB)。
    //    与 Linux include/linux/nvme.h 一致: NVME_CC_IOSQES = 6<<16, NVME_CC_IOCQES = 4<<20。
    wr32(mmio + REG_CC, 1 | (6 << 16) | (4 << 20));

    // 5. 等 CSTS.RDY 置位。
    timeout = 0;
    while rd32(mmio + REG_CSTS) & 1 == 0 {
        timeout += 1;
        if timeout > 1_000_000 {
            println("nvme: timeout waiting CSTS.RDY=1");
            return;
        }
    }

    // Admin 队列门铃 (SQ0 tail / CQ0 head)。
    let asq_doorbell = mmio + DOORBELL_BASE;
    let acq_doorbell = mmio + DOORBELL_BASE + stride;
    let mut admin_tail: u32 = 0;
    let mut admin_head: u32 = 0;
    // CQ phase tag 首条为 1 (NVMe 规范 / QEMU NVMe 行为), 故初始期望 phase=1,
    // 避免把被清零的 CQ 槽 (phase=0) 误判为已完成的 CQE。
    let mut admin_phase: u32 = 1;

    // 6. Identify Controller (CNS=1) → 打印型号 / 序列号。
    let mut sqe = Sqe::zero();
    sqe.opcode = OP_IDENTIFY;
    sqe.cid = 1;
    sqe.prp1 = cfg.data_paddr;
    sqe.cdw10 = 0x01;
    if !submit_wait(
        cfg.asq_vaddr,
        cfg.acq_vaddr,
        asq_doorbell,
        acq_doorbell,
        cfg.admin_qdepth as u32,
        mmio,
        ADMIN_WAIT_MASK,
        sqe,
        &mut admin_tail,
        &mut admin_head,
        &mut admin_phase,
    ) {
        println("nvme: Identify Controller FAILED");
        return;
    }

    // 6b. 从 Identify Controller 取 NN (Number of Namespaces, 数据偏移 516)。
    //     QEMU 把各 `nvme-ns` 连续编号为 1..NN, 故 namespace 列表即 1..=NN。
    //     (不用 Identify CNS=2: 实测部分 QEMU 版本返回的列表不完整。)
    //     注意必须在此处读: 紧接着的 CNS=0 会覆盖同一数据页。
    let nn = rd32(cfg.data_vaddr + 516) as usize;
    {
        let n = if nn > NVME_MAX_NS { NVME_MAX_NS } else { nn };
        let mut i = 0usize;
        while i < n {
            unsafe {
                NVME_NSIDS[i] = (i + 1) as u32;
            }
            i += 1;
        }
        unsafe {
            NVME_NS_COUNT = n;
        }
    }

    // 7. 若 NN 不可用 (为 0), 回退按当前 QEMU 配置假定 namespace 为 1..=3。
    //    必须在取 NSZE 之前: 下一步要按 namespace 列表逐个查询容量。
    if unsafe { NVME_NS_COUNT } == 0 {
        unsafe {
            NVME_NSIDS[0] = 1;
            NVME_NSIDS[1] = 2;
            NVME_NSIDS[2] = 3;
            NVME_NS_COUNT = 3;
        }
    }

    // 7b. 逐个 namespace 取容量 (Identify Namespace CNS=0 → NSZE)。
    //     卷层用它给「无分区表的整盘」填 sectors —— 此前那一栏恒为 0 (容量未知),
    //     于是文件系统只能按写死的默认值格式化, 一整块盘会被格成迷你分区。
    {
        let n = unsafe { NVME_NS_COUNT };
        let mut i = 0usize;
        while i < n {
            let nsid = unsafe { NVME_NSIDS[i] };
            let sec = nvme_ns_sectors(
                nsid,
                &cfg,
                mmio,
                asq_doorbell,
                acq_doorbell,
                &mut admin_tail,
                &mut admin_head,
                &mut admin_phase,
            );
            unsafe {
                NVME_NS_SECTORS[i] = sec;
            }
            i += 1;
        }
    }

    // 8/9. 建 IO_QUEUES 条 I/O 队列 (qid = 1..=IO_QUEUES), 每条 CQ 用**自己的**中断向量
    //      (IV = 队列下标) —— 于是完成中断能直接区分是哪条队列, 等待时用一个掩码等全部
    //      I/O 队列, 谁先完成都算数 (阶段 40 多向量 + 多队列)。
    //
    //      CDW11: PC=1 (物理连续), IEN=1 (该 CQ **允许产生中断**), IV=队列下标。
    //
    //      IEN 必须显式置位: 它默认是 0, 于是这个 I/O CQ 的完成根本不投中断 —— 轮询路径
    //      看不出来 (CQE 照样写进内存), 但中断路径会一直等不到。Admin 队列的 IEN 不受
    //      本命令影响, 所以只错在 I/O 队列上。
    let mut q = 0usize;
    while q < IO_QUEUES {
        let (cq_paddr, sq_paddr) = if q == 0 {
            (cfg.icq_paddr, cfg.isq_paddr)
        } else {
            (cfg.icq2_paddr, cfg.isq2_paddr)
        };
        let qid = q as u32 + 1;

        sqe = Sqe::zero();
        sqe.opcode = OP_CREATE_IO_CQ;
        sqe.cid = 3 + (q as u16) * 2;
        sqe.prp1 = cq_paddr;
        sqe.cdw10 = qid | ((cfg.io_qdepth as u32 - 1) << 16);
        // CDW11: PC=1 (物理连续), IEN=1 (该 CQ **允许产生中断**), IV = qid (完成队列下标)。
        //
        // IV 取 `qid` 而不是 `q`: 中断向量按**完成队列下标**分配 —— admin CQ 固定是 0
        // (向量 `msix_vector + 0`), 故 qid 1 用 IV=1、qid 2 用 IV=2。写错成 `q` 会让
        // qid 1 与 admin 抢向量 0, 于是 I/O 完成投的还是 admin 那条向量, 而驱动正在等的
        // 是 I/O 队列的向量 —— 表现就是「一直等不到中断、白等一轮看门狗后回退轮询」。
        sqe.cdw11 = 1 | (1 << 1) | (qid << 16);
        if !submit_wait(
            cfg.asq_vaddr,
            cfg.acq_vaddr,
            asq_doorbell,
            acq_doorbell,
            cfg.admin_qdepth as u32,
            mmio,
            ADMIN_WAIT_MASK,
            sqe,
            &mut admin_tail,
            &mut admin_head,
            &mut admin_phase,
        ) {
            println("nvme: Create I/O CQ FAILED");
            return;
        }

        sqe = Sqe::zero();
        sqe.opcode = OP_CREATE_IO_SQ;
        sqe.cid = 4 + (q as u16) * 2;
        sqe.prp1 = sq_paddr;
        sqe.cdw10 = qid | ((cfg.io_qdepth as u32 - 1) << 16);
        sqe.cdw11 = 1 | (qid << 16); // PC=1, CQID=qid (与本队列的 CQ 一一对应)
        if !submit_wait(
            cfg.asq_vaddr,
            cfg.acq_vaddr,
            asq_doorbell,
            acq_doorbell,
            cfg.admin_qdepth as u32,
            mmio,
            ADMIN_WAIT_MASK,
            sqe,
            &mut admin_tail,
            &mut admin_head,
            &mut admin_phase,
        ) {
            println("nvme: Create I/O SQ FAILED");
            return;
        }
        q += 1;
    }

    // 10. 进入块设备服务循环: 经 IPC 接收 BlockReq, 轮转用各条 I/O 队列读扇区。
    //     每条队列各有自己的 tail/head/phase (自己的 SQ/CQ 就是自己的环形队列);
    //     提交哪条由 `io_select_queue` 轮转决定, 门铃与队列内存都由它给出。
    let mut io_tail = [0u32; IO_QUEUES];
    let mut io_head = [0u32; IO_QUEUES];
    // 与 Admin 队列同理: 首条 completion 的 phase tag 为 1。
    let mut io_phase = [1u32; IO_QUEUES];
    let mut io_rr = 0usize;

    // 10b. 卷层初始化: 分配扫描缓冲页, 逐 namespace 解析分区表并登记卷。
    //      此后 `dev` 一律是「卷号」, 实际 I/O 用 (vol.nsid, vol.start_lba + lba)。
    if sys_alloc_page(VOL_SCRATCH_VADDR) != 1 {
        println("nvme: alloc vol scratch FAILED");
        return;
    }
    // 多页 DMA 的 PRP 表页 (> 2 页的传输用它描述第 2..N 页)。
    if sys_alloc_page(PRP_LIST_VADDR) != 1 {
        println("nvme: alloc prp list FAILED");
        return;
    }
    let scratch = VOL_SCRATCH_VADDR as *mut u8;
    vol_reset();
    let nsn = unsafe { NVME_NS_COUNT };
    let mut nsi = 0usize;
    while nsi < nsn {
        let nsid = unsafe { NVME_NSIDS[nsi] };
        // 每个 namespace 轮转一条队列 (启动扫描每次只发一条命令, 队列选择不影响正确性,
        // 但会让「多队列都被真正用过」这件事在日志里留下痕迹)。
        let (qi, isq_doorbell, icq_doorbell) = io_select_queue(mmio, stride, &mut io_rr);
        vol_scan_namespace(
            nsid,
            &cfg,
            mmio,
            isq_doorbell,
            icq_doorbell,
            scratch,
            &mut io_tail[qi],
            &mut io_head[qi],
            &mut io_phase[qi],
        );
        nsi += 1;
    }
    // 卷表 (启动诊断, 也是 `mkfs.mfs <卷号>` 的卷号来源)。
    vol_print_table();
    // 启动期完成路径证据: 卷扫描已经真的发起过块 I/O, 这行说明它们走的是哪条路径。
    nvme_stats_print("nvme: after volume scan ");

    // 11. E1c 自测: 越界 DMA 探针。
    //
    // 内核 VT-d 侧只允许本设备 DMA 到窗口 `[0, 3 GiB)`（见 `kernel/src/arch/iommu.rs` 的
    // `TARGET_WINDOW_LIMIT`）；这里**故意**把一条 NVM 读的 PRP1 指到 **3 GiB 整** —— 恰好是
    // 窗口外第一个地址。开了 IOMMU 时这次**设备发起**的 DMA 会被拒绝（QEMU 侧
    // `vtd_iommu_translate: detected translation failure`；内核侧 FSTS/FRCD 留下记录，
    // 由空闲任务转印到内核日志）；没开 IOMMU 时该物理地址在 QEMU q35 上不属于任何内存区，
    // 写入被丢弃 → 系统同样不受影响，故探针可以**无条件**跑，不需要先问内核要什么标志。
    //
    // 两个刻意的实现选择:
    //   ① 走**轮询**而不是 `submit_wait` —— 一次故意的失败绝不能让中断模式粘性回退成轮询
    //      (那会让验收口径 `poll_cmds = 0` 失效)，也不计入 `nvme: stats` 的命令计数；
    //   ② 放在**卷扫描之后** —— 此后本驱动只用 I/O 队列，探针若让某条命令不再回来，
    //      也只影响探针自己用掉的那个槽位，不会连累已经跑完的启动流程。
    {
        // 走 I/O 队列 0 提交一条 **1 个扇区的读**, 数据落点 PRP1 指到窗口外。
        // 选 NVM Read 而不是 Admin Identify: 卷扫描一路都在用同一条读路径, 它**确定**会
        // 让设备去 PRP1 取数 —— 这正是一次设备发起的 DMA。
        let (isq_doorbell_probe, icq_doorbell_probe) = io_q_doorbells(mmio, stride, 0);
        let (sq_probe, cq_probe) = io_q_vaddrs(&cfg, 0);
        let mut probe = Sqe::zero();
        probe.opcode = OP_READ;
        probe.cid = 0x0F; // 与初始化阶段已用的 cid 1..4 / 0x100+ 错开
        probe.nsid = 1;
        probe.prp1 = IOMMU_PROBE_IOVA;
        probe.cdw10 = 0; // SLBA = 0
        probe.cdw12 = 0; // NLB-1 = 0 → 1 个扇区 (512 B)
        let qd = cfg.io_qdepth as u32;
        let idx = (io_tail[0] % qd) as u64;
        unsafe {
            core::ptr::write_volatile((sq_probe + idx * 64) as *mut Sqe, probe);
        }
        io_tail[0] = (io_tail[0] + 1) % qd;
        wr32(isq_doorbell_probe, io_tail[0]);

        let mut outcome = "no-completion";
        let mut spins = 0u32;
        while spins < NVME_IOMMU_PROBE_SPINS {
            // 读一次 MMIO 逼 QEMU 主循环运行, 设备才有机会处理这条命令。
            let _ = rd32(mmio + REG_CSTS);
            if let Some(ok) = try_complete(
                cq_probe,
                icq_doorbell_probe,
                qd,
                &probe,
                &mut io_head[0],
                &mut io_phase[0],
            ) {
                outcome = if ok { "ok" } else { "cqe-error" };
                break;
            }
            spins += 1;
        }
        print("IOMMU1 out-of-window DMA probe: prp=0x");
        print_hex(IOMMU_PROBE_IOVA);
        print(" window=[0,0x");
        print_hex(IOMMU_PROBE_IOVA);
        print(") probe=");
        println(outcome);
    }

    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);

        if msg.tag != BLOCK_REQ_TAG {
            print("nvme: unexpected tag=");
            print_u64(msg.tag);
            println("");
            sys_reply(0);
            continue;
        }
        let req: BlockReq =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const BlockReq) };

        // `dev` 是「卷号」(卷层已把分区起始 LBA 合并进卷描述)。
        let dev = (req.op >> 8) as usize;
        let opcode_low = (req.op & 0xFF) as u8;
        match opcode_low {
            BLOCK_OP_READ | BLOCK_OP_WRITE => {
                let vol = match vol_get(dev) {
                    Some(v) => v,
                    None => {
                        sys_reply(0);
                        continue;
                    }
                };
                let opcode = if opcode_low == BLOCK_OP_READ {
                    OP_READ
                } else {
                    OP_WRITE
                };
                let base_lba = vol.start_lba.saturating_add(req.lba as u32);
                let total = req.count;
                if total == 0 {
                    sys_reply(0);
                    continue;
                }
                // 超过单条命令上限 (256 扇区 = 128 KiB) 的请求按命令上限切分:
                // 每段起始都落在页边界上 (256 * 512 = 128 KiB = 32 页), 故
                // 除最后一段外每段的缓冲区都是页对齐的。
                let mut done: u64 = 0;
                let mut ok = true;
                while done < total {
                    let chunk = (total - done).min(NVME_MAX_SECTORS as u64) as u16;
                    let seg_buf = (req.buf as *mut u8).wrapping_add((done * 512) as usize);
                    // 每段换一条队列 (轮转): 于是多队列、多向量在整轮自测里都会被走到。
                    let (qi, isq_doorbell, icq_doorbell) =
                        io_select_queue(mmio, stride, &mut io_rr);
                    if !nvme_rw_sectors(
                        opcode,
                        vol.nsid,
                        &cfg,
                        mmio,
                        isq_doorbell,
                        icq_doorbell,
                        base_lba.saturating_add(done as u32),
                        chunk,
                        seg_buf,
                        &mut io_tail[qi],
                        &mut io_head[qi],
                        &mut io_phase[qi],
                    ) {
                        ok = false;
                        break;
                    }
                    done += chunk as u64;
                }
                sys_reply(if ok { 1 } else { 0 });
            }
            BLOCK_OP_LIST_VOLUMES => {
                // 把卷描述符数组写进调用方共享页, 回复卷数。
                let dst = req.buf as *mut u8;
                let max = req.count as usize;
                let n = core::cmp::min(max, unsafe { VOL_COUNT });
                let dsize = core::mem::size_of::<VolumeDesc>();
                let mut i = 0usize;
                while i < n {
                    let v = unsafe { VOLUMES[i] };
                    let d = VolumeDesc {
                        id: i as u32,
                        nsid: v.nsid,
                        start_lba: v.start_lba,
                        sectors: v.sectors,
                        kind: v.kind,
                        _pad: 0,
                    };
                    unsafe {
                        core::ptr::write_unaligned(dst.add(i * dsize) as *mut VolumeDesc, d);
                    }
                    i += 1;
                }
                sys_reply(n as u64);
            }
            // 分区表写入 (S2 卷管理收口): 一律按 nsid 寻址, 见 `PartReq`。
            BLOCK_OP_PART_CREATE | BLOCK_OP_PART_DELETE | BLOCK_OP_PART_WIPE
            | BLOCK_OP_PART_RELOAD | BLOCK_OP_DISK_READ => {
                let preq: PartReq =
                    unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const PartReq) };
                // 整个分区表操作固定用一条队列 (它内部会连发多条命令, 换队列没有好处);
                // 具体哪条仍由轮转决定, 于是多队列都会被用到。
                let (qi, isq_doorbell, icq_doorbell) = io_select_queue(mmio, stride, &mut io_rr);
                let mut io = NvmeIo {
                    cfg: &cfg,
                    mmio,
                    isq: isq_doorbell,
                    icq: icq_doorbell,
                    tail: &mut io_tail[qi],
                    head: &mut io_head[qi],
                    phase: &mut io_phase[qi],
                };
                let nsid = preq.nsid as u32;
                let r = match opcode_low {
                    BLOCK_OP_PART_CREATE => nvme_part_create(
                        &mut io,
                        nsid,
                        preq.arg0,
                        (req.op >> 8) & PART_FLAG_FORCE_MBR != 0,
                        scratch,
                    ),
                    BLOCK_OP_PART_DELETE => nvme_part_delete(&mut io, nsid, preq.arg0, scratch),
                    BLOCK_OP_PART_WIPE => nvme_part_wipe(&mut io, nsid, scratch),
                    BLOCK_OP_PART_RELOAD => part_reload_volumes(&mut io, scratch),
                    // 裸读一个扇区: arg0 = 扇区号, arg1 = 目标缓冲页。
                    _ => {
                        if io.read(nsid, preq.arg0 as u32, 1, preq.arg1 as *mut u8) {
                            1
                        } else {
                            0
                        }
                    }
                };
                sys_reply(r);
            }
            _ => {
                sys_reply(0);
            }
        }
    }
}
// ---------------------------------------------------------------------------
// 域 5 — 磁盘驱动服务 (IDE PIO, 文件系统阶段 1)
// ---------------------------------------------------------------------------
// Legacy IDE (PATA) primary 通道 I/O 端口, 用 ATA PIO 命令读扇区, 不依赖
// DMA / MMIO / MSI-X / PCI bus master, 是最简单的块设备访问路径。
const IDE_DATA: u16 = 0x1F0; // 16 位数据寄存器 (读/写)
const IDE_SECT_CNT: u16 = 0x1F2; // 扇区数
const IDE_LBA_LO: u16 = 0x1F3; // LBA 位 0-7
const IDE_LBA_MID: u16 = 0x1F4; // LBA 位 8-15
const IDE_LBA_HI: u16 = 0x1F5; // LBA 位 16-23
const IDE_DRIVE: u16 = 0x1F6; // 驱动器/磁头 (bit6=1 LBA 模式, bit7=1 master)
const IDE_STATUS: u16 = 0x1F7; // 状态 (读) / 命令 (写)
const IDE_CMD: u16 = 0x1F7; // 命令寄存器 (写)

/// ATA 命令: READ SECTORS (28 位 LBA)。
const ATA_READ_SECTORS: u8 = 0x20;
/// ATA 命令: WRITE SECTORS (28 位 LBA)。
const ATA_WRITE_SECTORS: u8 = 0x30;
/// ATA 命令: IDENTIFY DEVICE (回读 256 个 16 位字的设备参数, 含总扇区数)。
const ATA_IDENTIFY: u8 = 0xEC;

/// 状态寄存器位。
const ATA_BSY: u8 = 0x80; // busy
const ATA_DRDY: u8 = 0x40; // drive ready
const ATA_DRQ: u8 = 0x08; // data request
const ATA_ERR: u8 = 0x01; // error

/// 从 I/O 端口读一个 16 位小端字并写入缓冲区 (避开对齐要求)。
unsafe fn read_sector_word(buf: *mut u8, i: usize) {
    let w = sys_port_in16(IDE_DATA);
    core::ptr::write_unaligned(buf.add(i * 2) as *mut u16, w);
}

/// 从缓冲区读一个 16 位小端字并写入 I/O 端口 (避开对齐要求)。
unsafe fn write_sector_word(buf: *const u8, i: usize) {
    let w = core::ptr::read_unaligned(buf.add(i * 2) as *const u16);
    sys_port_out16(IDE_DATA, w);
}

/// 读 LBA 起 `count` 个扇区到 `buf` (28 位 LBA, PIO 模式)。`count` 取值 1..=256
/// (写入 SECT_CNT 时 256 自动回绕为 0)。`buf` 需至少 `count * 512` 字节。成功返回 true。
fn read_sectors(lba: u32, count: u16, buf: *mut u8) -> bool {
    if count == 0 || count > 256 {
        return false;
    }

    // 1. 等控制器就绪: BSY 清零且 DRDY 置位。
    let mut ready = false;
    for _ in 0..100_000 {
        let s = sys_port_in8(IDE_STATUS);
        if s & ATA_BSY == 0 && s & ATA_DRDY != 0 {
            ready = true;
            break;
        }
    }
    if !ready {
        return false;
    }

    // 2. 写扇区数与 LBA 参数 (28 位 LBA)。
    sys_port_out8(IDE_SECT_CNT, (count & 0xFF) as u8); // 256 -> 0
    sys_port_out8(IDE_LBA_LO, (lba & 0xFF) as u8);
    sys_port_out8(IDE_LBA_MID, ((lba >> 8) & 0xFF) as u8);
    sys_port_out8(IDE_LBA_HI, ((lba >> 16) & 0xFF) as u8);
    sys_port_out8(IDE_DRIVE, 0xE0 | ((lba >> 24) & 0x0F) as u8); // master + LBA
    sys_port_out8(IDE_CMD, ATA_READ_SECTORS);

    // 3. 逐扇区等待 DRQ 后读 256 个 16 位字 (512 字节)。
    for i in 0..count as usize {
        let mut got_drq = false;
        for _ in 0..100_000 {
            let s = sys_port_in8(IDE_STATUS);
            if s & ATA_BSY != 0 {
                continue;
            }
            if s & ATA_ERR != 0 {
                return false;
            }
            if s & ATA_DRQ != 0 {
                got_drq = true;
                break;
            }
        }
        if !got_drq {
            return false;
        }
        let sector = unsafe { buf.add(i * 512) };
        for w in 0..256usize {
            unsafe { read_sector_word(sector, w) };
        }
    }
    true
}

/// 写 LBA 起 `count` 个扇区 (`buf` 为数据源, 28 位 LBA, PIO 模式)。
/// `count` 取值 1..=256 (写入 SECT_CNT 时 256 自动回绕为 0)。成功返回 true。
fn write_sectors(lba: u32, count: u16, buf: *mut u8) -> bool {
    if count == 0 || count > 256 {
        return false;
    }

    // 1. 等控制器就绪: BSY 清零且 DRDY 置位。
    let mut ready = false;
    for _ in 0..100_000 {
        let s = sys_port_in8(IDE_STATUS);
        if s & ATA_BSY == 0 && s & ATA_DRDY != 0 {
            ready = true;
            break;
        }
    }
    if !ready {
        return false;
    }

    // 2. 写扇区数与 LBA 参数 (28 位 LBA)。
    sys_port_out8(IDE_SECT_CNT, (count & 0xFF) as u8); // 256 -> 0
    sys_port_out8(IDE_LBA_LO, (lba & 0xFF) as u8);
    sys_port_out8(IDE_LBA_MID, ((lba >> 8) & 0xFF) as u8);
    sys_port_out8(IDE_LBA_HI, ((lba >> 16) & 0xFF) as u8);
    sys_port_out8(IDE_DRIVE, 0xE0 | ((lba >> 24) & 0x0F) as u8); // master + LBA
    sys_port_out8(IDE_CMD, ATA_WRITE_SECTORS);

    // 3. 逐扇区等待 DRQ 后写 256 个 16 位字 (512 字节)。
    for i in 0..count as usize {
        let mut got_drq = false;
        for _ in 0..100_000 {
            let s = sys_port_in8(IDE_STATUS);
            if s & ATA_BSY != 0 {
                continue;
            }
            if s & ATA_ERR != 0 {
                return false;
            }
            if s & ATA_DRQ != 0 {
                got_drq = true;
                break;
            }
        }
        if !got_drq {
            return false;
        }
        let sector = unsafe { buf.add(i * 512) };
        for w in 0..256usize {
            unsafe { write_sector_word(sector, w) };
        }
    }
    true
}

/// IDENTIFY DEVICE 的 512 字节回读缓冲 (仅 IDE 回退路径在启动时用一次)。
static mut IDE_IDBUF: [u8; 512] = [0; 512];

/// 向**主盘**发 ATA IDENTIFY DEVICE, 把 512 字节设备参数读进 `buf`。成功返回 true。
///
/// 状态判定遵循 ATA 规范: 写完命令后状态读回 **0** 说明端口上没有设备 (QEMU 未挂
/// `-drive if=ide`); 否则等 BSY 清零 —— 期间 DRQ 置位即数据就绪, ERR/ABRT 置位即失败。
fn ide_identify(buf: *mut u8) -> bool {
    // IDENTIFY 要求扇区数与 LBA 寄存器清零, 驱动器寄存器选 master (0xA0)。
    sys_port_out8(IDE_DRIVE, 0xA0);
    sys_port_out8(IDE_SECT_CNT, 0);
    sys_port_out8(IDE_LBA_LO, 0);
    sys_port_out8(IDE_LBA_MID, 0);
    sys_port_out8(IDE_LBA_HI, 0);
    sys_port_out8(IDE_CMD, ATA_IDENTIFY);

    // 端口上无设备时状态读回 0 (寄存器浮空), 这是唯一的「无盘」信号。
    if sys_port_in8(IDE_STATUS) == 0 {
        return false;
    }
    let mut ready = false;
    for _ in 0..100_000 {
        let s = sys_port_in8(IDE_STATUS);
        if s & ATA_BSY != 0 {
            continue;
        }
        if s & ATA_ERR != 0 {
            return false;
        }
        if s & ATA_DRQ != 0 {
            ready = true;
            break;
        }
    }
    if !ready {
        return false;
    }
    for w in 0..256usize {
        unsafe { read_sector_word(buf, w) };
    }
    true
}

/// 主盘容量 (512 字节扇区数); 取不到返回 0 (容量未知)。
///
/// 优先 LBA48 (word 100-103, 需 word 83 bit10 的支持位), 否则 LBA28 (word 60-61)。
/// 两者都夹在 **28 位 LBA 上限** (`0x0FFF_FFFF` 扇区 = 128 GiB) 内: 本驱动的读写命令只发
/// 28 位 LBA, 报出更大的容量会让上层往根本读不到的区域写。
fn ide_capacity_sectors() -> u32 {
    let buf = core::ptr::addr_of_mut!(IDE_IDBUF).cast::<u8>();
    if !ide_identify(buf) {
        return 0;
    }
    let lba48 = unsafe { read_u16(buf.add(83 * 2)) } & 0x0400 != 0;
    let sectors = if lba48 {
        let lo = unsafe { read_u32(buf.add(100 * 2)) } as u64;
        let hi = unsafe { read_u32(buf.add(102 * 2)) } as u64;
        (hi << 32) | lo
    } else {
        let lo = unsafe { read_u16(buf.add(60 * 2)) } as u64;
        let hi = unsafe { read_u16(buf.add(61 * 2)) } as u64;
        (hi << 16) | lo
    };
    sectors.min(0x0FFF_FFFF) as u32
}

// ---------------------------------------------------------------------------
// 卷层: 把各 namespace 的分区表解析为「卷」, `dev` = 卷号
// ---------------------------------------------------------------------------
// 真实磁盘通常是「分区表 + 分区」, 而文件系统只认「卷」。本层在 block_srv 内完成:
//   - 扫描每个 namespace: 有 MBR/GPT 分区表 → 每个非空分区各成一个卷;
//     没有分区表 → **整个 namespace 视为一个卷** (向后兼容现有三张整盘镜像);
//   - 按卷首签名探测文件系统类型;
//   - 对上层把 `BlockReq.op` 高位定义为「卷号」(取代原先的 namespace 号),
//     实际 I/O 用 `(vol.nsid, vol.start_lba + lba)` 下发。
//
// 向后兼容: 现有三张整盘镜像 (无分区表) 各成一个卷, 卷号 0/1/2 与旧 dev 0/1/2 一致。

/// 卷表上限。
/// 卷扫描临时缓冲页: block_srv 自有 (不共享给任何域)。
///
/// ⚠️ 用户态固定虚拟地址分区 (从 `USER_BASE + 1 MiB` 起, 见各服务顶部注释):
///   0x10_0000..0x10_FFFF  fat32 / app·shell 共享页 / mfs / ext2 块缓冲
///   0x11_0000..0x11_3FFF  mfs GC 遍历 / inode 表 / 索引块 / GC 表块
///   0x11_4000..0x15_3FFF  exFAT 集群缓冲 (**按簇大小最多 64 页**, 故预留整段)
///   0x15_4000           exFAT 位图窗口
///   0x15_5000           exFAT upcase 窗口
///   0x15_6000           exFAT 单页暂存
///   0x16_0000           block_srv 卷扫描页 (本页)   ← 必须在 exFAT 预留段之上
///   0x16_1000           block_srv PRP 表页
/// 新增固定地址时务必对照本表 —— 一旦与别人的共享页重叠, 「同地址共享」会因为
/// 目标域的该地址已被映射而直接触发内核 panic (`map_user_page: PageAlreadyMapped`)。
const VOL_SCRATCH_VADDR: u64 = 0x0000_0080_0016_0000;
/// 建分区时 `PartReq.op` 高位 (flags) 的含义: bit0 = 强制 MBR。
const PART_FLAG_FORCE_MBR: u64 = 1;

/// 分区项大小: GPT 规范值为 128 字节 (必须是 128 的倍数)。
const PART_GPT_ENTRY_SIZE: u32 = 128;
/// 分区项数量: 取规范最常用的 128 项 (128 × 128 B = 16 KiB = 32 扇区)。
const PART_GPT_ENTRY_COUNT: u32 = 128;
/// GPT 项数组占用的扇区数 (128 项 × 128 字节 = 16 KiB = 32 扇区)。
const PART_GPT_ENTRIES_SECTORS: u64 =
    (PART_GPT_ENTRY_COUNT as u64 * PART_GPT_ENTRY_SIZE as u64) / 512;
/// GPT 头部保留的扇区数: 保护性 MBR(1) + 主头(1) + 主项数组(32) = 34。
///
/// 盘尾对称留 32 扇区备份项数组 + 1 扇区备份头, 故 `last_usable = 容量 - PART_GPT_RESERVED`。
const PART_GPT_RESERVED: u64 = 1 + 1 + PART_GPT_ENTRIES_SECTORS;
/// 分区起点对齐到 1 MiB (2048 扇区) —— 与主流分区工具一致, 避开各家 SSD 的擦除块。
const PART_ALIGN_SECTORS: u64 = 2048;
/// MBR 分区项类型字节: 0x83 = Linux 文件系统。
///
/// 建分区时还不知道要 format 成什么 (MorionFS 是**之后**由 `mkfs.mfs` 建的), 故取通用类型;
/// 卷层探测类型靠卷首签名, 不依赖这里。
const PART_MBR_TYPE_LINUX: u8 = 0x83;
/// 保护性 MBR 的类型字节 (覆盖整盘的 GPT 占位项)。
const PART_MBR_TYPE_PROTECTIVE: u8 = 0xEE;

/// 一个卷: 落在某 namespace 上的 [start_lba, start_lba+sectors) 区间。
/// `sectors == 0` 表示**容量未知** (Identify Namespace 没取到, 例如 IDE PIO 回退路径),
/// 此时「卷从 start_lba 起一直到盘尾」, 但没人知道盘尾在哪 —— 需要容量的上层 (MFS
/// 首次格式化) 必须按默认值兜底, 不能把 0 当成「零长度卷」。
#[derive(Clone, Copy)]
struct Volume {
    nsid: u32,
    start_lba: u32,
    sectors: u32,
    kind: u32,
}
const VOL_EMPTY: Volume = Volume {
    nsid: 0,
    start_lba: 0,
    sectors: 0,
    kind: VOL_KIND_UNKNOWN,
};
static mut VOLUMES: [Volume; VOL_MAX] = [VOL_EMPTY; VOL_MAX];
static mut VOL_COUNT: usize = 0;
fn vol_push(nsid: u32, start_lba: u32, sectors: u32, kind: u32) {
    let n = unsafe { VOL_COUNT };
    if n >= VOL_MAX {
        return;
    }
    unsafe {
        VOLUMES[n] = Volume {
            nsid,
            start_lba,
            sectors,
            kind,
        };
        VOL_COUNT = n + 1;
    }
}

fn vol_get(i: usize) -> Option<Volume> {
    if i >= unsafe { VOL_COUNT } {
        None
    } else {
        Some(unsafe { VOLUMES[i] })
    }
}

fn vol_reset() {
    unsafe {
        VOL_COUNT = 0;
    }
}

/// 卷类型的可读名 (与 `VOL_KIND_*` 对应), 仅供卷表打印。
fn vol_kind_name(kind: u32) -> &'static str {
    match kind {
        VOL_KIND_FAT => "fat32",
        VOL_KIND_EXFAT => "exfat",
        VOL_KIND_MFS => "mfs",
        VOL_KIND_EXT2 => "ext2",
        _ => "unknown",
    }
}

/// 打印卷表 (每卷一行 `vol: <卷号> nsid=<n> lba=<n> sectors=<n> kind=<名>`)。
///
/// 启动诊断, 也是 `mkfs.mfs <卷号>` 唯一的信息来源: 卷号由卷层在启动时按扫描顺序
/// 分配, 不打印出来就只能靠猜。`kind=unknown` 的卷是没格式化的 (可被 mkfs 接受)。
fn vol_print_table() {
    let n = unsafe { VOL_COUNT };
    let mut i = 0usize;
    while i < n {
        let v = match vol_get(i) {
            Some(v) => v,
            None => break,
        };
        print("vol: ");
        print_u64(i as u64);
        print(" nsid=");
        print_u64(v.nsid as u64);
        print(" lba=");
        print_u64(v.start_lba as u64);
        print(" sectors=");
        print_u64(v.sectors as u64);
        print(" kind=");
        println(vol_kind_name(v.kind));
        i += 1;
    }
}

/// 按卷首若干字节的签名判断文件系统类型。
///
/// 判据 (先强特征后弱特征): exFAT `"EXFAT   "` (偏移 3) → MFS magic `"MFS0".."MFS9"`
/// (偏移 0) → ext2 magic `0xEF53` (偏移 1080) → FAT 引导扇区尾 `0x55AA` (偏移 510)。
fn vol_detect_kind(buf: *const u8) -> u32 {
    let mut exfat = true;
    for (i, &c) in b"EXFAT   ".iter().enumerate() {
        if unsafe { *buf.add(3 + i) } != c {
            exfat = false;
            break;
        }
    }
    if exfat {
        return VOL_KIND_EXFAT;
    }
    let m = read_u32(buf);
    // "MFS0".."MFS9" (magic 低字节是版本号; 版本不符由挂载路径自行重格式化)。
    if m & 0xFFFF_FF00 == 0x4D46_5300 && (0x30..=0x39).contains(&(m & 0xFF)) {
        return VOL_KIND_MFS;
    }
    if read_u16(unsafe { buf.add(1080) }) == 0xEF53 {
        return VOL_KIND_EXT2;
    }
    let lo = unsafe { *buf.add(510) };
    let hi = unsafe { *buf.add(511) };
    if lo == 0x55 && hi == 0xAA {
        return VOL_KIND_FAT;
    }
    VOL_KIND_UNKNOWN
}

/// 读卷首 8 扇区 (4096 字节) 到 `scratch` 以探测类型。
#[allow(clippy::too_many_arguments)]
fn vol_probe_kind(
    nsid: u32,
    start_lba: u32,
    cfg: &NvmeConfig,
    mmio: u64,
    isq_doorbell: u64,
    icq_doorbell: u64,
    scratch: *mut u8,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) -> u32 {
    if !nvme_rw_sectors(
        OP_READ,
        nsid,
        cfg,
        mmio,
        isq_doorbell,
        icq_doorbell,
        start_lba,
        8,
        scratch,
        tail,
        head,
        phase,
    ) {
        return VOL_KIND_UNKNOWN;
    }
    vol_detect_kind(scratch)
}

/// 解析 GPT 分区表 (仅在检测到保护性 MBR 时进入): 逐个分区项登记卷。
#[allow(clippy::too_many_arguments)]
fn vol_scan_gpt(
    nsid: u32,
    cfg: &NvmeConfig,
    mmio: u64,
    isq_doorbell: u64,
    icq_doorbell: u64,
    scratch: *mut u8,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) {
    // LBA 1 = GPT 头。
    if !nvme_rw_sectors(
        OP_READ,
        nsid,
        cfg,
        mmio,
        isq_doorbell,
        icq_doorbell,
        1,
        1,
        scratch,
        tail,
        head,
        phase,
    ) {
        return;
    }
    let sig = unsafe { core::slice::from_raw_parts(scratch, 8) };
    if sig != b"EFI PART" {
        return;
    }
    let entry_lba = read_u64(unsafe { scratch.add(0x48) });
    let num = read_u32(unsafe { scratch.add(0x50) });
    let esize = read_u32(unsafe { scratch.add(0x54) });
    // 规范要求分区项大小为 128 的倍数; 上限保护避免异常镜像导致长循环。
    if esize < 128 || num == 0 || num > 128 || entry_lba > u32::MAX as u64 {
        return;
    }
    let mut idx = 0u32;
    while idx < num {
        let byte_off = idx as u64 * esize as u64;
        let sec = entry_lba + byte_off / 512;
        let within = (byte_off % 512) as usize;
        if sec > u32::MAX as u64 || within + esize as usize > 512 {
            // 项跨扇区 (非标准 esize): 跳过, 不冒险解析。
            idx += 1;
            continue;
        }
        if !nvme_rw_sectors(
            OP_READ,
            nsid,
            cfg,
            mmio,
            isq_doorbell,
            icq_doorbell,
            sec as u32,
            1,
            scratch,
            tail,
            head,
            phase,
        ) {
            return;
        }
        let e = unsafe { scratch.add(within) };
        // type_guid 全 0 = 未使用项。
        let mut used = false;
        let mut k = 0usize;
        while k < 16 {
            if unsafe { *e.add(k) } != 0 {
                used = true;
                break;
            }
            k += 1;
        }
        if used {
            let first = read_u64(unsafe { e.add(0x20) });
            let last = read_u64(unsafe { e.add(0x28) });
            if last >= first && first <= u32::MAX as u64 {
                let sectors = (last - first + 1).min(u32::MAX as u64) as u32;
                let kind = vol_probe_kind(
                    nsid,
                    first as u32,
                    cfg,
                    mmio,
                    isq_doorbell,
                    icq_doorbell,
                    scratch,
                    tail,
                    head,
                    phase,
                );
                vol_push(nsid, first as u32, sectors, kind);
            }
        }
        idx += 1;
    }
}

/// 扫描一个 namespace: 有分区表则逐分区登记卷, 否则整盘一个卷。
#[allow(clippy::too_many_arguments)]
fn vol_scan_namespace(
    nsid: u32,
    cfg: &NvmeConfig,
    mmio: u64,
    isq_doorbell: u64,
    icq_doorbell: u64,
    scratch: *mut u8,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) {
    // LBA 0: 判断 MBR / GPT / 无分区表。
    if !nvme_rw_sectors(
        OP_READ,
        nsid,
        cfg,
        mmio,
        isq_doorbell,
        icq_doorbell,
        0,
        1,
        scratch,
        tail,
        head,
        phase,
    ) {
        return;
    }
    let boot_sig = unsafe { *scratch.add(510) } == 0x55 && unsafe { *scratch.add(511) } == 0xAA;
    let entries = unsafe { scratch.add(446) };
    // 保护性 MBR (分区项 0 类型 = 0xEE) → 转 GPT。
    if boot_sig && unsafe { *entries.add(4) } == 0xEE {
        vol_scan_gpt(
            nsid,
            cfg,
            mmio,
            isq_doorbell,
            icq_doorbell,
            scratch,
            tail,
            head,
            phase,
        );
        return;
    }
    // 普通 MBR: 4 个 16 字节主分区项 (boot@0, type@4, lba_start@8, sectors@12)。
    // 先把所有项摘出来再逐个探测: 探测文件系统类型会读盘并覆写 `scratch`
    // (即分区表所在缓冲), 边读表边探测会把后续分区项毁掉。
    //
    // ⚠️ 必须校验项的合法性, 不能只看「type != 0 且 count != 0」: 本函数也用于
    // **本身就是分区**的卷 (真机 U 盘的分区用 `-drive file=/dev/sdXN` 接入)。此时
    // 扇区 0 是该分区的 VBR 而不是 MBR, 而 0x55AA 之外 446..509 那段在真实 FAT32
    // 引导代码里是**非零 ASCII**, 会被误读成 4 条主分区项, 于是真正的文件系统卷
    // 永远登记不上、只登记出 4 个指向非法 LBA 的假卷 (读它们直接 `LBA Out of Range`)。
    // 合法主分区项的启动标志只能是 0x00/0x80, 且 LBA 起点不可能为 0 (扇区 0 是 MBR 本身)。
    let mut parts: [(u32, u32); 4] = [(0, 0); 4];
    let mut npart = 0usize;
    let mut i = 0usize;
    while i < 4 {
        let e = unsafe { entries.add(i * 16) };
        let bootflag = unsafe { *e.add(0) };
        let ptype = unsafe { *e.add(4) };
        let start = read_u32(unsafe { e.add(8) });
        let count = read_u32(unsafe { e.add(12) });
        let plausible = (bootflag == 0x00 || bootflag == 0x80) && start != 0;
        if boot_sig && ptype != 0 && count != 0 && plausible {
            parts[npart] = (start, count);
            npart += 1;
        }
        i += 1;
    }
    if npart == 0 {
        // 无分区表 → 整盘一个卷。容量取 Identify Namespace 的 NSZE(在 `vol_probe_kind`
        // 覆盖 scratch 之前查好); 取不到则仍记 0 = 容量未知, 由上层按默认值兜底。
        let sectors = nvme_ns_sectors_of(nsid);
        let kind = vol_probe_kind(
            nsid,
            0,
            cfg,
            mmio,
            isq_doorbell,
            icq_doorbell,
            scratch,
            tail,
            head,
            phase,
        );
        vol_push(nsid, 0, sectors, kind);
        return;
    }
    let mut k = 0usize;
    while k < npart {
        let (start, count) = parts[k];
        let kind = vol_probe_kind(
            nsid,
            start,
            cfg,
            mmio,
            isq_doorbell,
            icq_doorbell,
            scratch,
            tail,
            head,
            phase,
        );
        vol_push(nsid, start, count, kind);
        k += 1;
    }
}
// ---------------------------------------------------------------------------
// 分区表**写入** (S2 卷管理收口): 在整块盘上建/删 GPT 与 MBR 分区
// ---------------------------------------------------------------------------
// 卷层此前只**读**分区表 (vol_scan_*); 这里补上写路径, 于是「新买一块盘 → 分区 → 格式化 →
// 挂载」在客户机里能一条龙走完, 不必再去宿主用 fdisk/sgdisk。
//
// 三条硬约束:
//   1. **按 nsid 寻址** —— 分区表属于整块盘, 卷号只是它的产物 (建之前没有、删完就没了);
//   2. 只动**表**, 不动数据: 删分区只清条目, 数据区一个字节都不碰;
//   3. 认不出来 / 会毁掉既有表的操作一律拒绝 (例如把已有 GPT 的盘改成 MBR)。

/// 一页能装下的 GPT 项数, 也是流式读写的粒度 —— 由下面两个常量**推出来**, 免得手写数字漂移
/// (一项 128 字节, 一页 4096 字节 → 32 项)。
const PART_ENTRIES_PER_CHUNK: u32 = PART_CHUNK_SECTORS as u32 * 512 / PART_GPT_ENTRY_SIZE;
/// 上面那个粒度对应的扇区数 (4096 / 512 = 8), 必须是 `nvme_rw_sectors` 能一次发完的量。
const PART_CHUNK_SECTORS: u16 = 8;
/// GPT 项数组的起始 LBA (规范惯例: 保护性 MBR 在 0, 头在 1, 项数组从 2 起)。
const PART_GPT_ENTRIES_LBA: u64 = 2;

/// 分区表风格 (见 `part_probe_style`)。
const PART_STYLE_NONE: u32 = 0;
const PART_STYLE_MBR: u32 = 1;
const PART_STYLE_GPT: u32 = 2;

/// GPT 分区类型 GUID: Linux 文件系统数据 (`0FC63DAF-8483-4772-8E79-3D69D8477DE4`)。
///
/// 建分区时还不知道要 format 成什么 (MorionFS 是**之后**由 `mkfs.mfs` 建的), 故取通用类型;
/// 卷层探测类型靠卷首签名, 不看这个 GUID。
const PART_GPT_TYPE_LINUX: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

/// NVMe I/O 队列上下文: 分区表读写要连发多条命令, 打包传递, 免得每个函数都长十几参数。
struct NvmeIo<'a> {
    cfg: &'a NvmeConfig,
    mmio: u64,
    isq: u64,
    icq: u64,
    tail: &'a mut u32,
    head: &'a mut u32,
    phase: &'a mut u32,
}

impl NvmeIo<'_> {
    fn rw(&mut self, op: u8, nsid: u32, lba: u32, count: u16, buf: *mut u8) -> bool {
        nvme_rw_sectors(
            op, nsid, self.cfg, self.mmio, self.isq, self.icq, lba, count, buf, self.tail,
            self.head, self.phase,
        )
    }
    fn read(&mut self, nsid: u32, lba: u32, count: u16, buf: *mut u8) -> bool {
        self.rw(OP_READ, nsid, lba, count, buf)
    }
    fn write(&mut self, nsid: u32, lba: u32, count: u16, buf: *mut u8) -> bool {
        self.rw(OP_WRITE, nsid, lba, count, buf)
    }
}

/// 把 `v` 向上对齐到 `align` 的整数倍。
fn align_up(v: u64, align: u64) -> u64 {
    if align == 0 {
        return v;
    }
    v.div_ceil(align) * align
}

/// 探测盘 `nsid` 的分区表风格 (读 LBA 0)。
///
/// 判据与卷层解析 (`vol_scan_namespace`) 保持一致: 没有 `0x55AA`, 或**没有一条合法主分区项**,
/// 都算「无分区表」—— 真实 FAT32 分区的 VBR 里 446..509 是非零引导码, 只看「type != 0」会把
/// 它误判成 MBR, 于是分区操作会去改一个根本不存在的主分区项。
fn part_probe_style(io: &mut NvmeIo, nsid: u32, scratch: *mut u8) -> u32 {
    if !io.read(nsid, 0, 1, scratch) {
        return PART_STYLE_NONE;
    }
    if unsafe { *scratch.add(510) } != 0x55 || unsafe { *scratch.add(511) } != 0xAA {
        return PART_STYLE_NONE;
    }
    let entries = unsafe { scratch.add(446) };
    if unsafe { *entries.add(4) } == PART_MBR_TYPE_PROTECTIVE {
        return PART_STYLE_GPT;
    }
    let mut i = 0usize;
    while i < 4 {
        let e = unsafe { entries.add(i * 16) };
        let bootflag = unsafe { *e.add(0) };
        let ptype = unsafe { *e.add(4) };
        let start = read_u32(unsafe { e.add(8) });
        let count = read_u32(unsafe { e.add(12) });
        if ptype != 0 && count != 0 && start != 0 && (bootflag == 0x00 || bootflag == 0x80) {
            return PART_STYLE_MBR;
        }
        i += 1;
    }
    PART_STYLE_NONE
}

/// 混一个 64 位 (splitmix64 变体) —— 只为把 (盘, 角色) 摊成 GUID 的字节。
fn part_mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// 写一个由 `(seed, role)` **确定性**派生的 16 字节 GUID 到 `out`。
///
/// GPT 只要求 GUID 非 0; 用确定性派生而不是随机数, 是为了回归可复现 (同一块盘、同一个分区
/// 序号每次都得到同一张表, 自测才好断言)。
fn part_guid(seed: u64, role: u64, out: *mut u8) {
    let a = part_mix(seed ^ role.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let b = part_mix(a ^ 0xA5A5_A5A5_A5A5_A5A5);
    write_u64(out, a);
    write_u64(unsafe { out.add(8) }, b);
    if a == 0 && b == 0 {
        // 概率极低, 但全 0 GUID 会被当成「未使用项」, 必须兜住。
        write_u32(out, 0x4D4F_5249); // "MORI"
    }
}

/// GPT 项是否在用 (type GUID 非全 0)。
fn gpt_entry_used(e: *const u8) -> bool {
    let mut i = 0usize;
    while i < 16 {
        if unsafe { *e.add(i) } != 0 {
            return true;
        }
        i += 1;
    }
    false
}

/// GPT 项数组扫描结果 (一次扫完拿全, 免得建/删分区各扫两遍)。
struct GptScan {
    /// 已用项数 (0 = 这张表还没有分区)。
    used: u32,
    /// 第一个空项下标 (没有空位时 = `PART_GPT_ENTRY_COUNT`)。
    first_free: u64,
    /// 已用项的**最大终点 + 1** (即最后一块被占用的 LBA + 1); 无分区时为 `first_usable`。
    max_end: u64,
    /// 本次指定要查的那一项是否已用 (下标越界时为 false)。
    target_used: bool,
}

/// 流式扫一遍 GPT 项数组。失败返回 None。
fn part_scan_gpt(
    io: &mut NvmeIo,
    nsid: u32,
    first_usable: u64,
    target: u64,
    scratch: *mut u8,
) -> Option<GptScan> {
    let per = PART_ENTRIES_PER_CHUNK as u64;
    let chunks = (PART_GPT_ENTRY_COUNT as u64).div_ceil(per) as u32;
    let mut s = GptScan {
        used: 0,
        first_free: PART_GPT_ENTRY_COUNT as u64,
        max_end: first_usable,
        target_used: false,
    };
    let mut c = 0u32;
    while c < chunks {
        // ⚠️ 步长是**扇区数**(8), 不是项数(32): 一项 128 字节, 32 项才 4 KiB = 8 扇区。
        let lba = PART_GPT_ENTRIES_LBA + (c as u64) * PART_CHUNK_SECTORS as u64;
        if !io.read(nsid, lba as u32, PART_CHUNK_SECTORS, scratch) {
            return None;
        }
        let lo = (c as u64) * per;
        let mut k = 0u64;
        while k < per {
            let e = unsafe { scratch.add((k as usize) * PART_GPT_ENTRY_SIZE as usize) };
            if gpt_entry_used(e) {
                s.used += 1;
                let last = read_u64(unsafe { e.add(0x28) });
                if last + 1 > s.max_end {
                    s.max_end = last + 1;
                }
                if lo + k == target {
                    s.target_used = true;
                }
            } else if s.first_free == PART_GPT_ENTRY_COUNT as u64 {
                s.first_free = lo + k;
            }
            k += 1;
        }
        c += 1;
    }
    Some(s)
}

/// 要在 GPT 项数组里落地的一处改动 (新建一项 = 填字段; 删除 = 清空)。
#[derive(Clone, Copy)]
struct GptPatch {
    index: u64,
    clear: bool,
    first_lba: u64,
    last_lba: u64,
    type_guid: [u8; 16],
    uniq_guid: [u8; 16],
}

/// 把一个 GPT 头写进 `sector` (512 字节): 填字段并算好自校验 CRC32。
///
/// CRC 覆盖头的前 92 字节 (`header_size`), 计算时 `header_crc32` 字段本身必须为 0 ——
/// 所以先清零、填完全部字段、最后才算 CRC 并回填。
#[allow(clippy::too_many_arguments)] // 头部字段本来就有这么多, 打包成结构反而更难对照规范
fn gpt_build_header(
    sector: *mut u8,
    current_lba: u64,
    backup_lba: u64,
    first_usable: u64,
    last_usable: u64,
    disk_guid: &[u8; 16],
    entries_lba: u64,
    entry_crc: u32,
) {
    unsafe { core::ptr::write_bytes(sector, 0, 512) };
    for (i, &c) in b"EFI PART".iter().enumerate() {
        unsafe { *sector.add(i) = c };
    }
    write_u32(unsafe { sector.add(0x08) }, 0x0001_0000); // revision 1.0
    write_u32(unsafe { sector.add(0x0C) }, 92); // header_size (规范固定)
    write_u64(unsafe { sector.add(0x18) }, current_lba);
    write_u64(unsafe { sector.add(0x20) }, backup_lba);
    write_u64(unsafe { sector.add(0x28) }, first_usable);
    write_u64(unsafe { sector.add(0x30) }, last_usable);
    let mut i = 0usize;
    while i < 16 {
        unsafe { *sector.add(0x38 + i) = disk_guid[i] };
        i += 1;
    }
    write_u64(unsafe { sector.add(0x48) }, entries_lba);
    write_u32(unsafe { sector.add(0x50) }, PART_GPT_ENTRY_COUNT);
    write_u32(unsafe { sector.add(0x54) }, PART_GPT_ENTRY_SIZE);
    write_u32(unsafe { sector.add(0x58) }, entry_crc);
    let crc = mfs_crc32(unsafe { core::slice::from_raw_parts(sector, 92) });
    write_u32(unsafe { sector.add(0x10) }, crc);
}

/// 流式重写 GPT 的**两份**项数组与两份头, 顺带把 `patch` 应用到项数组上。
///
/// 一次只碰一页 (8 扇区 = 32 项), 所以 16 KiB 的项数组不需要额外缓冲; 项数组 CRC 边读边算
/// (`crc32_update`)。主副本写完立刻复制到盘尾的备份区 —— 只认主表的工具能跑, 但真实工具链
/// (sgdisk / 固件) 会校验备份, 缺了它这张表就是半成品。
fn gpt_flush(
    io: &mut NvmeIo,
    nsid: u32,
    cap: u64,
    patch: &GptPatch,
    scratch: *mut u8,
) -> Option<u32> {
    let per = PART_ENTRIES_PER_CHUNK as u64;
    let chunks = (PART_GPT_ENTRY_COUNT as u64).div_ceil(per) as u32;
    let backup_entries_lba = cap - PART_GPT_ENTRIES_SECTORS - 1;
    let mut reg: u32 = 0xFFFF_FFFF;
    let mut c = 0u32;
    while c < chunks {
        // 步长用**扇区数**(8): 32 项 × 128 字节 = 4 KiB = 8 扇区 (不是 32)。
        let lba = PART_GPT_ENTRIES_LBA + (c as u64) * PART_CHUNK_SECTORS as u64;
        if !io.read(nsid, lba as u32, PART_CHUNK_SECTORS, scratch) {
            return None;
        }
        let lo = (c as u64) * per;
        if patch.index >= lo && patch.index < lo + per {
            let e =
                unsafe { scratch.add((patch.index - lo) as usize * PART_GPT_ENTRY_SIZE as usize) };
            unsafe { core::ptr::write_bytes(e, 0, PART_GPT_ENTRY_SIZE as usize) };
            if !patch.clear {
                let mut i = 0usize;
                while i < 16 {
                    unsafe { *e.add(i) = patch.type_guid[i] };
                    unsafe { *e.add(0x10 + i) = patch.uniq_guid[i] };
                    i += 1;
                }
                write_u64(unsafe { e.add(0x20) }, patch.first_lba);
                write_u64(unsafe { e.add(0x28) }, patch.last_lba);
                // 名字 (UTF-16LE): 写个可读标识, 宿主工具里一眼看出是谁建的。
                for (i, &ch) in b"MorionFS".iter().enumerate() {
                    unsafe { *e.add(0x38 + i * 2) = ch };
                }
            }
        }
        reg = crc32_update(reg, unsafe { core::slice::from_raw_parts(scratch, 4096) });
        if !io.write(nsid, lba as u32, PART_CHUNK_SECTORS, scratch) {
            return None;
        }
        if !io.write(
            nsid,
            (backup_entries_lba + (c as u64) * PART_CHUNK_SECTORS as u64) as u32,
            PART_CHUNK_SECTORS,
            scratch,
        ) {
            return None;
        }
        c += 1;
    }
    let entry_crc = !reg;
    let mut disk_guid = [0u8; 16];
    part_guid(nsid as u64, 0, disk_guid.as_mut_ptr());
    gpt_build_header(
        scratch,
        1,
        cap - 1,
        PART_GPT_RESERVED,
        cap - PART_GPT_RESERVED,
        &disk_guid,
        PART_GPT_ENTRIES_LBA,
        entry_crc,
    );
    if !io.write(nsid, 1, 1, scratch) {
        return None;
    }
    gpt_build_header(
        scratch,
        cap - 1,
        1,
        PART_GPT_RESERVED,
        cap - PART_GPT_RESERVED,
        &disk_guid,
        backup_entries_lba,
        entry_crc,
    );
    if !io.write(nsid, (cap - 1) as u32, 1, scratch) {
        return None;
    }
    Some(entry_crc)
}

/// 写保护性 MBR (LBA 0): 一条 `0xEE` 项覆盖整盘, 让只认 MBR 的工具看到「这盘被 GPT 占了」。
fn part_write_protective_mbr(io: &mut NvmeIo, nsid: u32, cap: u64, scratch: *mut u8) -> bool {
    unsafe { core::ptr::write_bytes(scratch, 0, 512) };
    write_u32(unsafe { scratch.add(440) }, 0x4D4F_0000 | nsid); // 磁盘签名 (非 0)
    let e = unsafe { scratch.add(446) };
    unsafe { *e.add(0) = 0x00 };
    unsafe { *e.add(4) = PART_MBR_TYPE_PROTECTIVE };
    write_u32(unsafe { e.add(8) }, 1); // 从 LBA 1 (GPT 头) 起
    write_u32(unsafe { e.add(12) }, (cap - 1).min(0xFFFF_FFFF) as u32);
    unsafe { *scratch.add(510) = 0x55 };
    unsafe { *scratch.add(511) = 0xAA };
    io.write(nsid, 0, 1, scratch)
}

/// 在盘上建一个 GPT 分区, 返回 `(起点 LBA, 扇区数)`; 失败 None。
fn part_create_gpt(
    io: &mut NvmeIo,
    nsid: u32,
    cap: u64,
    size_sectors: u64,
    scratch: *mut u8,
) -> Option<(u64, u64)> {
    let last_usable = cap - PART_GPT_RESERVED;
    let scan = part_scan_gpt(io, nsid, PART_GPT_RESERVED, u64::MAX, scratch)?;
    if scan.first_free >= PART_GPT_ENTRY_COUNT as u64 {
        println("block: part create refused (GPT full: 128 entries)");
        return None;
    }
    let start = align_up(scan.max_end, PART_ALIGN_SECTORS).max(PART_GPT_RESERVED);
    if start > last_usable {
        println("block: part create refused (no room left on disk)");
        return None;
    }
    let room = last_usable - start + 1;
    let size = if size_sectors == 0 {
        room
    } else {
        size_sectors
    };
    if size > room {
        println("block: part create refused (requested size exceeds free space)");
        return None;
    }
    let mut uniq = [0u8; 16];
    part_guid(nsid as u64, scan.first_free + 1, uniq.as_mut_ptr());
    let patch = GptPatch {
        index: scan.first_free,
        clear: false,
        first_lba: start,
        last_lba: start + size - 1,
        type_guid: PART_GPT_TYPE_LINUX,
        uniq_guid: uniq,
    };
    if gpt_flush(io, nsid, cap, &patch, scratch).is_none() {
        println("block: part create FAILED (write GPT)");
        return None;
    }
    Some((start, size))
}

/// 在盘上建一个 MBR 主分区, 返回 `(起点 LBA, 扇区数)`; 失败 None。
///
/// MBR 的 LBA / 大小字段都是 32 位, 故可用空间夹在 `u32::MAX` 扇区 (≈2 TiB) 内 —— 更大的盘
/// 只能用 GPT, 请求超界直接拒绝而不是悄悄截断。
fn part_create_mbr(
    io: &mut NvmeIo,
    nsid: u32,
    cap: u64,
    size_sectors: u64,
    scratch: *mut u8,
) -> Option<(u64, u64)> {
    let cap32 = cap.min(0xFFFF_FFFF);
    if !io.read(nsid, 0, 1, scratch) {
        return None;
    }
    let mut slot = usize::MAX;
    let mut floor: u64 = 1; // 分区不可能从 LBA 0 起 (那里是 MBR 自己)
    let mut i = 0usize;
    while i < 4 {
        let e = unsafe { scratch.add(446 + i * 16) };
        let ptype = unsafe { *e.add(4) };
        let start = read_u32(unsafe { e.add(8) }) as u64;
        let count = read_u32(unsafe { e.add(12) }) as u64;
        if ptype != 0 && count != 0 {
            if start + count > floor {
                floor = start + count;
            }
        } else if slot == usize::MAX {
            slot = i;
        }
        i += 1;
    }
    if slot == usize::MAX {
        println("block: part create refused (MBR full: 4 primary partitions)");
        return None;
    }
    let start = align_up(floor, PART_ALIGN_SECTORS).max(PART_ALIGN_SECTORS);
    if start >= cap32 {
        println("block: part create refused (no room left on disk)");
        return None;
    }
    let room = cap32 - start;
    let size = if size_sectors == 0 {
        room
    } else {
        size_sectors
    };
    if size > room {
        println("block: part create refused (requested size exceeds free space)");
        return None;
    }
    // 就地改 LBA0: 引导码 (0..440) 原样保留, 只改磁盘签名与分区项 —— 空白盘上引导码本来就是 0。
    write_u32(unsafe { scratch.add(440) }, 0x4D4F_0000 | nsid);
    let e = unsafe { scratch.add(446 + slot * 16) };
    unsafe { core::ptr::write_bytes(e, 0, 16) };
    unsafe { *e.add(0) = 0x00 }; // 非活动分区
    unsafe { *e.add(4) = PART_MBR_TYPE_LINUX };
    write_u32(unsafe { e.add(8) }, start as u32);
    write_u32(unsafe { e.add(12) }, size as u32);
    unsafe { *scratch.add(510) = 0x55 };
    unsafe { *scratch.add(511) = 0xAA };
    if !io.write(nsid, 0, 1, scratch) {
        println("block: part create FAILED (write MBR)");
        return None;
    }
    Some((start, size))
}

/// 把 LBA 0 的分区表区 (偏移 440..512: 磁盘签名 + 4 个项 + `0x55AA`) 清零。
///
/// 卷层要求「有 `0x55AA` **且**至少一条合法分区项」才算 MBR, 所以只清这一片就足够让盘回到
/// 「无分区表」—— 引导码留着无害, 真机上也更愿意保留。
fn mbr_clear_table(io: &mut NvmeIo, nsid: u32, scratch: *mut u8) -> bool {
    if !io.read(nsid, 0, 1, scratch) {
        return false;
    }
    unsafe { core::ptr::write_bytes(scratch.add(440), 0, 72) };
    io.write(nsid, 0, 1, scratch)
}

/// 清空盘 `nsid` 的分区表 (GPT 连头与两份项数组一起抹掉), 让它回到「无分区表」。
fn nvme_part_wipe(io: &mut NvmeIo, nsid: u32, scratch: *mut u8) -> u64 {
    let cap = nvme_ns_sectors_of(nsid) as u64;
    let style = part_probe_style(io, nsid, scratch);
    if !mbr_clear_table(io, nsid, scratch) {
        println("block: part wipe FAILED (write LBA 0)");
        return 0;
    }
    // 只有 GPT 需要额外清理: 主头 + 主项数组 (LBA 1..=33) 与盘尾的备份 (cap-33..cap-1)。
    // 残留的旧头会让 `sgdisk` 之类看到「有表的残骸」, 也容易被下次建表时的探测误判。
    if style == PART_STYLE_GPT && cap > 2 * PART_GPT_RESERVED {
        let mut lba = 1u64;
        let head_end = PART_GPT_RESERVED - 1; // 33
        while lba <= head_end {
            let n = core::cmp::min(PART_CHUNK_SECTORS as u64, head_end - lba + 1) as u16;
            unsafe { core::ptr::write_bytes(scratch, 0, n as usize * 512) };
            if !io.write(nsid, lba as u32, n, scratch) {
                println("block: part wipe FAILED (clear primary header)");
                return 0;
            }
            lba += n as u64;
        }
        let tail_start = cap - PART_GPT_ENTRIES_SECTORS - 1;
        let mut lba = tail_start;
        while lba < cap {
            let n = core::cmp::min(PART_CHUNK_SECTORS as u64, cap - lba) as u16;
            unsafe { core::ptr::write_bytes(scratch, 0, n as usize * 512) };
            if !io.write(nsid, lba as u32, n, scratch) {
                println("block: part wipe FAILED (clear backup)");
                return 0;
            }
            lba += n as u64;
        }
    }
    // 表没了 -> 卷表要跟着变 (这块盘回到「一个整盘卷」)。
    part_reload_volumes(io, scratch);
    1
}

/// 删掉盘 `nsid` 上序号为 `index` 的分区 (**只清条目, 数据区一个字节都不动**)。
///
/// 删完若一个分区都不剩, 整张表被清空 (盘回到空白) —— 否则盘上会留一张「合法但空」的表,
/// 下次要在空白盘上建 MBR 就没机会了。
fn nvme_part_delete(io: &mut NvmeIo, nsid: u32, index: u64, scratch: *mut u8) -> u64 {
    let cap = nvme_ns_sectors_of(nsid) as u64;
    match part_probe_style(io, nsid, scratch) {
        PART_STYLE_GPT => {
            if cap < 2 * PART_GPT_RESERVED || index >= PART_GPT_ENTRY_COUNT as u64 {
                println("block: part delete refused (no such partition)");
                return 0;
            }
            let scan = match part_scan_gpt(io, nsid, PART_GPT_RESERVED, index, scratch) {
                Some(s) => s,
                None => {
                    println("block: part delete FAILED (read GPT)");
                    return 0;
                }
            };
            if !scan.target_used {
                println("block: part delete refused (no such partition)");
                return 0;
            }
            if scan.used == 1 {
                return nvme_part_wipe(io, nsid, scratch);
            }
            let patch = GptPatch {
                index,
                clear: true,
                first_lba: 0,
                last_lba: 0,
                type_guid: [0; 16],
                uniq_guid: [0; 16],
            };
            if gpt_flush(io, nsid, cap, &patch, scratch).is_none() {
                println("block: part delete FAILED (write GPT)");
                return 0;
            }
            part_reload_volumes(io, scratch);
            1
        }
        PART_STYLE_MBR => {
            if index >= 4 || !io.read(nsid, 0, 1, scratch) {
                println("block: part delete refused (no such partition)");
                return 0;
            }
            let e = unsafe { scratch.add(446 + index as usize * 16) };
            let ptype = unsafe { *e.add(4) };
            let count = read_u32(unsafe { e.add(12) });
            if ptype == 0 || count == 0 {
                println("block: part delete refused (no such partition)");
                return 0;
            }
            unsafe { core::ptr::write_bytes(e, 0, 16) };
            // 还有别的分区吗? 没有就整张表清掉 (盘回到空白)。
            let mut rest = 0usize;
            let mut i = 0usize;
            while i < 4 {
                let pe = unsafe { scratch.add(446 + i * 16) };
                if unsafe { *pe.add(4) } != 0 && read_u32(unsafe { pe.add(12) }) != 0 {
                    rest += 1;
                }
                i += 1;
            }
            if rest == 0 {
                unsafe { core::ptr::write_bytes(scratch.add(440), 0, 72) };
            }
            if !io.write(nsid, 0, 1, scratch) {
                println("block: part delete FAILED (write MBR)");
                return 0;
            }
            part_reload_volumes(io, scratch);
            1
        }
        _ => {
            println("block: part delete refused (no partition table)");
            0
        }
    }
}

/// 重扫全部 namespace 重建卷表 (分区表改动后调用) 并打印新表; 返回重读后的卷数。
fn part_reload_volumes(io: &mut NvmeIo, scratch: *mut u8) -> u64 {
    vol_reset();
    let nsn = unsafe { NVME_NS_COUNT };
    let mut i = 0usize;
    while i < nsn {
        let nsid = unsafe { NVME_NSIDS[i] };
        vol_scan_namespace(
            nsid,
            io.cfg,
            io.mmio,
            io.isq,
            io.icq,
            scratch,
            &mut *io.tail,
            &mut *io.head,
            &mut *io.phase,
        );
        i += 1;
    }
    vol_print_table();
    unsafe { VOL_COUNT as u64 }
}

/// 在卷表里找 `(nsid, start_lba)` 对应的卷号 —— 分区表换过之后用它把「刚建的分区」翻成卷号。
fn vol_id_of(nsid: u32, start_lba: u32) -> Option<u64> {
    let n = unsafe { VOL_COUNT };
    let mut i = 0usize;
    while i < n {
        let v = unsafe { VOLUMES[i] };
        if v.nsid == nsid && v.start_lba == start_lba {
            return Some(i as u64);
        }
        i += 1;
    }
    None
}

/// 建分区 (S2 收口): 决定表风格 → 写表 → 重读卷表 → 返回新分区的**卷号**。
///
/// 表风格按盘自适应 (已有 MBR 就继续 MBR、已有 GPT 就继续 GPT), **空白盘默认 GPT**;
/// `force_mbr` 只在盘上还没有分区表时有效 —— 已有 GPT 时强制 MBR 会毁掉整张表, 直接拒绝。
fn nvme_part_create(
    io: &mut NvmeIo,
    nsid: u32,
    size_sectors: u64,
    force_mbr: bool,
    scratch: *mut u8,
) -> u64 {
    let cap = nvme_ns_sectors_of(nsid) as u64;
    if cap == 0 {
        println("block: part create refused (disk capacity unknown)");
        return u64::MAX;
    }
    if cap < 2 * PART_GPT_RESERVED {
        println("block: part create refused (disk too small for a partition table)");
        return u64::MAX;
    }
    let style = part_probe_style(io, nsid, scratch);
    if style == PART_STYLE_GPT && force_mbr {
        println("block: part create refused (disk already has GPT; --mbr cannot convert)");
        return u64::MAX;
    }
    let use_gpt = match style {
        PART_STYLE_GPT => true,
        PART_STYLE_MBR => false,
        _ => !force_mbr,
    };
    let (start, size) = if use_gpt {
        let r = match part_create_gpt(io, nsid, cap, size_sectors, scratch) {
            Some(r) => r,
            None => return u64::MAX,
        };
        if style != PART_STYLE_GPT {
            // 保护性 MBR **最后**写: 万一前面失败, 盘读作「无分区表」而不是「有 GPT 但头不对」。
            if !part_write_protective_mbr(io, nsid, cap, scratch) {
                println("block: part create FAILED (write protective MBR)");
                return u64::MAX;
            }
        }
        r
    } else {
        match part_create_mbr(io, nsid, cap, size_sectors, scratch) {
            Some(r) => r,
            None => return u64::MAX,
        }
    };
    print("part-dbg: create nsid=");
    print_u64(nsid as u64);
    print(if use_gpt {
        " gpt start="
    } else {
        " mbr start="
    });
    print_u64(start);
    print(" sectors=");
    print_u64(size);
    println("");
    // 重读分区表: 新分区立刻成为卷, 于是可以直接 `mkfs.mfs <卷号>`。
    part_reload_volumes(io, scratch);
    match vol_id_of(nsid, start as u32) {
        Some(id) => id,
        None => {
            println("block: part create FAILED (new partition not registered)");
            u64::MAX
        }
    }
}
/// 域 5 — 块设备服务: 优先 NVMe (若内核已配置), 否则 IDE PIO 回退。
///
/// 接收 `BlockReq` (op/lba/count/buf), 读扇区写入调用方共享的缓冲页,
/// 回复状态 tag (1=成功, 0=失败)。数据经共享页零拷贝回传, IPC 仅传控制信息。
pub fn run() {
    let magic =
        unsafe { core::ptr::read_volatile(libdevice::grant::DEVICE_CFG_VADDR as *const u64) };
    if magic == libdevice::grant::DEVICE_GRANT_MAGIC {
        nvme_main();
    } else {
        ide_block_main();
    }
}

/// 域 5 — IDE PIO 块设备服务 (无 NVMe 控制器时回退)。
///
/// M1 范围: IDE 单盘仅登记「整盘一个卷」(不扫描分区表), 故 `dev == 0` 的行为与
/// 引入卷层之前完全一致, `dev > 0` 一律失败。IDE 盘的分区扫描留待后续。
fn ide_block_main() {
    vol_reset();
    // 容量向盘要 (ATA IDENTIFY DEVICE), 不再恒为「未知」—— 从前 `sectors = 0` 会让
    // MFS 首次格式化退回 16 MiB 默认尺寸, 整块 IDE 盘也只用得上一点点。
    let sectors = ide_capacity_sectors();
    vol_push(0, 0, sectors, VOL_KIND_UNKNOWN);
    // 卷表 (启动诊断): IDE 回退路径只有一个整盘卷; 问不到容量时 sectors=0。
    vol_print_table();
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

        // 从 payload 解出块设备请求。
        let req: BlockReq =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const BlockReq) };

        // `dev` 是卷号 (IDE 只有一个整盘卷, 即 dev 0)。
        let dev = (req.op >> 8) as usize;
        let opcode = (req.op & 0xFF) as u8;
        match opcode {
            BLOCK_OP_READ | BLOCK_OP_WRITE => {
                let vol = match vol_get(dev) {
                    Some(v) => v,
                    None => {
                        sys_reply(0);
                        continue;
                    }
                };
                let lba = vol.start_lba.saturating_add(req.lba as u32);
                let ok = if opcode == BLOCK_OP_READ {
                    read_sectors(lba, req.count as u16, req.buf as *mut u8)
                } else {
                    write_sectors(lba, req.count as u16, req.buf as *mut u8)
                };
                sys_reply(if ok { 1 } else { 0 });
            }
            BLOCK_OP_LIST_VOLUMES => {
                let dst = req.buf as *mut u8;
                let n = if req.count as usize >= 1 {
                    1usize
                } else {
                    0usize
                };
                if n == 1 {
                    let v = vol_get(0).unwrap_or(VOL_EMPTY);
                    let d = VolumeDesc {
                        id: 0,
                        nsid: v.nsid,
                        start_lba: v.start_lba,
                        sectors: v.sectors,
                        kind: v.kind,
                        _pad: 0,
                    };
                    unsafe {
                        core::ptr::write_unaligned(dst as *mut VolumeDesc, d);
                    }
                }
                sys_reply(n as u64);
            }
            _ => {
                // 分区表写入 (`BLOCK_OP_PART_*`) 与裸盘读目前**只实现了 NVMe 路径**:
                // IDE 回退路径只登记一块整盘、也没有 nsid 概念, 拿卷号去分区会与 NVMe 的
                // 语义分叉。真要在 IDE 盘上分区时再补 (回归走 NVMe, 见 FS-26)。
                sys_reply(0);
            }
        }
    }
}
