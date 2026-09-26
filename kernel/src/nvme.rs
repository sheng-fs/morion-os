//! NVMe 驱动服务域配置 (文件系统阶段 1)
//!
//! 内核负责:
//!   1. 分配物理连续的 DMA 内存 (队列 + 数据缓冲);
//!   2. 把 BAR0 (MMIO) 与 DMA 内存映射到 `nvme_srv` 域的约定虚拟地址;
//!   3. 把各缓冲的物理/虚拟地址写入配置结构 (映射到约定地址);
//!   4. 授予 `nvme_srv` `Mmio` 能力;
//!   5. MSI-X 中断配置 (阶段 4): 使能 LAPIC、关 INTx、写 MSI-X 表项并使能、授予
//!      `Irq(vector)` 能力, 把向量写进配置结构 —— 驱动只负责等中断 (等不到就回退轮询)。
//!
//! 用户态 `nvme_srv` 读配置结构后自行完成控制器初始化 (Admin Queue、
//! Identify、I/O 队列、read/write)。NVMe 队列/缓冲必须物理连续且页对齐。

use crate::arch::{apic, idt, pci};
use crate::memory::{frame_allocator, paging};

/// 页大小 (字节)。
const PAGE: u64 = 4096;

/// 配置结构 / MMIO / DMA 在 `nvme_srv` 域内的约定虚拟地址。
///
/// 布置在 `USER_BASE + 8 MiB` 起的高地址数据区, 远离用户程序镜像 (自 `USER_BASE`
/// 起且随代码增长) 与用户栈 (`USER_BASE + 4 MiB`), 避免镜像增长后踩到这些固定映射。
/// 共享页 (`USER_BASE + 0x80_0000`) 亦属同一数据区, 由 sender/receiver 使用。
pub const NVME_CFG_VADDR: u64 = paging::USER_SPACE_BASE + 0x81_0000;
pub const NVME_MMIO_VADDR: u64 = paging::USER_SPACE_BASE + 0x82_0000;
pub const NVME_DMA_VADDR: u64 = paging::USER_SPACE_BASE + 0x83_0000;

/// DMA 区域页数: ASQ / ACQ / ISQ / ICQ / 数据缓冲, 共 5 页 (物理连续)。
pub const NVME_DMA_PAGES: u64 = 5;

/// BAR0 需要映射进驱动域的页数。
///
/// NVMe 规范要求 BAR0 至少 16 KiB: 0x0000 控制器寄存器 / 0x1000 门铃 /
/// 0x2000 MSI-X 表 / 0x3000 PBA。多映射的这两页是 MSI-X 表所在处 —— 驱动自己
/// 不写表 (内核写), 但把整段映射齐便于将来把表交给驱动。
pub const NVME_MMIO_PAGES: u64 = 4;

/// 配置结构 magic (校验内核与用户态布局一致)。
pub const NVME_CONFIG_MAGIC: u64 = 0x004E_564D_454F_5321; // "NVM EOS!"

/// 内核写入、用户态读取的 NVMe 配置结构。
/// `#[repr(C)]` 保证跨 crate 布局一致。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvmeConfig {
    pub magic: u64,
    /// NVMe BAR0 物理地址 (页对齐)。
    pub bar0_paddr: u64,
    /// BAR0 映射到 `nvme_srv` 的虚拟地址 (寄存器基址)。
    pub mmio_vaddr: u64,
    /// 各队列 / 缓冲物理地址 (写 SQE 与 ASQ/ACQ 寄存器用)。
    pub asq_paddr: u64,
    pub acq_paddr: u64,
    pub isq_paddr: u64,
    pub icq_paddr: u64,
    pub data_paddr: u64,
    /// 各队列 / 缓冲虚拟地址 (驱动读写用)。
    pub asq_vaddr: u64,
    pub acq_vaddr: u64,
    pub isq_vaddr: u64,
    pub icq_vaddr: u64,
    pub data_vaddr: u64,
    /// Admin / I/O 队列深度 (条目数, 取 2 的幂)。
    ///
    /// 深度不得低于 2, 也不得把队列撑破其 DMA 页: 每个队列固定 1 页 (4096B),
    /// ASQ/ISQ 每条 SQE 64B → 上限 64 条; ACQ/ICQ 每条 CQE 16B → 上限 256 条。
    /// 取 64: ASQ/ISQ 正好填满一页, 且远小于 MQES(2047), 又足够驱动命令数。
    pub admin_qdepth: u16,
    pub io_qdepth: u16,
    /// 页大小 (恒 4096)。
    pub page_size: u32,
    /// MSI-X 中断向量 (0 = 未启用 MSI-X, 驱动走轮询)。
    ///
    /// 非 0 时驱动的启用顺序是: ① 把表项 0 写到 `mmio_vaddr + msix_table_offset`;
    /// ② `SYS_MSIX_ENABLE` 请内核打开 MSI-X (配置空间写留在内核); ③ `SYS_REGISTER_IRQ`
    /// 注册本向量; ④ 之后用 `SYS_IRQ_POLL(vector)` 等完成中断。任一步失败都回退轮询。
    pub msix_vector: u32,
    /// MSI-X 表相对 BAR0 的字节偏移 (表在 BAR0 内, 已随 BAR0 一起映射给驱动)。
    pub msix_table_offset: u32,
    /// MSI-X 中断消息地址 (低 32 位; 物理目的模式, 高 32 位恒 0)。
    pub msix_addr: u32,
}

/// 配置 NVMe 服务域: 配置 MSI-X、分配 DMA、映射 BAR0 与 DMA、写入配置、授权能力。
///
/// `bar0` 为 NVMe BAR0 物理地址 (内部会做页对齐); `(bus, dev, func)` 是控制器的
/// PCI 位置 (配置空间访问 MSI-X 能力用)。成功返回 `true`。
pub fn setup(nvme_domain: u64, bus: u8, dev: u8, func: u8, bar0: u64) -> bool {
    let bar0 = bar0 & !0xFFF;

    // 0. MSI-X (阶段 4)。必须在写配置结构之前完成: 向量/表位置要写进配置结构。
    let msix = setup_msix(nvme_domain, bus, dev, func);

    // 1. 分配物理连续 DMA 内存 (ASQ/ACQ/ISQ/ICQ/data)。
    let dma_paddr = match frame_allocator::allocate_frames(NVME_DMA_PAGES as usize) {
        Some(p) => p,
        None => return false,
    };
    // 清零 DMA 区域 (队列内存须从干净状态开始; 物理地址 < 4 GiB 在恒等映射内)。
    unsafe {
        core::ptr::write_bytes(dma_paddr as *mut u8, 0, (NVME_DMA_PAGES * PAGE) as usize);
    }

    // 2. 分配配置页。
    let cfg_paddr = match frame_allocator::allocate_frame() {
        Some(p) => p,
        None => return false,
    };

    // 3. 各队列虚拟/物理地址 (DMA 区域内连续分布, 天然页对齐 + 物理连续)。
    let (asq_paddr, acq_paddr, isq_paddr, icq_paddr, data_paddr) = (
        dma_paddr,
        dma_paddr + PAGE,
        dma_paddr + 2 * PAGE,
        dma_paddr + 3 * PAGE,
        dma_paddr + 4 * PAGE,
    );
    let (asq_vaddr, acq_vaddr, isq_vaddr, icq_vaddr, data_vaddr) = (
        NVME_DMA_VADDR,
        NVME_DMA_VADDR + PAGE,
        NVME_DMA_VADDR + 2 * PAGE,
        NVME_DMA_VADDR + 3 * PAGE,
        NVME_DMA_VADDR + 4 * PAGE,
    );

    // 4. 写配置结构到配置页 (恒等映射, 直接以物理地址作为指针写)。
    let cfg = NvmeConfig {
        magic: NVME_CONFIG_MAGIC,
        bar0_paddr: bar0,
        mmio_vaddr: NVME_MMIO_VADDR,
        asq_paddr,
        acq_paddr,
        isq_paddr,
        icq_paddr,
        data_paddr,
        asq_vaddr,
        acq_vaddr,
        isq_vaddr,
        icq_vaddr,
        data_vaddr,
        admin_qdepth: 64,
        io_qdepth: 64,
        page_size: 4096,
        msix_vector: msix.vector,
        msix_table_offset: msix.table_offset,
        msix_addr: msix.msg_addr,
    };
    unsafe {
        core::ptr::write(cfg_paddr as *mut NvmeConfig, cfg);
    }

    // 5. 映射配置页、BAR0 (MMIO, 4 页覆盖寄存器 + 门铃 + MSI-X 表/PBA) 与 DMA 各页。
    paging::map_user_page(nvme_domain, NVME_CFG_VADDR, cfg_paddr);
    for i in 0..NVME_MMIO_PAGES {
        paging::map_mmio(nvme_domain, NVME_MMIO_VADDR + i * PAGE, bar0 + i * PAGE);
    }
    for i in 0..NVME_DMA_PAGES {
        paging::map_user_page(nvme_domain, NVME_DMA_VADDR + i * PAGE, dma_paddr + i * PAGE);
    }

    // 6. 授予 MMIO 能力 (允许 nvme_srv 后续自行管理映射)。
    crate::cap::grant(nvme_domain, crate::cap::Capability::Mmio(bar0));

    true
}

/// 无 NVMe 控制器时的降级配置: 仅映射一个零配置页到约定地址,
/// 让 `nvme_srv` 读到 `magic == 0` (不等于 `NVME_CONFIG_MAGIC`) 后自行优雅退出,
/// 而不是读未映射地址触发缺页死循环。
pub fn setup_empty(nvme_domain: u64) {
    let cfg_paddr = match frame_allocator::allocate_frame() {
        Some(p) => p,
        None => return,
    };
    // 分配器不保证新帧内容为 0, 显式清零。
    unsafe {
        core::ptr::write_bytes(cfg_paddr as *mut u8, 0, PAGE as usize);
    }
    paging::map_user_page(nvme_domain, NVME_CFG_VADDR, cfg_paddr);
}

/// MSI-X 的配置结果 (向量 0 = 未启用)。
struct MsixSetup {
    vector: u32,
    table_offset: u32,
    msg_addr: u32,
}

const MSIX_DISABLED: MsixSetup = MsixSetup {
    vector: 0,
    table_offset: 0,
    msg_addr: 0,
};

/// 待打开的 MSI-X (`setup_msix` 记下, 由驱动域经 `SYS_MSIX_ENABLE` 在写好表项后打开)。
static MSIX_PENDING: spin::Mutex<Option<MsixPending>> = spin::Mutex::new(None);

/// 待打开项: 控制器位置 + MSI-X 能力偏移 + 允许调用 `SYS_MSIX_ENABLE` 的域。
struct MsixPending {
    domain: u64,
    bus: u8,
    dev: u8,
    func: u8,
    cap_ptr: u8,
}

/// 打开 MSI-X (由该控制器的驱动域调用, 且必须在它写好 MSI-X 表项之后)。
///
/// 顺序是硬要求: 表项没写好就置 Enable, 设备可能按表里未定义的内容发中断消息。
/// 幂等: 第一次成功后清掉待打开项, 之后的调用返回 `false`。
/// PCI 配置空间写留在内核 —— 驱动只能通过这一个窄接口, 动不了别的设备。
pub fn enable_msix() -> bool {
    let mut slot = MSIX_PENDING.lock();
    let pending = match slot.take() {
        Some(p) => p,
        None => return false,
    };
    if crate::scheduler::current_domain() != pending.domain {
        // 不是这台控制器的驱动域: 放回待打开项并拒绝。
        *slot = Some(pending);
        return false;
    }
    pci::enable_msix(pending.bus, pending.dev, pending.func, pending.cap_ptr);
    crate::video::println("nvme: MSI-X enabled (driver wrote table, kernel set config)");
    true
}

/// 为 NVMe 控制器准备 MSI-X, 成功返回向量与表位置, 任一步不满足返回禁用状态。
///
/// 分工: 内核负责 LAPIC、PCI 配置空间与向量段, 驱动负责写设备 MMIO 里的 MSI-X 表 ——
/// 内核自身到不了这个 BAR (由固件分配在 4 GiB 以上, 不在内核的恒等映射内), 而该 BAR
/// 本来就已经非缓存地映射给了驱动。任一步不满足都只**降级** (打印原因 + 返回禁用),
/// 不失败启动 —— 中断化是优化, 轮询才是保底路径。
fn setup_msix(nvme_domain: u64, bus: u8, dev: u8, func: u8) -> MsixSetup {
    let cap = match pci::find_msix(bus, dev, func) {
        Some(c) => c,
        None => {
            crate::video::println("nvme: no MSI-X capability, IRQ mode off (polling)");
            return MSIX_DISABLED;
        }
    };
    // 表必须落在 BAR0 (BIR=0), 且要能被映射给驱动的那几页盖住。
    if cap.table_bir != 0 {
        crate::video::print("nvme: MSI-X table in BAR");
        crate::video::print_u64(cap.table_bir as u64);
        crate::video::println(", not BAR0 -> polling");
        return MSIX_DISABLED;
    }
    let table_end = cap.table_offset as u64 + cap.table_size as u64 * 16;
    if table_end > NVME_MMIO_PAGES * PAGE {
        crate::video::println("nvme: MSI-X table outside mapped BAR0 window -> polling");
        return MSIX_DISABLED;
    }
    // LAPIC 是 MSI 消息的接收方, 没有它就没有中断可言。
    if apic::init().is_none() {
        crate::video::println("nvme: no LAPIC, MSI-X off (polling)");
        return MSIX_DISABLED;
    }
    let vector = idt::MSI_VECTOR_BASE as u32;

    // 先关 INTx (MSI-X 开起来后 INTx 必须不再触发), 再交给驱动写表、请内核打开。
    pci::disable_intx(bus, dev, func);
    *MSIX_PENDING.lock() = Some(MsixPending {
        domain: nvme_domain,
        bus,
        dev,
        func,
        cap_ptr: cap.cap_ptr,
    });
    // 让驱动能 `SYS_REGISTER_IRQ(vector)` / `SYS_IRQ_POLL(vector)`。
    crate::cap::grant(nvme_domain, crate::cap::Capability::Irq(vector as u8));

    crate::video::print("nvme: MSI-X prepared vector=0x");
    crate::video::print_hex(vector as u64);
    crate::video::print(" msi_addr=0x");
    crate::video::print_hex(apic::msi_address() as u64);
    crate::video::print(" table_off=0x");
    crate::video::print_hex(cap.table_offset as u64);
    crate::video::print(" entries=");
    crate::video::print_u64(cap.table_size as u64);
    crate::video::println("");
    MsixSetup {
        vector,
        table_offset: cap.table_offset,
        msg_addr: apic::msi_address(),
    }
}
