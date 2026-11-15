//! 设备资源与 DMA 切分。
//!
//! 内核通过 `DeviceGrant` 交给驱动一块**物理连续、页对齐**的 DMA 块，以及 BAR0
//! 映射的虚拟地址。本模块把它切成 CORB、RIRB、BDL、PCM 四个区域，并做对齐/容量
//! 检查。任何越界或不对齐都返回 [`DriverError::Invalid`]，让服务层在 `start` 阶段
//! 就失败，而不是 DMA 起来后踩坏内存。

use libdevmodel::DriverError;

use crate::regs::{BDLE_BYTES, BDLE_MAX_BYTES};

/// HDA 对 BDL / PCM 缓冲的常见对齐要求（128 字节）。
pub const DMA_ALIGN: u64 = 128;
/// CORB 区大小（1024 字节 = 256 条）。控制器实际条目数由只读的 `CORBSIZE` 报告，
/// 这里按规范最大值预留 —— 环大小不能由软件写（写 `CORBSIZE` 是无效动作）。
pub const CORB_BYTES: u64 = 1024;
/// RIRB 区大小（2048 字节 = 256 条 × 8 字节），同样按规范最大值预留。
pub const RIRB_BYTES: u64 = 2048;
/// BDL 最多 8 条描述符，即单个流最多 32 KiB PCM 环。
pub const MAX_BDL_ENTRIES: usize = 8;
/// BDL 区固定预留的字节数（8 * 16）。
pub const BDL_RESERVED: u64 = (MAX_BDL_ENTRIES as u64) * 16;
/// PCM 环的最小长度（128 个 16 位样本）。
pub const MIN_PCM_BYTES: u64 = 256;
/// PCM 环的最大长度（受 BDL 条目数限制）。
pub const MAX_PCM_BYTES: u64 = (MAX_BDL_ENTRIES as u64) * 4096;

/// 控制器需要的全部资源。
///
/// `mmio_vaddr` 是 BAR0 的虚拟映射，`dma_*` 来自 `DeviceGrant`（物理地址给硬件，
/// 虚拟地址给 CPU 读写）。真实服务层直接从 `DeviceGrant` 填这个结构。
#[derive(Clone, Copy, Debug)]
pub struct HdaResources {
    /// BAR0 虚拟基址。
    pub mmio_vaddr: u64,
    /// BAR0 映射长度。
    pub mmio_len: u64,
    /// DMA 块虚拟基址。
    pub dma_vaddr: u64,
    /// DMA 块物理基址（写进 CORB/RIRB/BDL 寄存器）。
    pub dma_paddr: u64,
    /// DMA 块长度。
    pub dma_len: u64,
}

/// DMA 切分结果。所有 `*_vaddr` / `*_paddr` 都是绝对地址。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaLayout {
    pub corb_vaddr: u64,
    pub corb_paddr: u64,
    pub corb_len: u64,
    pub rirb_vaddr: u64,
    pub rirb_paddr: u64,
    pub rirb_len: u64,
    pub bdl_vaddr: u64,
    pub bdl_paddr: u64,
    pub bdl_len: u64,
    pub pcm_vaddr: u64,
    pub pcm_paddr: u64,
    pub pcm_len: u64,
    /// BDL 需要写多少条（PCM 长度 / 4096 向上取整）。
    pub bdl_entries: usize,
}

const fn align_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

impl HdaResources {
    /// 校验资源并切分 DMA 区。
    pub fn layout(&self) -> Result<DmaLayout, DriverError> {
        // MMIO 至少要覆盖到流 0 的 BDL 基址寄存器（0x98 + 4）。
        if self.mmio_vaddr == 0 || self.mmio_len < 0xA0 {
            return Err(DriverError::Invalid);
        }
        if !self.mmio_vaddr.is_multiple_of(4) {
            return Err(DriverError::Invalid);
        }
        if self.dma_vaddr == 0 || self.dma_paddr == 0 || self.dma_len == 0 {
            return Err(DriverError::Invalid);
        }
        // BDL 与 PCM 的基址必须 128 字节对齐；内核通常给页对齐，这里仍显式检查。
        if !self.dma_vaddr.is_multiple_of(DMA_ALIGN) || !self.dma_paddr.is_multiple_of(DMA_ALIGN) {
            return Err(DriverError::Invalid);
        }

        let corb_off = 0u64;
        let rirb_off = align_up(corb_off + CORB_BYTES, DMA_ALIGN);
        let bdl_off = align_up(rirb_off + RIRB_BYTES, DMA_ALIGN);
        let pcm_off = align_up(bdl_off + BDL_RESERVED, DMA_ALIGN);

        if self.dma_len < pcm_off + MIN_PCM_BYTES {
            return Err(DriverError::Invalid);
        }

        // PCM 环限制在 32 KiB 内，并向下对齐到 128 字节（BDL 条目长度必须是 128 的倍数）。
        let pcm_len = core::cmp::min(self.dma_len - pcm_off, MAX_PCM_BYTES) & !(DMA_ALIGN - 1);
        if pcm_len < MIN_PCM_BYTES {
            return Err(DriverError::Invalid);
        }
        let bdl_entries = pcm_len.div_ceil(BDLE_MAX_BYTES as u64) as usize;
        if bdl_entries == 0 || bdl_entries > MAX_BDL_ENTRIES {
            return Err(DriverError::Invalid);
        }
        let bdl_len = (bdl_entries as u64) * BDLE_BYTES as u64;

        Ok(DmaLayout {
            corb_vaddr: self.dma_vaddr + corb_off,
            corb_paddr: self.dma_paddr + corb_off,
            corb_len: CORB_BYTES,
            rirb_vaddr: self.dma_vaddr + rirb_off,
            rirb_paddr: self.dma_paddr + rirb_off,
            rirb_len: RIRB_BYTES,
            bdl_vaddr: self.dma_vaddr + bdl_off,
            bdl_paddr: self.dma_paddr + bdl_off,
            bdl_len,
            pcm_vaddr: self.dma_vaddr + pcm_off,
            pcm_paddr: self.dma_paddr + pcm_off,
            pcm_len,
            bdl_entries,
        })
    }
}
