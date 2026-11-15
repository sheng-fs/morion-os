//! 通用 codec 发现与输出路径配置。
//!
//! HDA 控制器本身只负责搬运 PCM；真正决定"声音从哪个插孔出来"的是 codec 里的
//! widget 树：root（NID 0）→ Audio Function Group（AFG）→ Audio Output Converter
//! （DAC）→ … → Pin Complex。不同厂商的 NID 完全不同，因此这里**不硬编码任何 NID**，
//! 而是用 Get Parameter / Get Connection List 现场遍历。
//!
//! 厂商有自己的封装/怪癖时，不需要改本文件：实现 [`CodecProfile`] 返回
//! [`CodecQuirks`]（覆盖 DAC/Pin、pin control、额外 verb 序列等）即可。

use libdevmodel::{AudioFormat, DriverError};

use crate::io::{immediate_command, HdaIo};
use crate::regs::{
    amp_payload, config_default_device, node_count, verb12, verb4, volume_to_amp,
    PARAM_CONN_LIST_LEN, PARAM_FUNCTION_TYPE, PARAM_NODE_COUNT, PARAM_PIN_CAP, PARAM_VENDOR_ID,
    PARAM_WIDGET_CAP, PIN_CAP_EAPD, PIN_CAP_OUTPUT, PIN_CTL_HP_EN, PIN_CTL_OUT_EN,
    VERB_GET_CONFIG_DEFAULT, VERB_GET_CONN_LIST, VERB_GET_PARAM, VERB_SET_AMP4,
    VERB_SET_CHANNEL_STREAMID, VERB_SET_EAPD_BTLENABLE, VERB_SET_FORMAT4,
    VERB_SET_PIN_WIDGET_CONTROL, VERB_SET_POWER_STATE, WIDGET_AUDIO_OUTPUT, WIDGET_PIN_COMPLEX,
};

/// 一次枚举最多记录多少个同类 widget（DAC / Pin）。
const MAX_WIDGETS: usize = 32;
/// 单次连接列表读取最多保留多少项。
const MAX_CONNECTIONS: usize = 16;
/// 连接图深度上限，防止坏 codec 造成无限递归。
const MAX_DEPTH: u8 = 6;

// ============================================================
// 可替换点：codec 配置覆盖
// ============================================================

/// 厂商对通用发现的覆盖项。默认值 [`CodecQuirks::GENERIC`] 表示"完全通用发现"。
#[derive(Clone, Copy, Debug)]
pub struct CodecQuirks {
    /// 只在该 codec 地址上尝试（None = 遍历 STATESTS 的所有地址）。
    pub codec_address: Option<u8>,
    /// 强制使用的 DAC NID（None = 自动挑第一个 Audio Output Converter）。
    pub dac_nid: Option<u8>,
    /// 强制使用的输出 Pin NID（None = 自动按可达性 + 默认设备类型打分）。
    pub pin_nid: Option<u8>,
    /// 覆盖 SET_PIN_WIDGET_CONTROL 的 payload。
    pub pin_control: Option<u8>,
    /// 是否发送 EAPD 使能（None = pin 支持就发）。
    pub eapd: Option<bool>,
    /// 在发现完成后追加发送的 verb 序列。**不含 codec 地址**（高 4 位为 0）；
    /// 驱动会按实际 codec 地址补齐 `[31:28]`。
    pub extra_verbs: &'static [u32],
    /// converter / 流描述符共用的 stream tag（1..=15）。
    pub stream_tag: u8,
}

impl CodecQuirks {
    /// 完全通用的默认配置。
    pub const GENERIC: Self = Self {
        codec_address: None,
        dac_nid: None,
        pin_nid: None,
        pin_control: None,
        eapd: None,
        extra_verbs: &[],
        stream_tag: 1,
    };
}

/// 厂商 codec 配置槽。默认实现返回通用配置，不改变通用发现逻辑。
pub trait CodecProfile {
    /// 根据 codec 的 VID:DID 返回覆盖项。
    fn quirks(&self, vendor_id: u16, device_id: u16) -> CodecQuirks {
        let _ = (vendor_id, device_id);
        CodecQuirks::GENERIC
    }
}

/// 默认 profile：按 codec 的 VID:DID 查内置芯片表（`chips` 模块）。
///
/// 表里没有的 codec 返回 [`CodecQuirks::GENERIC`]，通用发现照常工作。
#[derive(Clone, Copy, Debug, Default)]
pub struct GenericProfile;

impl CodecProfile for GenericProfile {
    fn quirks(&self, vendor_id: u16, device_id: u16) -> CodecQuirks {
        crate::chips::codec_quirks(vendor_id, device_id)
    }
}

/// 直接拿一份固定 quirks 当 profile 用（测试/单型号定制）。
impl CodecProfile for CodecQuirks {
    fn quirks(&self, _vendor_id: u16, _device_id: u16) -> CodecQuirks {
        *self
    }
}

/// 让 `&'static dyn CodecProfile` 也能直接作为 profile 传入（运行期选择厂商 profile）。
impl CodecProfile for &'static dyn CodecProfile {
    fn quirks(&self, vendor_id: u16, device_id: u16) -> CodecQuirks {
        (**self).quirks(vendor_id, device_id)
    }
}

// ============================================================
// 发现结果
// ============================================================

/// 通用发现得到的 codec 摘要，也是服务层诊断日志/`/proc` 的数据源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecInfo {
    /// codec 地址（0..=14）。
    pub address: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    /// 芯片可读名字（来自 `chips` 表；未知 codec 为通用描述）。
    pub name: &'static str,
    /// Audio Function Group 的 NID。
    pub afg_nid: u8,
    /// 选中的输出 converter（DAC）NID。
    pub dac_nid: u8,
    /// 选中的输出 Pin Complex NID。
    pub pin_nid: u8,
    /// Pin Capabilities 原始值。
    pub pin_caps: u16,
    /// Config Default 原始值。
    pub pin_config: u32,
    /// 实际发送给 pin 的 SET_PIN_WIDGET_CONTROL payload。
    pub pin_control: u8,
    /// 是否发送 EAPD 使能。
    pub eapd: bool,
    /// 本 codec 使用的 stream tag。
    pub stream_tag: u8,
}

/// 发现一个 codec 的输出路径。
///
/// 失败语义：
/// - 没有 Audio Function Group / DAC / 输出 pin → [`DriverError::Other`]（带具体阶段）；
/// - verb 一直无响应 → [`DriverError::Timeout`]；
/// - profile 把该 codec 限定在别的地址 → [`DriverError::NotSupported`]（调用方跳过）。
pub fn discover_codec<I: HdaIo>(
    io: &mut I,
    address: u8,
    profile: &dyn CodecProfile,
) -> Result<CodecInfo, DriverError> {
    let id = get_param(io, address, 0, PARAM_VENDOR_ID)?;
    // Get Parameter(VENDOR_ID) 的布局：高 16 位 = Vendor ID，低 16 位 = Device ID
    // （QEMU/Realtek 等均如此；写反会把 0x10EC0256 认成厂商 0x0256）。
    let vendor_id = ((id >> 16) & 0xFFFF) as u16;
    let device_id = (id & 0xFFFF) as u16;

    // codec 身份已知，才谈得上"厂商覆盖项"：芯片表/用户 profile 都按 VID:DID 选。
    let quirks = profile.quirks(vendor_id, device_id);
    if let Some(want) = quirks.codec_address {
        if want != address {
            return Err(DriverError::NotSupported);
        }
    }
    let name = crate::chips::codec_name(vendor_id, device_id);

    // ---- root → AFG ----
    let (root_start, root_count) = node_count(get_param(io, address, 0, PARAM_NODE_COUNT)?);
    let mut afg_nid = None;
    // root 的子节点通常只有 1..2 个 function group；扫描到 node count 为止，
    // 不用硬编码 NID 1。
    for i in 0..root_count {
        let nid = root_start.wrapping_add(i);
        let ty = get_param(io, address, nid, PARAM_FUNCTION_TYPE)?;
        if ty & 0x01 != 0 {
            afg_nid = Some(nid);
            break;
        }
    }
    // 兜底：部分 codec（QEMU `hda-duplex` 即如此）不在 NID 0 报 subordinate count，
    // root_count 为 0。直接探测 NID 1..=15 的 Function Group Type —— 只读、无副作用，
    // 真机上 root_count 正常时这条路径不会走到。
    if afg_nid.is_none() {
        for nid in 1..=15u8 {
            if let Ok(ty) = get_param(io, address, nid, PARAM_FUNCTION_TYPE) {
                if ty & 0x01 != 0 {
                    afg_nid = Some(nid);
                    break;
                }
            }
        }
    }
    let afg_nid = afg_nid.ok_or(DriverError::Other("hda: codec has no AFG"))?;

    // ---- AFG → widgets ----
    let (wstart, wcount) = node_count(get_param(io, address, afg_nid, PARAM_NODE_COUNT)?);
    let mut dacs = [0u8; MAX_WIDGETS];
    let mut dac_count = 0usize;
    let mut pins = [0u8; MAX_WIDGETS];
    let mut pin_count = 0usize;

    // 扫描 AFG 的全部子节点（真实 codec 常见 20~40 个 widget）。只把 DAC / Pin
    // 存进固定数组；selector / mixer 在 DFS 时按需读连接列表。
    for i in 0..wcount {
        let nid = wstart.wrapping_add(i);
        let caps = get_param(io, address, nid, PARAM_WIDGET_CAP)?;
        // Widget Type 在 **bits 23:20**（`AC_WCAP_TYPE = 0xf << 20`），不在低 4 位。
        let ty = ((caps >> 20) & 0x0F) as u8;
        if ty == WIDGET_AUDIO_OUTPUT && dac_count < MAX_WIDGETS {
            dacs[dac_count] = nid;
            dac_count += 1;
        } else if ty == WIDGET_PIN_COMPLEX && pin_count < MAX_WIDGETS {
            pins[pin_count] = nid;
            pin_count += 1;
        }
    }

    // 兜底：AFG 的 subordinate count 也为 0 时（少数简单 codec），直接扫一段 NID。
    // 只读 widget capabilities，不会改动设备状态。
    if dac_count == 0 && pin_count == 0 {
        for nid in 1..=32u8 {
            let Ok(caps) = get_param(io, address, nid, PARAM_WIDGET_CAP) else {
                continue;
            };
            let ty = ((caps >> 20) & 0x0F) as u8;
            if ty == WIDGET_AUDIO_OUTPUT && dac_count < MAX_WIDGETS {
                dacs[dac_count] = nid;
                dac_count += 1;
            } else if ty == WIDGET_PIN_COMPLEX && pin_count < MAX_WIDGETS {
                pins[pin_count] = nid;
                pin_count += 1;
            }
        }
    }

    if dac_count == 0 && quirks.dac_nid.is_none() {
        return Err(DriverError::Other("hda: codec has no DAC"));
    }

    // ---- Pin + DAC 配对 ----
    // 一个 codec 可能有多个 DAC；先按 pin 打分，再取该 pin 真正可达的 DAC。
    let mut chosen_dac = quirks.dac_nid.unwrap_or(dacs[0]);
    let (pin_nid, pin_caps, pin_config) = if let Some(pin) = quirks.pin_nid {
        let caps = get_param(io, address, pin, PARAM_PIN_CAP)? as u16;
        let cfg = get_config_default(io, address, pin)?;
        if quirks.dac_nid.is_none() {
            chosen_dac = first_reachable_dac(io, address, pin, &dacs[..dac_count], chosen_dac)?;
        }
        (pin, caps, cfg)
    } else {
        // 严格模式只接受「Pin Capabilities 报告输出能力」的 pin；找不到时放宽一次 ——
        // 少数 codec（含 QEMU `hda-duplex`）不报 `AC_PINCAP_OUT`，但 pin 实际可输出。
        let mut best = pick_pin(io, address, &pins[..pin_count], &dacs[..dac_count], quirks, true)?;
        if best.is_none() {
            best = pick_pin(io, address, &pins[..pin_count], &dacs[..dac_count], quirks, false)?;
        }
        match best {
            Some((_, pin, caps, cfg, dac)) => {
                chosen_dac = dac;
                (pin, caps, cfg)
            }
            None => return Err(DriverError::Other("hda: codec has no output pin")),
        }
    };

    let default_ctl = {
        let mut ctl = PIN_CTL_OUT_EN;
        // 默认设备字段 0x2 = 耳机，额外打开耳机驱动。
        if config_default_device(pin_config) == 0x2 {
            ctl |= PIN_CTL_HP_EN;
        }
        ctl
    };

    Ok(CodecInfo {
        address,
        vendor_id,
        device_id,
        name,
        afg_nid,
        dac_nid: chosen_dac,
        pin_nid,
        pin_caps,
        pin_config,
        pin_control: quirks.pin_control.unwrap_or(default_ctl),
        eapd: quirks.eapd.unwrap_or(pin_caps & PIN_CAP_EAPD != 0),
        stream_tag: {
            let tag = quirks.stream_tag & 0x0F;
            // stream tag 0 在 HDA 里是合法编码，但本驱动用立即命令/单流，约定 0 表示"用默认 1"。
            if tag == 0 {
                1
            } else {
                tag
            }
        },
    })
}

/// pin 选择结果：`(score, pin NID, pin caps, pin config, DAC NID)`。
type PinChoice = (u32, u8, u16, u32, u8);

/// 在候选 pin 中挑一个：按「能否到达 DAC」+ Config Default 设备类型打分。
///
/// `require_output = true` 时只考虑 Pin Capabilities 报输出能力的 pin；放宽模式用于
/// 少数不报该位但仍可输出的 codec。返回 `(score, pin, pin_caps, pin_config, dac)`。
fn pick_pin<I: HdaIo>(
    io: &mut I,
    address: u8,
    pins: &[u8],
    dacs: &[u8],
    quirks: CodecQuirks,
    require_output: bool,
) -> Result<Option<PinChoice>, DriverError> {
    let mut best: Option<PinChoice> = None;
    for &pin in pins {
        let caps = get_param(io, address, pin, PARAM_PIN_CAP)? as u16;
        if require_output && caps & PIN_CAP_OUTPUT == 0 {
            continue;
        }
        let cfg = get_config_default(io, address, pin)?;

        let (reachable, dac) = if let Some(target) = quirks.dac_nid {
            let mut visited = [0u8; MAX_CONNECTIONS];
            let mut visited_len = 0usize;
            (
                path_to_dac(io, address, pin, target, 0, &mut visited, &mut visited_len)?,
                target,
            )
        } else {
            let mut found = None;
            for &d in dacs {
                let mut visited = [0u8; MAX_CONNECTIONS];
                let mut visited_len = 0usize;
                if path_to_dac(io, address, pin, d, 0, &mut visited, &mut visited_len)? {
                    found = Some(d);
                    break;
                }
            }
            (found.is_some(), found.unwrap_or(dacs[0]))
        };

        let score = pin_score(cfg, reachable);
        if best.is_none_or(|(s, ..)| score > s) {
            best = Some((score, pin, caps, cfg, dac));
        }
    }
    Ok(best)
}

/// 选中的输出路径下发到 codec：DAC 格式、stream tag、pin 输出使能 / EAPD / amp。
pub fn configure_output<I: HdaIo>(
    io: &mut I,
    codec: &CodecInfo,
    fmt: AudioFormat,
    format: u16,
    volume: (u8, u8),
) -> Result<(), DriverError> {
    // 工作状态切到 D0（有些 codec 复位后默认是 D3）。
    immediate_command(
        io,
        verb12(codec.address, codec.dac_nid, VERB_SET_POWER_STATE, 0),
    )?;
    immediate_command(
        io,
        verb12(codec.address, codec.pin_nid, VERB_SET_POWER_STATE, 0),
    )?;

    // 4 位 verb + 16 位格式字。
    immediate_command(
        io,
        verb4(codec.address, codec.dac_nid, VERB_SET_FORMAT4, format),
    )?;
    // stream tag 在 payload 高 4 位，channel 低 4 位（PCM 用 0）。
    immediate_command(
        io,
        verb12(
            codec.address,
            codec.dac_nid,
            VERB_SET_CHANNEL_STREAMID,
            (codec.stream_tag & 0x0F) << 4,
        ),
    )?;

    set_output_volume(io, codec, fmt.channels, volume)?;

    // Pin 的 output amp 解除静音（0 dB）；没有 output amp 的 codec 会忽略该 verb。
    // 立体声时逐个通道发送，避免右声道仍停在硬件默认的静音状态。
    for ch in 0..fmt.channels.clamp(1, 2) {
        immediate_command(
            io,
            verb4(
                codec.address,
                codec.pin_nid,
                VERB_SET_AMP4,
                amp_payload(true, ch, 0, false),
            ),
        )?;
    }

    // 打开输出（必要时也打开耳机驱动）。
    immediate_command(
        io,
        verb12(
            codec.address,
            codec.pin_nid,
            VERB_SET_PIN_WIDGET_CONTROL,
            codec.pin_control,
        ),
    )?;

    if codec.eapd {
        // EAPD bit1：外部功放使能。
        immediate_command(
            io,
            verb12(codec.address, codec.pin_nid, VERB_SET_EAPD_BTLENABLE, 0x02),
        )?;
    }
    Ok(())
}

/// 把左右声道音量写到 DAC 的 output amp。`channels` 为 1 时只写 index 0。
pub fn set_output_volume<I: HdaIo>(
    io: &mut I,
    codec: &CodecInfo,
    channels: u8,
    volume: (u8, u8),
) -> Result<(), DriverError> {
    let (lg, lm) = volume_to_amp(volume.0);
    let (rg, rm) = volume_to_amp(volume.1);
    let n = channels.clamp(1, 2);
    for ch in 0..n {
        let (gain, mute) = if ch == 1 { (rg, rm) } else { (lg, lm) };
        let payload = amp_payload(true, ch, gain, mute);
        immediate_command(
            io,
            verb4(codec.address, codec.dac_nid, VERB_SET_AMP4, payload),
        )?;
    }
    Ok(())
}

/// 发送 profile 提供的额外 verb（高 4 位 codec 地址由这里补齐）。
pub fn apply_extra_verbs<I: HdaIo>(
    io: &mut I,
    address: u8,
    verbs: &[u32],
) -> Result<(), DriverError> {
    for v in verbs {
        immediate_command(io, (*v & 0x0FFF_FFFF) | ((address as u32) << 28))?;
    }
    Ok(())
}

// ============================================================
// verb 级辅助
// ============================================================

fn get_param<I: HdaIo>(io: &mut I, codec: u8, nid: u8, param: u8) -> Result<u32, DriverError> {
    immediate_command(io, verb12(codec, nid, VERB_GET_PARAM, param))
}

fn get_config_default<I: HdaIo>(io: &mut I, codec: u8, nid: u8) -> Result<u32, DriverError> {
    immediate_command(io, verb12(codec, nid, VERB_GET_CONFIG_DEFAULT, 0))
}

/// 读取一个节点的连接列表。
///
/// 每次 Get Connection List 返回 2 个 16 位条目；长格式（bit7 = 1）时两个条目
/// 表示一个 NID 范围（start, end）。这里统一展开成 NID 列表。
fn read_connections<I: HdaIo>(
    io: &mut I,
    codec: u8,
    nid: u8,
    out: &mut [u8; MAX_CONNECTIONS],
) -> Result<usize, DriverError> {
    let len = get_param(io, codec, nid, PARAM_CONN_LIST_LEN)?;
    let count = (len & 0x7F) as usize;
    let long_form = len & 0x80 != 0;
    let mut n = 0usize;
    let mut base = 0usize;

    while base < count && n < out.len() {
        let resp = immediate_command(io, verb12(codec, nid, VERB_GET_CONN_LIST, base as u8))?;
        let e0 = (resp & 0xFFFF) as u16;
        let e1 = ((resp >> 16) & 0xFFFF) as u16;
        if long_form {
            // 长格式必须成对出现；跨响应的范围这里直接放弃，实际 codec 很少见。
            if base + 1 >= count {
                break;
            }
            let start = (e0 & 0x7FFF) as u8;
            let end = (e1 & 0x7FFF) as u8;
            let mut x = start;
            loop {
                if n >= out.len() {
                    break;
                }
                out[n] = x;
                n += 1;
                if x >= end {
                    break;
                }
                x = x.wrapping_add(1);
            }
        } else {
            if base < count && n < out.len() {
                out[n] = (e0 & 0xFF) as u8;
                n += 1;
            }
            if base + 1 < count && n < out.len() {
                out[n] = (e1 & 0xFF) as u8;
                n += 1;
            }
        }
        base += 2;
    }
    Ok(n)
}

/// 在 `dacs` 中找第一个与 `pin` 连通的 DAC；都不可达时返回 `fallback`。
fn first_reachable_dac<I: HdaIo>(
    io: &mut I,
    codec: u8,
    pin: u8,
    dacs: &[u8],
    fallback: u8,
) -> Result<u8, DriverError> {
    for &d in dacs {
        let mut visited = [0u8; MAX_CONNECTIONS];
        let mut visited_len = 0usize;
        if path_to_dac(io, codec, pin, d, 0, &mut visited, &mut visited_len)? {
            return Ok(d);
        }
    }
    Ok(fallback)
}

/// 从 `nid` 沿连接图找 `dac`。只走输入方向（pin → selector/mixer → DAC）。
fn path_to_dac<I: HdaIo>(
    io: &mut I,
    codec: u8,
    nid: u8,
    dac: u8,
    depth: u8,
    visited: &mut [u8; MAX_CONNECTIONS],
    visited_len: &mut usize,
) -> Result<bool, DriverError> {
    if nid == dac {
        return Ok(true);
    }
    if depth >= MAX_DEPTH || *visited_len >= visited.len() {
        return Ok(false);
    }
    if visited[..*visited_len].contains(&nid) {
        return Ok(false);
    }
    visited[*visited_len] = nid;
    *visited_len += 1;

    let mut conns = [0u8; MAX_CONNECTIONS];
    let n = read_connections(io, codec, nid, &mut conns)?;
    let mut found = false;
    for &conn in conns.iter().take(n) {
        if path_to_dac(io, codec, conn, dac, depth + 1, visited, visited_len)? {
            found = true;
            break;
        }
    }
    *visited_len -= 1;
    Ok(found)
}

/// pin 打分：可达 DAC 权重最高，其次看 Config Default 的默认设备类型。
fn pin_score(config: u32, reachable: bool) -> u32 {
    let dev_score = match config_default_device(config) {
        0x0 => 40, // Line Out
        0x1 => 36, // Speaker
        0x2 => 32, // Headphone
        0x3 => 24, // CD
        _ => 8,
    };
    dev_score + if reachable { 100 } else { 0 }
}
