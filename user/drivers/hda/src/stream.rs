//! 播放流：BDL 描述符、循环 PCM 缓冲与硬件位置跟踪。
//!
//! 播放路径不依赖中断：`write_pcm` 把样本写进 DMA 环，`drain` 轮询 SDnLPIB 直到
//! 硬件追平。HDA 的 LPIB 在 CBL 内回绕，所以这里按"增量模 CBL"累计已播字节数。

use libdevmodel::DriverError;

use crate::io::HdaIo;
use crate::regs::{
    sd_reg, BDLE_BYTES, BDLE_IOC, BDLE_MAX_BYTES, SD_BDPL, SD_BDPU, SD_CBL, SD_CTL,
    SD_CTL_ID_SHIFT, SD_CTL_RESET, SD_CTL_RUN, SD_CTL_TAG_SHIFT, SD_FMT, SD_LPIB, SD_LVI, SD_STS,
};
use crate::resources::{DmaLayout, MAX_BDL_ENTRIES};

/// `drain` 的最长轮询次数。每次 `stall(64)`；真实硬件的 `stall` 实现应做有意义的延时。
const DRAIN_POLL_LIMIT: u32 = 10_000_000;

/// 一条播放流。
pub(crate) struct PlaybackStream {
    stream: u8,
    cbl: u32,
    entries: usize,
    pcm_vaddr: u64,
    pcm_paddr: u64,
    bdl_vaddr: u64,
    bdl_paddr: u64,
    /// 软件累计写入字节数。
    written: u64,
    /// 硬件累计已播字节数。
    hw_total: u64,
    /// 上一次读到的 LPIB 原始值（已按 CBL 归约）。
    last_lpib: u32,
    running: bool,
}

impl PlaybackStream {
    pub(crate) fn new(layout: &DmaLayout, stream: u8) -> Self {
        Self {
            stream,
            cbl: layout.pcm_len as u32,
            entries: layout.bdl_entries,
            pcm_vaddr: layout.pcm_vaddr,
            pcm_paddr: layout.pcm_paddr,
            bdl_vaddr: layout.bdl_vaddr,
            bdl_paddr: layout.bdl_paddr,
            written: 0,
            hw_total: 0,
            last_lpib: 0,
            running: false,
        }
    }

    pub(crate) fn queued_bytes(&self) -> u64 {
        self.written.saturating_sub(self.hw_total)
    }

    /// 停止 DMA。可重复调用。
    pub(crate) fn stop<I: HdaIo>(&mut self, io: &mut I) {
        let ctl = sd_reg(self.stream, SD_CTL);
        // SDnCTL 是 24 位寄存器，不能用 32 位写覆盖后面的 SDnSTS。
        io.wr8(ctl, 0);
        io.wr8(ctl + 1, 0);
        io.wr8(ctl + 2, 0);
        // 清 FIFO / descriptor error 位（写 1 清除）。
        io.wr8(sd_reg(self.stream, SD_STS), 0x1C);
        io.fence();
        self.running = false;
        self.written = 0;
        self.hw_total = 0;
        self.last_lpib = 0;
    }

    /// 配置并启动一条播放流。调用前 codec 的 converter 格式应已设置好。
    pub(crate) fn start<I: HdaIo>(
        &mut self,
        io: &mut I,
        format_word: u16,
        stream_tag: u8,
    ) -> Result<(), DriverError> {
        self.stop(io);
        // 规范要求的流复位序列：SRST=1 → 读回 1（进入复位）→ SRST=0 → 读回 0（可操作）。
        // 部分控制器（旧 QEMU）写 1 后立刻读回 0，故"等 1"做成尽力而为；"等 0"必须成功，
        // 否则后面的寄存器写入会被控制器忽略。
        let ctl_off = sd_reg(self.stream, SD_CTL);
        io.wr8(ctl_off, (SD_CTL_RESET & 0xFF) as u8);
        for _ in 0..1_000 {
            if io.rd8(ctl_off) & 1 != 0 {
                break;
            }
            io.stall(1);
        }
        io.wr8(ctl_off, 0);
        let mut spins = 0u32;
        while io.rd8(ctl_off) & 1 != 0 {
            if spins >= 10_000 {
                return Err(DriverError::Timeout);
            }
            io.stall(8);
            spins += 1;
        }

        let entries = program_bdl(self.bdl_vaddr, self.pcm_paddr, self.cbl)?;
        if entries != self.entries {
            return Err(DriverError::Invalid);
        }

        io.wr32(sd_reg(self.stream, SD_CBL), self.cbl);
        io.wr16(sd_reg(self.stream, SD_LVI), (entries - 1) as u16);
        io.wr32(sd_reg(self.stream, SD_BDPL), self.bdl_paddr as u32);
        io.wr32(sd_reg(self.stream, SD_BDPU), (self.bdl_paddr >> 32) as u32);
        io.wr16(sd_reg(self.stream, SD_FMT), format_word);
        io.fence();

        let ctl = SD_CTL_RUN
            | (((stream_tag & 0x0F) as u32) << SD_CTL_TAG_SHIFT)
            | (0u32 << SD_CTL_ID_SHIFT);
        // 一次写完整的 24 位 CTL（RUN 与 stream tag 必须**同一次写**到达控制器：
        // 字节序写入会先置 RUN、后置 tag，控制器在 RUN 时通知 codec 的 stream 号还是 0，
        // codec 侧配置的 stream 1 永远等不到启动 —— QEMU 上表现为 LPIB 恒 0）。
        io.wr32(ctl_off, ctl);
        io.fence();

        self.running = true;
        self.written = 0;
        self.hw_total = 0;
        self.last_lpib = 0;
        Ok(())
    }

    /// 写入 16 位样本，返回实际接收的样本数（可能短写）。
    pub(crate) fn write<I: HdaIo>(
        &mut self,
        io: &mut I,
        samples: &[i16],
    ) -> Result<usize, DriverError> {
        if !self.running {
            return Err(DriverError::BadState);
        }
        if samples.is_empty() {
            return Ok(0);
        }
        self.refresh(io);

        let queued = self.queued_bytes().min(self.cbl as u64);
        let free_bytes = self.cbl as u64 - queued;
        let accepted = core::cmp::min(samples.len() as u64, free_bytes / 2) as usize;
        if accepted == 0 {
            return Ok(0);
        }

        let bytes = accepted * 2;
        let start = (self.written % self.cbl as u64) as usize;
        let cap = self.cbl as usize;
        let dst = self.pcm_vaddr as *mut u8;
        unsafe {
            if start + bytes <= cap {
                core::ptr::copy_nonoverlapping(
                    samples.as_ptr() as *const u8,
                    dst.add(start),
                    bytes,
                );
            } else {
                // 环形缓冲尾部写满后，剩余部分从 0 继续。
                let first = cap - start;
                core::ptr::copy_nonoverlapping(
                    samples.as_ptr() as *const u8,
                    dst.add(start),
                    first,
                );
                core::ptr::copy_nonoverlapping(
                    (samples.as_ptr() as *const u8).add(first),
                    dst,
                    bytes - first,
                );
            }
        }
        self.written += bytes as u64;
        // DMA 读的是同一块非缓存内存；提交写序再返回。
        io.fence();
        Ok(accepted)
    }

    /// 等待已写入的样本播完。
    pub(crate) fn drain<I: HdaIo>(&mut self, io: &mut I) -> Result<(), DriverError> {
        if !self.running {
            return Ok(());
        }
        let mut spins = 0u32;
        loop {
            self.refresh(io);
            if self.queued_bytes() == 0 {
                return Ok(());
            }
            if spins >= DRAIN_POLL_LIMIT {
                return Err(DriverError::Timeout);
            }
            io.stall(64);
            spins += 1;
        }
    }

    fn refresh<I: HdaIo>(&mut self, io: &mut I) {
        if self.cbl == 0 {
            return;
        }
        let raw = io.rd32(sd_reg(self.stream, SD_LPIB));
        let pos = if raw >= self.cbl { raw % self.cbl } else { raw };
        let delta = (pos + self.cbl - self.last_lpib) % self.cbl;
        self.hw_total = self.hw_total.saturating_add(delta as u64);
        self.last_lpib = pos;
    }
}

/// 写 BDL：把 PCM 环切成最多 8 条、每条不超过 4 KiB、长度 128 字节对齐的条目。
///
/// BDLE 布局（16 字节）：`[0..8]` 缓冲物理地址，`[8..12]` 长度，`[12..16]` 标志。
pub(crate) fn program_bdl(
    bdl_vaddr: u64,
    pcm_paddr: u64,
    pcm_len: u32,
) -> Result<usize, DriverError> {
    if pcm_len == 0 || !pcm_len.is_multiple_of(128) {
        return Err(DriverError::Invalid);
    }
    let entries = pcm_len.div_ceil(BDLE_MAX_BYTES) as usize;
    if entries == 0 || entries > MAX_BDL_ENTRIES {
        return Err(DriverError::Invalid);
    }

    let mut offset = 0u32;
    for i in 0..entries {
        let len = core::cmp::min(BDLE_MAX_BYTES, pcm_len - offset);
        let flags = if i + 1 == entries { BDLE_IOC } else { 0 };
        let e = bdl_vaddr + i as u64 * BDLE_BYTES as u64;
        let addr = pcm_paddr + offset as u64;
        libdevice::mmio::wr32(e, addr as u32);
        libdevice::mmio::wr32(e + 4, (addr >> 32) as u32);
        libdevice::mmio::wr32(e + 8, len);
        libdevice::mmio::wr32(e + 12, flags);
        offset += len;
    }
    libdevice::mmio::fence();
    Ok(entries)
}
