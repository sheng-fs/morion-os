//! `HdaDriver`：把控制器初始化、codec 发现与播放流串成 `libdevmodel::Driver`。
//!
//! 服务层的使用姿势：
//!
//! ```ignore
//! let mut drv = MmioHdaDriver::new_mmio(grant.bar_vaddr, MyLog);
//! drv.start_with(&dev, HdaResources { .. })?;   // 或 set_resources + Driver::start
//! let audio = drv.audio().unwrap();
//! audio.open(AudioFormat::S16_48K_STEREO)?;
//! ```

use libdevmodel::{AudioFormat, AudioOps, Class, DeviceInfo, Driver, DriverError};

use crate::codec::{self, CodecInfo, CodecProfile, GenericProfile};
use crate::io::{HdaIo, HdaLog, LogLine, MmioHdaIo, SilentLog};
use crate::regs::{
    sd_reg, CORBCTL, CORBLBASE, CORBRP, CORBUBASE, CORBWP, GCTL, GCTL_CRST, INTCTL, INTSTS,
    RING_PTR_RESET, RINTCNT, RIRBCTL, RIRBLBASE, RIRBUBASE, RIRBWP, SD_CTL, SD_STS, STATESTS,
    STATESTS_MASK,
};
use crate::resources::{DmaLayout, HdaResources};
use crate::stream::PlaybackStream;

/// 本驱动对外承诺的 PCM 格式。只列最常用的两种；新增格式需要同时确认
/// codec 的 converter 支持（Get Parameter 0x0A）。
pub static SUPPORTED_FORMATS: [AudioFormat; 2] =
    [AudioFormat::S16_48K_STEREO, AudioFormat::S16_16K_MONO];

/// 控制器状态轮询上限。`stall(64)` 为一个自旋批次。
const CTRL_POLL_LIMIT: u32 = 1_000_000;
/// 复位后等待 codec 在 STATESTS 上报到齐的上限（spec 允许数百毫秒）。
const CODEC_POLL_LIMIT: u32 = 1_000_000;
/// 使用第一个**输出**流描述符。HDA 的 8 个流描述符里 0..3 是输入、4..7 是输出
/// （QEMU 的寄存器名 "IN0" 也印证了 SD0 属于输入方向），播放必须用 4..7 段。
pub const STREAM_INDEX: u8 = 4;
/// 默认音量。HDA amp 的 gain 是衰减量，具体换算见 `regs::volume_to_amp`。
const DEFAULT_VOLUME: (u8, u8) = (80, 80);

/// 通用 HDA 驱动。
///
/// `I` 是寄存器访问实现（真机 [`MmioHdaIo`]，测试用假寄存器），`L` 是日志槽，
/// `P` 是 codec 覆盖 profile。三者都是编译期泛型，驱动核心仍然零分配。
pub struct HdaDriver<I: HdaIo, L: HdaLog = SilentLog, P: CodecProfile = GenericProfile> {
    io: I,
    log: L,
    profile: P,
    pending: Option<HdaResources>,
    started: bool,
    layout: Option<DmaLayout>,
    codec: Option<CodecInfo>,
    stream: Option<PlaybackStream>,
    fmt: Option<AudioFormat>,
    volume: (u8, u8),
    stream_tag: u8,
}

/// 真机常用组合：`MmioHdaIo` + 默认通用 profile。
pub type MmioHdaDriver<L = SilentLog, P = GenericProfile> = HdaDriver<MmioHdaIo, L, P>;

impl<L: HdaLog> HdaDriver<MmioHdaIo, L, GenericProfile> {
    /// 直接用 BAR0 虚拟基址构造真机驱动。
    pub fn new_mmio(mmio_vaddr: u64, log: L) -> Self {
        Self::new(MmioHdaIo::new(mmio_vaddr), log, GenericProfile)
    }
}

impl<I: HdaIo, L: HdaLog, P: CodecProfile> HdaDriver<I, L, P> {
    pub fn new(io: I, log: L, profile: P) -> Self {
        Self {
            io,
            log,
            profile,
            pending: None,
            started: false,
            layout: None,
            codec: None,
            stream: None,
            fmt: None,
            volume: DEFAULT_VOLUME,
            stream_tag: 1,
        }
    }

    /// 记录待用的设备资源；之后 `Driver::start` 会消费它。
    pub fn set_resources(&mut self, resources: HdaResources) {
        self.pending = Some(resources);
    }

    /// `Driver::start` 的便捷版本：一次传入授权资源。
    pub fn start_with(
        &mut self,
        dev: &DeviceInfo,
        resources: HdaResources,
    ) -> Result<(), DriverError> {
        self.pending = Some(resources);
        Driver::start(self, dev)
    }

    /// 通用发现的结果（服务层日志 / 诊断用）。
    pub fn codec_info(&self) -> Option<CodecInfo> {
        self.codec
    }

    /// 当前打开的输出格式。
    pub fn format(&self) -> Option<AudioFormat> {
        self.fmt
    }

    /// 直接访问寄存器实现（单测用）。
    #[cfg(test)]
    pub(crate) fn io_mut(&mut self) -> &mut I {
        &mut self.io
    }

    fn init(&mut self, dev: &DeviceInfo, resources: HdaResources) -> Result<(), DriverError> {
        if !<Self as Driver>::probe(self, dev)? {
            return Err(DriverError::NotSupported);
        }
        if self.started {
            return Err(DriverError::BadState);
        }

        let layout = resources.layout()?;
        // 让寄存器层能把 CORB/RIRB 的物理地址翻译成授权 DMA 窗口里的虚拟地址
        // （立即命令不可用时的回退通路需要直接读写环内存）。
        self.io
            .set_dma_map(resources.dma_vaddr, resources.dma_paddr);

        self.controller_reset()?;

        // 复位后 codec 需要一段时间才在 STATESTS 上报；这里轮询等待，而不是只读一次。
        let mut states = self.io.rd16(STATESTS) & STATESTS_MASK;
        let mut spins = 0u32;
        while states == 0 && spins < CODEC_POLL_LIMIT {
            self.io.stall(64);
            spins += 1;
            states = self.io.rd16(STATESTS) & STATESTS_MASK;
        }
        // STATESTS 的每个 bit 对应一个 codec 地址；写回原值即"写 1 清除"。
        self.io.wr16(STATESTS, states);
        if states == 0 {
            self.log_str("hda: link has no codec");
            return Err(DriverError::NotFound);
        }

        // CORB/RIRB 初始化（基址、指针、DMA 使能）。verb 优先走立即命令，控制器不实现
        // 时由 `io::immediate_command` 自动回退到这两个环。
        self.program_rings(&layout)?;

        let mut chosen = None;
        for addr in 0..15u8 {
            if states & (1 << addr) == 0 {
                continue;
            }
            match codec::discover_codec(&mut self.io, addr, &self.profile) {
                Ok(info) => {
                    chosen = Some(info);
                    break;
                }
                Err(DriverError::Timeout) => {
                    self.log_str("hda: codec verb timeout (no immediate command / rings)");
                }
                Err(DriverError::NotSupported) => {
                    // profile 明确把该 codec 限定在别的地址，静默跳过。
                }
                Err(e) => {
                    let msg = match e {
                        DriverError::Other(s) => s,
                        _ => "hda: codec discovery failed",
                    };
                    self.log_str(msg);
                }
            }
        }
        let codec = chosen.ok_or(DriverError::NotFound)?;

        // 覆盖项按 codec 的真实 VID:DID 取（芯片表或用户 profile），而不是 PCI 设备 ID。
        let quirks = self.profile.quirks(codec.vendor_id, codec.device_id);
        codec::apply_extra_verbs(&mut self.io, codec.address, quirks.extra_verbs)?;
        self.stream_tag = if codec.stream_tag == 0 {
            1
        } else {
            codec.stream_tag
        };

        let mut line = LogLine::new();
        line.push_str("hda: codec=");
        line.push_dec(codec.address as u32);
        line.push_str(" afg=");
        line.push_dec(codec.afg_nid as u32);
        line.push_str(" dac=");
        line.push_dec(codec.dac_nid as u32);
        line.push_str(" pin=");
        line.push_dec(codec.pin_nid as u32);
        line.push_str(" name=");
        line.push_str(codec.name);
        let msg = line.as_str();
        self.log.log(msg);

        self.layout = Some(layout);
        self.codec = Some(codec);
        self.started = true;
        Ok(())
    }

    /// 复位控制器并清中断状态。
    fn controller_reset(&mut self) -> Result<(), DriverError> {
        let io = &mut self.io;

        io.wr32(INTCTL, 0);
        let intsts = io.rd32(INTSTS);
        if intsts != 0 {
            io.wr32(INTSTS, intsts);
        }

        // 停掉 SD0，确保复位后不会残留 DMA。
        let ctl = sd_reg(STREAM_INDEX, SD_CTL);
        io.wr8(ctl, 0);
        io.wr8(ctl + 1, 0);
        io.wr8(ctl + 2, 0);
        io.wr8(sd_reg(STREAM_INDEX, SD_STS), 0x1C);

        // CRST：写 0 进入复位，写 1 后读回 1 表示控制器就绪。
        let gctl = io.rd32(GCTL);
        io.wr32(GCTL, gctl & !GCTL_CRST);
        wait_bits(io, GCTL, GCTL_CRST, false)?;
        let gctl = io.rd32(GCTL);
        io.wr32(GCTL, gctl | GCTL_CRST);
        wait_bits(io, GCTL, GCTL_CRST, true)?;
        Ok(())
    }

    /// 初始化 CORB/RIRB 环。立即命令不可用时，codec verb 走这条规范通路。
    fn program_rings(&mut self, layout: &DmaLayout) -> Result<(), DriverError> {
        // 清环内存，避免 stale verb 被硬件读走。
        let mut off = 0u64;
        while off < layout.corb_len {
            libdevice::mmio::wr32(layout.corb_vaddr + off, 0);
            off += 4;
        }
        let mut off = 0u64;
        while off < layout.rirb_len {
            libdevice::mmio::wr32(layout.rirb_vaddr + off, 0);
            off += 4;
        }
        libdevice::mmio::fence();

        let io = &mut self.io;
        io.wr32(CORBLBASE, layout.corb_paddr as u32);
        io.wr32(CORBUBASE, (layout.corb_paddr >> 32) as u32);
        io.wr32(RIRBLBASE, layout.rirb_paddr as u32);
        io.wr32(RIRBUBASE, (layout.rirb_paddr >> 32) as u32);

        // 读/写指针复位（bit15 写 1，再清零）。
        io.wr16(CORBRP, RING_PTR_RESET);
        io.wr16(CORBRP, 0);
        io.wr16(CORBWP, 0);
        io.wr16(RIRBWP, RING_PTR_RESET);
        io.wr16(RIRBWP, 0);

        // CORBSIZE / RIRBSIZE 是**只读**的能力寄存器（控制器报告支持的环大小），
        // 软件不能设置；驱动按规范最大值预留缓冲，并按读回值推进指针。
        //
        // RINTCNT 必须显式写成非 0：它是「累计多少个响应后置中断/暂停取命令」的阈值，
        // 复位值 0 会让部分实现（QEMU `intel-hda`）认为配额已用完，CORB 一条都不取。
        // 这里写 1（每条命令都可能触发中断，但 INTCTL 未使能，实际不产生中断）。
        io.wr16(RINTCNT, 1);
        // CORBCTL **bit1 = CORBRUN**（bit0 是内存错误中断使能）；RIRBCTL bit1 = DMA_EN。
        // 两者都以 bit1 启动 DMA 通路；写成 0x1 只会开中断/错误上报，控制器一条命令都不取。
        // 同时开 RIRB 中断使能（bit0）：CORB 响应配额按 RINTCNT 计数，驱动取完一条就清
        // RIRBSTS 把配额归零，因此需要 IRQ 状态位真的被置起（INTCTL 仍为 0，不会真中断）。
        io.wr8(CORBCTL, 0x2);
        io.wr8(RIRBCTL, 0x3);
        io.fence();
        Ok(())
    }

    fn log_str(&mut self, msg: &str) {
        self.log.log(msg);
    }
}

/// 轮询 32 位寄存器直到 `mask` 位等于期望状态。
fn wait_bits<I: HdaIo>(io: &mut I, off: u32, mask: u32, want_set: bool) -> Result<(), DriverError> {
    let mut spins = 0u32;
    loop {
        let v = io.rd32(off);
        if ((v & mask) != 0) == want_set {
            return Ok(());
        }
        if spins >= CTRL_POLL_LIMIT {
            return Err(DriverError::Timeout);
        }
        io.stall(64);
        spins += 1;
    }
}

impl<I: HdaIo, L: HdaLog, P: CodecProfile> Driver for HdaDriver<I, L, P> {
    fn name(&self) -> &'static str {
        "hda"
    }

    fn class(&self) -> Class {
        Class::Audio
    }

    fn probe(&mut self, dev: &DeviceInfo) -> Result<bool, DriverError> {
        // PCI class 0x04（多媒体）/ subclass 0x03（HDA）。AMD 与 Intel 同规范，
        // 因此不按 VID:PID 限定；厂商需要更窄的匹配时用注册表条目覆盖。
        Ok(dev.matches_class(0x04, 0x03))
    }

    fn start(&mut self, dev: &DeviceInfo) -> Result<(), DriverError> {
        let resources = self.pending.ok_or(DriverError::BadState)?;
        self.init(dev, resources)
    }

    fn stop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            stream.stop(&mut self.io);
        }
        self.fmt = None;
        self.started = false;
        self.codec = None;
        self.layout = None;
        self.pending = None;
    }

    fn audio(&mut self) -> Option<&mut dyn AudioOps> {
        Some(self)
    }
}

impl<I: HdaIo, L: HdaLog, P: CodecProfile> AudioOps for HdaDriver<I, L, P> {
    fn name(&self) -> &'static str {
        "Intel HDA"
    }

    fn formats(&self) -> &'static [AudioFormat] {
        &SUPPORTED_FORMATS
    }

    fn open(&mut self, fmt: AudioFormat) -> Result<(), DriverError> {
        if !self.started {
            return Err(DriverError::BadState);
        }
        if self.fmt.is_some() {
            return Err(DriverError::BadState);
        }
        if !SUPPORTED_FORMATS.contains(&fmt) {
            return Err(DriverError::Invalid);
        }
        let format_word = crate::regs::format_word(fmt).ok_or(DriverError::Invalid)?;
        let codec = self.codec.ok_or(DriverError::BadState)?;
        let layout = self.layout.ok_or(DriverError::BadState)?;

        // 顺序：先让 codec 的 converter / pin 就绪，再启动流描述符。
        codec::configure_output(&mut self.io, &codec, fmt, format_word, self.volume)?;
        let mut stream = PlaybackStream::new(&layout, STREAM_INDEX);
        stream.start(&mut self.io, format_word, self.stream_tag)?;

        self.stream = Some(stream);
        self.fmt = Some(fmt);

        let mut line = LogLine::new();
        line.push_str("hda: open ch=");
        line.push_dec(fmt.channels as u32);
        line.push_str(" bits=");
        line.push_dec(fmt.bits as u32);
        line.push_str(" rate=");
        line.push_dec(fmt.rate);
        let msg = line.as_str();
        self.log.log(msg);
        Ok(())
    }

    fn write_pcm(&mut self, samples: &[i16]) -> Result<usize, DriverError> {
        let stream = self.stream.as_mut().ok_or(DriverError::BadState)?;
        stream.write(&mut self.io, samples)
    }

    fn drain(&mut self) -> Result<(), DriverError> {
        let stream = self.stream.as_mut().ok_or(DriverError::BadState)?;
        stream.drain(&mut self.io)
    }

    fn close(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            stream.stop(&mut self.io);
        }
        self.fmt = None;
        self.log_str("hda: close");
    }

    fn volume(&self) -> (u8, u8) {
        self.volume
    }

    fn set_volume(&mut self, left: u8, right: u8) -> Result<(), DriverError> {
        if !self.started {
            return Err(DriverError::BadState);
        }
        let l = left.min(100);
        let r = right.min(100);
        self.volume = (l, r);
        if let (Some(fmt), Some(codec)) = (self.fmt, self.codec) {
            codec::set_output_volume(&mut self.io, &codec, fmt.channels, (l, r))?;
        }
        Ok(())
    }
}
