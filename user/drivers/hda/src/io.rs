//! MMIO 抽象与日志槽 —— 让驱动核心能在宿主机用假寄存器单测。
//!
//! 核心逻辑只依赖 [`HdaIo`] 这一组 8/16/32 位寄存器读写；真实硬件实现
//! [`MmioHdaIo`] 用 [`libdevice::mmio`] 的易失读写，测试实现则可以在内存里
//! 模拟寄存器与 codec 响应。

use libdevmodel::DriverError;

use crate::regs::{
    CORBLBASE, CORBSIZE, CORBUBASE, CORBWP, ICS, ICS_ICB, ICS_IRV, ICW, IRR, RIRBLBASE, RIRBSTS,
    RIRBUBASE, RIRBWP,
};

/// 立即命令的轮询上限。每次失败等待一个 `stall` 单位，避免硬件无响应时死循环。
const IC_POLL_LIMIT: u32 = 100_000;

/// 从只读的 `CORBSIZE` / `RIRBSIZE` 编码换算环条目数（0=2、1=16、2=256）。
fn ring_entries<I: HdaIo>(io: &mut I) -> u32 {
    match io.rd8(CORBSIZE) & 0x0F {
        0 => 2,
        1 => 16,
        2 => 256,
        _ => 256,
    }
}

/// HDA 寄存器访问抽象。偏移都是相对 BAR0 基址的字节偏移。
///
/// 所有方法都带 `&mut self`，于是测试替身可以借读写推进内部状态（例如 LPIB）。
pub trait HdaIo {
    fn rd8(&mut self, off: u32) -> u8;
    fn rd16(&mut self, off: u32) -> u16;
    fn rd32(&mut self, off: u32) -> u32;
    fn wr8(&mut self, off: u32, value: u8);
    fn wr16(&mut self, off: u32, value: u16);
    fn wr32(&mut self, off: u32, value: u32);

    /// 提交写序（DMA 描述符 / 数据写完后调用）。x86 是 TSO，这里主要挡编译器重排。
    fn fence(&mut self) {}

    /// 短暂等待，用于轮询硬件状态。真实实现自旋；测试替身可借此推进模拟硬件。
    fn stall(&mut self, _units: u32) {}

    /// 把硬件给出的 DMA **物理**地址翻译成 CPU 可访问的虚拟地址。
    ///
    /// 恒等映射的实现可直接返回 `paddr`；`MmioHdaIo` 按授权的 DMA 窗口翻译。
    fn dma_ptr(&self, paddr: u64) -> u64 {
        paddr
    }

    /// 按 DMA 物理地址读/写一个 32 位字（CORB/RIRB 环内存）。
    ///
    /// 默认实现走 `dma_ptr` 的裸指针；测试替身若只模拟 MMIO、不支持环通路，
    /// 应保持 [`HdaIo::supports_rings`] 为 `false`，默认实现就不会被调用。
    fn mem_rd32(&mut self, paddr: u64) -> u32 {
        let p = self.dma_ptr(paddr);
        unsafe { core::ptr::read_volatile(p as *const u32) }
    }

    fn mem_wr32(&mut self, paddr: u64, value: u32) {
        let p = self.dma_ptr(paddr);
        unsafe { core::ptr::write_volatile(p as *mut u32, value) }
    }

    /// 该实现是否支持 CORB/RIRB 通路（需要能访问 DMA 环内存）。
    ///
    /// 立即命令寄存器在部分控制器（QEMU `intel-hda` 就是一例）上不实现，
    /// 此时 [`immediate_command`] 会回退到规范要求的 CORB/RIRB。
    fn supports_rings(&self) -> bool {
        false
    }

    /// 记录 DMA 块的虚拟/物理基址，供 `dma_ptr` 翻译环地址。
    fn set_dma_map(&mut self, _dma_vaddr: u64, _dma_paddr: u64) {}
}

/// 真实硬件的 MMIO 实现：`base` 是 BAR0 映射到本驱动域的虚拟地址。
pub struct MmioHdaIo {
    base: u64,
    dma_vaddr: u64,
    dma_paddr: u64,
}

impl MmioHdaIo {
    pub const fn new(base: u64) -> Self {
        Self {
            base,
            dma_vaddr: 0,
            dma_paddr: 0,
        }
    }

    pub const fn base(&self) -> u64 {
        self.base
    }
}

impl HdaIo for MmioHdaIo {
    fn rd8(&mut self, off: u32) -> u8 {
        libdevice::mmio::rd8(self.base + off as u64)
    }

    fn rd16(&mut self, off: u32) -> u16 {
        libdevice::mmio::rd16(self.base + off as u64)
    }

    fn rd32(&mut self, off: u32) -> u32 {
        libdevice::mmio::rd32(self.base + off as u64)
    }

    fn wr8(&mut self, off: u32, value: u8) {
        libdevice::mmio::wr8(self.base + off as u64, value)
    }

    fn wr16(&mut self, off: u32, value: u16) {
        libdevice::mmio::wr16(self.base + off as u64, value)
    }

    fn wr32(&mut self, off: u32, value: u32) {
        libdevice::mmio::wr32(self.base + off as u64, value)
    }

    fn fence(&mut self) {
        libdevice::mmio::fence()
    }

    fn stall(&mut self, units: u32) {
        for _ in 0..units {
            core::hint::spin_loop();
        }
    }

    fn dma_ptr(&self, paddr: u64) -> u64 {
        if self.dma_vaddr == 0 || paddr < self.dma_paddr {
            paddr
        } else {
            self.dma_vaddr + (paddr - self.dma_paddr)
        }
    }

    fn mem_rd32(&mut self, paddr: u64) -> u32 {
        libdevice::mmio::rd32(self.dma_ptr(paddr))
    }

    fn mem_wr32(&mut self, paddr: u64, value: u32) {
        libdevice::mmio::wr32(self.dma_ptr(paddr), value)
    }

    fn supports_rings(&self) -> bool {
        true
    }

    fn set_dma_map(&mut self, dma_vaddr: u64, dma_paddr: u64) {
        self.dma_vaddr = dma_vaddr;
        self.dma_paddr = dma_paddr;
    }
}

/// 日志槽。核心库不直接打印，服务层传入自己的实现即可把关键事件写进日志。
pub trait HdaLog {
    fn log(&mut self, msg: &str);
}

/// 默认日志实现：丢弃所有消息。
pub struct SilentLog;

impl HdaLog for SilentLog {
    fn log(&mut self, _msg: &str) {}
}

/// 让 `&mut L` 也能充当日志槽，便于服务层把同一个 logger 借给多个驱动。
impl<L: HdaLog + ?Sized> HdaLog for &mut L {
    fn log(&mut self, msg: &str) {
        (**self).log(msg)
    }
}

/// 通过立即命令寄存器（ICW/IRR/ICS）发一条 verb 并取回响应。
///
/// 首选立即命令：它不需要维护环形缓冲的指针，codec 发现阶段几十条 verb 足够用。
/// 但立即命令寄存器**并非所有控制器都实现**（QEMU `intel-hda` 即如此），因此超时后
/// 自动回退到 HDA 规范要求的 CORB/RIRB 通路 —— 二者返回同一份 verb 响应。
pub fn immediate_command<I: HdaIo>(io: &mut I, cmd: u32) -> Result<u32, DriverError> {
    match immediate_command_direct(io, cmd) {
        Ok(resp) => Ok(resp),
        Err(DriverError::Timeout) if io.supports_rings() => corb_command(io, cmd),
        Err(e) => Err(e),
    }
}

/// 立即命令寄存器通路（ICW → 等 ICS.IRV → 读 IRR）。
fn immediate_command_direct<I: HdaIo>(io: &mut I, cmd: u32) -> Result<u32, DriverError> {
    let mut spins = 0u32;
    // 1. 等上一条命令完成（ICB = 0）。
    while io.rd32(ICS) & ICS_ICB != 0 {
        if spins >= IC_POLL_LIMIT {
            return Err(DriverError::Timeout);
        }
        io.stall(1);
        spins += 1;
    }

    // 2. 写命令字。
    io.wr32(ICW, cmd);
    io.fence();

    // 3. 等响应有效（IRV = 1），读 IRR，再写 IRV 清除。
    spins = 0;
    loop {
        let ics = io.rd32(ICS);
        if ics & ICS_IRV != 0 {
            let resp = io.rd32(IRR);
            io.wr32(ICS, ICS_IRV);
            return Ok(resp);
        }
        if spins >= IC_POLL_LIMIT {
            return Err(DriverError::Timeout);
        }
        io.stall(1);
        spins += 1;
    }
}

/// CORB/RIRB 通路：把 verb 写进命令环，等控制器把响应写进响应环。
///
/// 环由 `HdaDriver::program_rings` 配置（缓冲按规范最大值预留），条目数从只读的
/// `CORBSIZE` 读取 —— 软件不能设置环大小，只能按控制器报告的条目数推进指针。
fn corb_command<I: HdaIo>(io: &mut I, cmd: u32) -> Result<u32, DriverError> {
    let corb = (io.rd32(CORBLBASE) as u64) | ((io.rd32(CORBUBASE) as u64) << 32);
    let rirb = (io.rd32(RIRBLBASE) as u64) | ((io.rd32(RIRBUBASE) as u64) << 32);
    if corb == 0 || rirb == 0 {
        return Err(DriverError::BadState);
    }
    let entries = ring_entries(io);

    // CORBWP 指出最后一条已写条目；新命令写在它的下一条。
    let wp = io.rd16(CORBWP) as u32 % entries;
    let next = (wp + 1) % entries;
    let before = io.rd16(RIRBWP) & 0x0FFF;

    io.mem_wr32(corb + (next as u64) * 4, cmd);
    io.fence();
    io.wr16(CORBWP, next as u16);

    // 等控制器推进 RIRB 写指针（一个响应 8 字节：响应字 + 扩展字）。
    let mut spins = 0u32;
    loop {
        let now = io.rd16(RIRBWP) & 0x0FFF;
        if now != before {
            let resp = io.mem_rd32(rirb + (now as u64) * 8);
            // 清 RIRBSTS 的 IRQ/OVERRUN 位：控制器内部有一个「已收响应数」配额，
            // 达到 RINTCNT 后就不再取 CORB 命令；清状态会把配额归零。
            io.wr8(RIRBSTS, 0x5);
            return Ok(resp);
        }
        if spins >= IC_POLL_LIMIT {
            return Err(DriverError::Timeout);
        }
        io.stall(4);
        spins += 1;
    }
}

/// 固定容量、无分配的日志行构造器：拼出 `"hda: codec=0 dac=5 pin=7"` 这类字符串。
pub(crate) struct LogLine {
    buf: [u8; 64],
    len: usize,
}

impl LogLine {
    pub(crate) fn new() -> Self {
        Self {
            buf: [0; 64],
            len: 0,
        }
    }

    pub(crate) fn push_str(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let n = bytes.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
    }

    pub(crate) fn push_dec(&mut self, mut v: u32) {
        let mut tmp = [0u8; 10];
        let mut i = tmp.len();
        loop {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        // 只有 ASCII 数字，安全。
        self.push_str(core::str::from_utf8(&tmp[i..]).unwrap_or(""));
    }

    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}
