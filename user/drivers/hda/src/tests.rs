//! 宿主机单测：用内存假寄存器 + 假 codec 驱动完整发现与播放路径。

use std::vec;
use std::vec::Vec;

use libdevmodel::{AudioFormat, AudioOps, DeviceInfo, Driver, DriverError};

use crate::codec::{self, CodecProfile, CodecQuirks, GenericProfile};
use crate::driver::HdaDriver;
use crate::io::{HdaIo, SilentLog};
use crate::regs::{
    format_word, sd_reg, verb12, verb4, BDLE_IOC, GCTL, GCTL_CRST, ICS, ICS_IRV, ICW, INTSTS, IRR,
    PARAM_CONN_LIST_LEN, PARAM_FUNCTION_TYPE, PARAM_NODE_COUNT, PARAM_PIN_CAP, PARAM_VENDOR_ID,
    PARAM_WIDGET_CAP, PIN_CTL_OUT_EN, SD_CBL, SD_FMT, SD_LPIB, STATESTS, VERB_GET_CONFIG_DEFAULT,
    VERB_GET_CONN_LIST, VERB_GET_PARAM, VERB_SET_AMP4, VERB_SET_FORMAT4,
};
use crate::resources::HdaResources;

// ============================================================
// 假 codec：NID 故意不用 QEMU 的布局（DAC 5 / mixer 6 / pin 7）
// ============================================================

struct FakeNode {
    nid: u8,
    widget: u8,
    conns: Vec<u8>,
    pin_caps: u16,
    config: u32,
}

struct FakeCodec {
    address: u8,
    vendor: u16,
    device: u16,
    afg: u8,
    nodes: Vec<FakeNode>,
}

impl FakeCodec {
    fn generic() -> Self {
        Self {
            address: 0,
            vendor: 0x10EC,
            device: 0x0250,
            afg: 1,
            nodes: vec![
                FakeNode {
                    nid: 5,
                    widget: 0,
                    conns: vec![],
                    pin_caps: 0,
                    config: 0,
                },
                FakeNode {
                    nid: 6,
                    widget: 2,
                    conns: vec![5],
                    pin_caps: 0,
                    config: 0,
                },
                FakeNode {
                    nid: 7,
                    widget: 4,
                    conns: vec![6],
                    pin_caps: 0x0110,
                    config: 0x0100_4000,
                },
            ],
        }
    }

    fn node(&self, nid: u8) -> Option<&FakeNode> {
        self.nodes.iter().find(|n| n.nid == nid)
    }

    fn param(&self, nid: u8, param: u8) -> u32 {
        match (nid, param) {
            (0, PARAM_VENDOR_ID) => ((self.vendor as u32) << 16) | self.device as u32,
            // root: 1 个 AFG，NID 1
            (0, PARAM_NODE_COUNT) => 1 | (1 << 8),
            (n, PARAM_FUNCTION_TYPE) if n == self.afg => 1,
            // AFG 子节点：5..7
            (n, PARAM_NODE_COUNT) if n == self.afg => 5 | (3 << 8),
            (n, PARAM_WIDGET_CAP) => self.node(n).map_or(0, |x| (x.widget as u32) << 20),
            (n, PARAM_PIN_CAP) => self.node(n).map_or(0, |x| x.pin_caps as u32),
            (n, PARAM_CONN_LIST_LEN) => self.node(n).map_or(0, |x| x.conns.len() as u32),
            _ => 0,
        }
    }

    fn response(&self, cmd: u32) -> u32 {
        if ((cmd >> 28) & 0xF) as u8 != self.address {
            return 0;
        }
        let nid = ((cmd >> 20) & 0xFF) as u8;
        let verb = ((cmd >> 8) & 0xFFF) as u16;
        match verb {
            VERB_GET_PARAM => self.param(nid, (cmd & 0xFF) as u8),
            VERB_GET_CONN_LIST => {
                let offset = (cmd & 0xFF) as usize;
                let mut resp = 0u32;
                if let Some(n) = self.node(nid) {
                    for i in 0..2 {
                        let v = n.conns.get(offset / 2 + i).copied().unwrap_or(0);
                        resp |= (v as u32) << (i * 16);
                    }
                }
                resp
            }
            VERB_GET_CONFIG_DEFAULT => self.node(nid).map_or(0, |n| n.config),
            _ => 0,
        }
    }
}

// ============================================================
// 假寄存器 / 立即命令 / LPIB
// ============================================================

struct FakeIo {
    regs: [u8; 0x300],
    codec: Option<FakeCodec>,
    icw: u32,
    irr: u32,
    irv: bool,
    lpib: u32,
    lpib_step: u32,
    cbl: u32,
    amp_writes: Vec<u16>,
    pin_ctl: u8,
}

impl FakeIo {
    fn new(codec: Option<FakeCodec>) -> Self {
        let mut regs = [0u8; 0x300];
        // 初始 CRST = 1，表示控制器处于可操作状态。
        regs[GCTL as usize..GCTL as usize + 4].copy_from_slice(&GCTL_CRST.to_le_bytes());
        if codec.is_some() {
            regs[STATESTS as usize..STATESTS as usize + 2].copy_from_slice(&1u16.to_le_bytes());
        }
        Self {
            regs,
            codec,
            icw: 0,
            irr: 0,
            irv: false,
            lpib: 0,
            lpib_step: 128,
            cbl: 0,
            amp_writes: Vec::new(),
            pin_ctl: 0,
        }
    }

    fn read32_reg(&self, off: u32) -> u32 {
        let i = off as usize;
        u32::from_le_bytes([
            self.regs[i],
            self.regs[i + 1],
            self.regs[i + 2],
            self.regs[i + 3],
        ])
    }

    fn write32_reg(&mut self, off: u32, v: u32) {
        let i = off as usize;
        self.regs[i..i + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn exec(&mut self) {
        self.irr = self.codec.as_ref().map_or(0, |c| c.response(self.icw));
        self.irv = true;

        // 记录 4 位 verb 0x3（Set Amp Gain/Mute）的 payload，供音量测试检查。
        if ((self.icw >> 16) & 0xF) as u8 == VERB_SET_AMP4 {
            self.amp_writes.push(self.icw as u16);
        }
        // 记录 pin widget control 的 payload。
        if ((self.icw >> 8) & 0xFFF) as u16 == crate::regs::VERB_SET_PIN_WIDGET_CONTROL {
            self.pin_ctl = self.icw as u8;
        }
    }
}

impl HdaIo for FakeIo {
    fn rd8(&mut self, off: u32) -> u8 {
        self.regs[off as usize]
    }

    fn rd16(&mut self, off: u32) -> u16 {
        let i = off as usize;
        u16::from_le_bytes([self.regs[i], self.regs[i + 1]])
    }

    fn rd32(&mut self, off: u32) -> u32 {
        if off == sd_reg(crate::driver::STREAM_INDEX, SD_LPIB) {
            return self.lpib;
        }
        if off == ICS {
            return (self.irv as u32) << 1;
        }
        if off == IRR {
            return self.irr;
        }
        self.read32_reg(off)
    }

    fn wr8(&mut self, off: u32, value: u8) {
        self.regs[off as usize] = value;
    }

    fn wr16(&mut self, off: u32, value: u16) {
        if off == STATESTS {
            // 写 1 清除。
            let old = self.rd16(off);
            let new = old & !value;
            let i = off as usize;
            self.regs[i..i + 2].copy_from_slice(&new.to_le_bytes());
            return;
        }
        if off == sd_reg(crate::driver::STREAM_INDEX, SD_CBL) {
            self.cbl = value as u32;
        }
        let i = off as usize;
        self.regs[i..i + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn wr32(&mut self, off: u32, value: u32) {
        if off == ICW {
            self.icw = value;
            self.exec();
            return;
        }
        if off == ICS {
            if value & ICS_IRV != 0 {
                self.irv = false;
            }
            return;
        }
        if off == INTSTS {
            self.write32_reg(off, 0);
            return;
        }
        if off == sd_reg(crate::driver::STREAM_INDEX, SD_CBL) {
            self.cbl = value;
        }
        self.write32_reg(off, value);
    }

    fn stall(&mut self, _units: u32) {
        if self.cbl > 0 {
            self.lpib = (self.lpib + self.lpib_step) % self.cbl;
        }
    }
}

#[repr(align(4096))]
struct Dma([u8; 8192]);

fn resources(dma: &mut Dma, dma_len: u64) -> HdaResources {
    HdaResources {
        mmio_vaddr: 0x1000_0000,
        mmio_len: 0x1000,
        dma_vaddr: dma.0.as_mut_ptr() as u64,
        dma_paddr: 0x2000_0000,
        dma_len,
    }
}

fn hda_dev() -> DeviceInfo {
    DeviceInfo::pci(0, 31, 3, 0x8086, 0x2668, 0x04, 0x03, 0, 0, 0)
}

fn boot(dma: &mut Dma, dma_len: u64) -> HdaDriver<FakeIo, SilentLog, GenericProfile> {
    let mut drv = HdaDriver::new(
        FakeIo::new(Some(FakeCodec::generic())),
        SilentLog,
        GenericProfile,
    );
    drv.start_with(&hda_dev(), resources(dma, dma_len))
        .expect("start");
    drv
}

// ============================================================
// 编码 / 布局
// ============================================================

#[test]
fn verb_and_format_encoding() {
    assert_eq!(verb12(0, 2, VERB_GET_PARAM, PARAM_NODE_COUNT), 0x002F_0004);
    assert_eq!(verb4(0, 2, VERB_SET_FORMAT4, 0x4011), 0x0022_4011);
    assert_eq!(verb12(3, 7, VERB_GET_PARAM, PARAM_VENDOR_ID), 0x307F_0000);

    assert_eq!(format_word(AudioFormat::S16_48K_STEREO), Some(0x0011));
    assert_eq!(format_word(AudioFormat::S16_16K_MONO), Some(0x0210));
    assert_eq!(
        format_word(AudioFormat {
            channels: 3,
            rate: 44_100,
            bits: 24
        }),
        Some(0x4032)
    );
    assert_eq!(
        format_word(AudioFormat {
            channels: 2,
            rate: 12_345,
            bits: 16
        }),
        None
    );
    assert_eq!(
        format_word(AudioFormat {
            channels: 2,
            rate: 48_000,
            bits: 8
        }),
        None
    );
}

#[test]
fn dma_layout_partitions_and_aligns() {
    let mut dma = Dma([0; 8192]);
    let res = resources(&mut dma, 8192);
    let l = res.layout().unwrap();

    assert_eq!(l.corb_len, 1024);
    assert_eq!(l.rirb_len, 2048);
    assert_eq!(l.bdl_entries, 2);
    assert_eq!(l.bdl_len, 32);
    assert_eq!(l.pcm_len, 4992);
    for addr in [
        l.corb_vaddr,
        l.rirb_vaddr,
        l.bdl_vaddr,
        l.pcm_vaddr,
        l.corb_paddr,
        l.rirb_paddr,
        l.bdl_paddr,
        l.pcm_paddr,
    ] {
        assert_eq!(addr % 128, 0, "DMA 区域必须 128 字节对齐");
    }
    assert!(l.corb_vaddr < l.rirb_vaddr);
    assert!(l.rirb_vaddr < l.bdl_vaddr);
    assert!(l.bdl_vaddr < l.pcm_vaddr);
}

#[test]
fn dma_layout_rejects_bad_grants() {
    let mut dma = Dma([0; 8192]);
    let mut res = resources(&mut dma, 1024);
    assert_eq!(res.layout(), Err(DriverError::Invalid));

    res.dma_len = 8192;
    res.dma_vaddr += 1;
    assert_eq!(res.layout(), Err(DriverError::Invalid));

    res.dma_vaddr -= 1;
    res.dma_paddr = 0;
    assert_eq!(res.layout(), Err(DriverError::Invalid));
}

// ============================================================
// 发现
// ============================================================

#[test]
fn generic_discovery_finds_dac_and_pin() {
    let mut dma = Dma([0; 8192]);
    let drv = boot(&mut dma, 8192);
    let c = drv.codec_info().unwrap();

    assert_eq!(c.address, 0);
    assert_eq!(c.vendor_id, 0x10EC);
    assert_eq!(c.device_id, 0x0250);
    assert_eq!(c.afg_nid, 1);
    assert_eq!(c.dac_nid, 5);
    assert_eq!(c.pin_nid, 7);
    assert_eq!(c.pin_control & PIN_CTL_OUT_EN, PIN_CTL_OUT_EN);
    assert!(c.eapd);
}

#[test]
fn no_codec_is_not_found() {
    let mut dma = Dma([0; 8192]);
    let mut drv = HdaDriver::new(FakeIo::new(None), SilentLog, GenericProfile);
    assert_eq!(
        drv.start_with(&hda_dev(), resources(&mut dma, 8192)),
        Err(DriverError::NotFound)
    );
}

#[test]
fn probe_only_claims_hda_class() {
    let mut drv = HdaDriver::new(FakeIo::new(None), SilentLog, GenericProfile);
    assert!(drv.probe(&hda_dev()).unwrap());

    let nic = DeviceInfo::pci(0, 25, 0, 0x8086, 0x1533, 0x02, 0x00, 0, 0, 0);
    assert!(!drv.probe(&nic).unwrap());
}

#[test]
fn profile_can_override_generic_choice() {
    struct ForceProfile;
    impl CodecProfile for ForceProfile {
        fn quirks(&self, _v: u16, _d: u16) -> CodecQuirks {
            CodecQuirks {
                dac_nid: Some(5),
                pin_nid: Some(7),
                stream_tag: 3,
                ..CodecQuirks::GENERIC
            }
        }
    }

    let mut dma = Dma([0; 8192]);
    let mut drv = HdaDriver::new(
        FakeIo::new(Some(FakeCodec::generic())),
        SilentLog,
        ForceProfile,
    );
    drv.start_with(&hda_dev(), resources(&mut dma, 8192))
        .unwrap();
    let c = drv.codec_info().unwrap();
    assert_eq!(c.dac_nid, 5);
    assert_eq!(c.pin_nid, 7);
    assert_eq!(c.stream_tag, 3);
}

// ============================================================
// 播放路径
// ============================================================

#[test]
fn open_rejects_unsupported_format_and_bad_state() {
    let mut dma = Dma([0; 8192]);
    let mut drv = boot(&mut dma, 8192);

    assert_eq!(
        drv.write_pcm(&[0i16; 4]),
        Err(DriverError::BadState),
        "未 open 不应能写"
    );
    assert_eq!(
        drv.open(AudioFormat {
            channels: 2,
            rate: 44_100,
            bits: 16
        }),
        Err(DriverError::Invalid)
    );
    drv.open(AudioFormat::S16_48K_STEREO).unwrap();
    assert_eq!(
        drv.open(AudioFormat::S16_48K_STEREO),
        Err(DriverError::BadState),
        "重复 open"
    );
}

#[test]
fn write_pcm_short_write_and_ring_advance() {
    let mut dma = Dma([0; 8192]);
    // 8192 字节 DMA → PCM 环 4992 字节 = 2496 个 i16 样本。
    let mut drv = boot(&mut dma, 8192);
    drv.open(AudioFormat::S16_48K_STEREO).unwrap();

    let samples = vec![0i16; 2600];
    assert_eq!(drv.write_pcm(&samples).unwrap(), 2496);
    // 硬件没推进时，环已满，只能短写 0。
    assert_eq!(drv.write_pcm(&samples).unwrap(), 0);

    // 模拟硬件播放了 128 字节（64 个样本）后，又可以写 64 个样本。
    drv.io_mut().lpib = 128;
    assert_eq!(drv.write_pcm(&samples).unwrap(), 64);
}

#[test]
fn drain_waits_until_hardware_catches_up() {
    let mut dma = Dma([0; 8192]);
    let mut drv = boot(&mut dma, 8192);
    drv.open(AudioFormat::S16_48K_STEREO).unwrap();

    let samples = vec![0i16; 576];
    assert_eq!(drv.write_pcm(&samples).unwrap(), 576);
    drv.drain().unwrap();

    // drain 完成后整个环都空出来，可再次写满。
    assert_eq!(drv.write_pcm(&samples).unwrap(), 576);
}

#[test]
fn bdl_layout_matches_pcm_ring() {
    let mut dma = Dma([0; 8192]);
    let mut drv = boot(&mut dma, 8192);
    let res = resources(&mut dma, 8192);
    let dma_vaddr = res.dma_vaddr;
    let l = res.layout().unwrap();
    drv.open(AudioFormat::S16_48K_STEREO).unwrap();

    let idx = (l.bdl_vaddr - dma_vaddr) as usize;
    let read = |off: usize| {
        u32::from_le_bytes([dma.0[off], dma.0[off + 1], dma.0[off + 2], dma.0[off + 3]])
    };

    assert_eq!(l.bdl_entries, 2);
    assert_eq!(
        read(idx) as u64 | ((read(idx + 4) as u64) << 32),
        l.pcm_paddr
    );
    assert_eq!(read(idx + 8), 4096);
    assert_eq!(read(idx + 12), 0);

    assert_eq!(
        read(idx + 16) as u64 | ((read(idx + 20) as u64) << 32),
        l.pcm_paddr + 4096
    );
    assert_eq!(read(idx + 24), 896);
    assert_eq!(read(idx + 28), BDLE_IOC);
}

#[test]
fn volume_is_clamped_and_written_to_amp() {
    let mut dma = Dma([0; 8192]);
    let mut drv = boot(&mut dma, 8192);
    drv.open(AudioFormat::S16_48K_STEREO).unwrap();

    drv.io_mut().amp_writes.clear();
    drv.set_volume(200, 30).unwrap();
    assert_eq!(drv.volume(), (100, 30));
    assert!(!drv.io_mut().amp_writes.is_empty());
}

#[test]
fn close_is_idempotent_and_allows_reopen() {
    let mut dma = Dma([0; 8192]);
    let mut drv = boot(&mut dma, 8192);

    drv.open(AudioFormat::S16_48K_STEREO).unwrap();
    drv.close();
    drv.close();
    assert!(drv.format().is_none());
    drv.open(AudioFormat::S16_16K_MONO).unwrap();
    assert_eq!(drv.format(), Some(AudioFormat::S16_16K_MONO));
}

#[test]
fn stop_resets_state() {
    let mut dma = Dma([0; 8192]);
    let mut drv = boot(&mut dma, 8192);
    drv.open(AudioFormat::S16_48K_STEREO).unwrap();
    drv.stop();
    assert!(drv.codec_info().is_none());
    assert_eq!(
        drv.open(AudioFormat::S16_48K_STEREO),
        Err(DriverError::BadState)
    );
    // 重复 stop 不 panic。
    drv.stop();
}

// ============================================================
// 直接验证 codec 模块的发现函数
// ============================================================

#[test]
fn codec_module_reports_pin_caps() {
    let mut io = FakeIo::new(Some(FakeCodec::generic()));
    let info = codec::discover_codec(&mut io, 0, &CodecQuirks::GENERIC).unwrap();
    assert_eq!(info.pin_caps, 0x0110);
    assert_eq!(info.pin_config, 0x0100_4000);

    let info = codec::discover_codec(
        &mut io,
        0,
        &CodecQuirks {
            pin_control: Some(0x40),
            eapd: Some(false),
            ..CodecQuirks::GENERIC
        },
    )
    .unwrap();
    assert_eq!(info.pin_control, 0x40);
    assert!(!info.eapd);
}

#[test]
fn immediate_command_transport() {
    use crate::io::immediate_command;

    let mut io = FakeIo::new(Some(FakeCodec::generic()));
    assert_eq!(
        immediate_command(&mut io, verb12(0, 0, VERB_GET_PARAM, PARAM_VENDOR_ID)).unwrap(),
        0x10EC_0250
    );
    assert_eq!(
        immediate_command(&mut io, verb12(0, 0, VERB_GET_PARAM, PARAM_NODE_COUNT)).unwrap(),
        0x0101
    );
    assert_eq!(
        immediate_command(&mut io, verb12(0, 1, VERB_GET_PARAM, PARAM_FUNCTION_TYPE)).unwrap(),
        1
    );
    assert_eq!(
        immediate_command(&mut io, verb12(0, 1, VERB_GET_PARAM, PARAM_NODE_COUNT)).unwrap(),
        0x0305
    );
    assert_eq!(
        immediate_command(&mut io, verb12(0, 5, VERB_GET_PARAM, PARAM_WIDGET_CAP)).unwrap(),
        0
    );
    assert_eq!(
        immediate_command(&mut io, verb12(0, 7, VERB_GET_PARAM, PARAM_WIDGET_CAP)).unwrap(),
        4 << 20
    );
    assert_eq!(
        immediate_command(&mut io, verb12(0, 7, VERB_GET_CONN_LIST, 0)).unwrap(),
        6
    );
    assert_eq!(
        immediate_command(&mut io, verb12(0, 7, VERB_GET_CONFIG_DEFAULT, 0)).unwrap(),
        0x0100_4000
    );
}

// 让测试编译时引用 `SD_FMT` 等常量，避免导入被优化器提示未使用（行为本身在驱动里）。
#[test]
fn stream_registers_are_covered() {
    assert_eq!(sd_reg(0, SD_CBL), 0x88);
    assert_eq!(sd_reg(0, SD_FMT), 0x92);
}
