//! NVMe 驱动服务域配置 (文件系统阶段 1)
//!
//! 内核负责:
//!   1. 分配物理连续的 DMA 内存 (队列 + 数据缓冲);
//!   2. 把 BAR0 (MMIO) 与 DMA 内存映射到 `nvme_srv` 域的约定虚拟地址;
//!   3. 把各缓冲的物理/虚拟地址写入配置结构 (映射到约定地址);
//!   4. 授予 `nvme_srv` `Mmio` 能力。
//!
//! 用户态 `nvme_srv` 读配置结构后自行完成控制器初始化 (Admin Queue、
//! Identify、I/O 队列、read/write)。NVMe 队列/缓冲必须物理连续且页对齐。

use crate::memory::{frame_allocator, paging};

/// 页大小 (字节)。
const PAGE: u64 = 4096;

/// 配置结构 / MMIO / DMA 在 `nvme_srv` 域内的约定虚拟地址。
/// 位于用户程序镜像 + 用户栈之上 (USER_BASE + 0x1000), 从 0x10000 起预留。
pub const NVME_CFG_VADDR: u64 = paging::USER_SPACE_BASE + 0x1_0000;
pub const NVME_MMIO_VADDR: u64 = paging::USER_SPACE_BASE + 0x2_0000;
pub const NVME_DMA_VADDR: u64 = paging::USER_SPACE_BASE + 0x3_0000;

/// DMA 区域页数: ASQ / ACQ / ISQ / ICQ / 数据缓冲, 共 5 页 (物理连续)。
pub const NVME_DMA_PAGES: u64 = 5;

/// 配置结构 magic (校验内核与用户态布局一致)。
pub const NVME_CONFIG_MAGIC: u64 = 0x4E56_4D45_4F53_21; // "NVM EOS!"

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
}

/// 配置 NVMe 服务域: 分配 DMA、映射 BAR0 与 DMA、写入配置、授权 Mmio。
///
/// `bar0` 为 NVMe BAR0 物理地址 (内部会做页对齐)。成功返回 `true`。
pub fn setup(nvme_domain: u64, bar0: u64) -> bool {
    let bar0 = bar0 & !0xFFF;

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
    };
    unsafe {
        core::ptr::write(cfg_paddr as *mut NvmeConfig, cfg);
    }

    // 5. 映射配置页、BAR0 (MMIO, 2 页覆盖寄存器 + doorbell) 与 DMA 各页到 nvme_srv。
    paging::map_user_page(nvme_domain, NVME_CFG_VADDR, cfg_paddr);
    paging::map_mmio(nvme_domain, NVME_MMIO_VADDR, bar0);
    paging::map_mmio(nvme_domain, NVME_MMIO_VADDR + PAGE, bar0 + PAGE);
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
