//! HDA 寄存器偏移、verb 编码、流格式编码与位域常量。
//!
//! 这些常量来自 Intel High Definition Audio Specification（AMD 平台同规范）。
//! 所有偏移都是相对 MMIO BAR0 基址的**字节偏移**；寄存器宽度不统一，读寄存器时
//! 必须按 spec 指定宽度访问（例如 STATESTS 是 16 位，GCTL 是 32 位）。

use libdevmodel::AudioFormat;

// ============================================================
// 控制器寄存器（BAR0）
// ============================================================

/// Global Capabilities（32 位）。
pub const GCAP: u32 = 0x00;
/// Global Control（32 位）。bit0 = CRST。
pub const GCTL: u32 = 0x08;
/// Wake Enable（16 位）。
pub const WAKEEN: u32 = 0x0C;
/// State Change Status（16 位，写 1 清除）。bit0..14 对应 codec 地址 0..14。
pub const STATESTS: u32 = 0x0E;
/// Global Status（16 位）。
pub const GSTS: u32 = 0x10;
/// Interrupt Control（32 位）。
pub const INTCTL: u32 = 0x20;
/// Interrupt Status（32 位，写 1 清除）。
pub const INTSTS: u32 = 0x24;
/// Wall Clock Counter（32 位）。
pub const WALCLK: u32 = 0x30;
/// Stream Synchronization（32 位）。
pub const SSYNC: u32 = 0x38;

/// CORB 低 32 位基址。
pub const CORBLBASE: u32 = 0x40;
/// CORB 高 32 位基址。
pub const CORBUBASE: u32 = 0x44;
/// CORB 写指针（16 位，bit15 = 复位读指针）。
pub const CORBWP: u32 = 0x48;
/// CORB 读指针（16 位，bit15 = 复位读指针）。
pub const CORBRP: u32 = 0x4A;
/// CORB 控制（8 位）。
pub const CORBCTL: u32 = 0x4C;
/// CORB 状态（8 位，写 1 清除）。
pub const CORBSTS: u32 = 0x4D;
/// CORB 大小（8 位）。
pub const CORBSIZE: u32 = 0x4E;

/// RIRB 低 32 位基址。
pub const RIRBLBASE: u32 = 0x50;
/// RIRB 高 32 位基址。
pub const RIRBUBASE: u32 = 0x54;
/// RIRB 写指针（16 位，bit15 = 复位写指针）。
pub const RIRBWP: u32 = 0x58;
/// RIRB 响应中断计数（16 位）。
pub const RINTCNT: u32 = 0x5A;
/// RIRB 控制（8 位）。
pub const RIRBCTL: u32 = 0x5C;
/// RIRB 状态（8 位，写 1 清除）。
pub const RIRBSTS: u32 = 0x5D;
/// RIRB 大小（8 位）。
pub const RIRBSIZE: u32 = 0x5E;

/// Immediate Command Write（32 位）：写入一条 verb。
pub const ICW: u32 = 0x60;
/// Immediate Response Register（32 位）：读回上一条立即命令的响应。
pub const IRR: u32 = 0x64;
/// Immediate Command Status（32 位）。bit0 = ICB（忙），bit1 = IRV（响应有效，写 1 清除）。
pub const ICS: u32 = 0x68;

/// 控制器复位位：写 0 进入复位，读回 1 表示复位完成 / 可操作。
pub const GCTL_CRST: u32 = 1 << 0;

/// STATESTS 中有效的 codec 地址位（0..=14）。
pub const STATESTS_MASK: u16 = 0x7FFF;

/// ICS.ICB：立即命令仍在执行。
pub const ICS_ICB: u32 = 1 << 0;
/// ICS.IRV：立即命令响应有效；写 1 清除。
pub const ICS_IRV: u32 = 1 << 1;

/// 用来"复位" CORB/RIRB 指针的两个 bit（读/写指针复位）。
pub const RING_PTR_RESET: u16 = 0x8000;

// ============================================================
// 流描述符（SDn，n = 0..15，步长 0x20）
// ============================================================

/// 第一个流描述符的偏移。
pub const SD_BASE: u32 = 0x80;
/// 每个流描述符的步长。
pub const SD_STRIDE: u32 = 0x20;

/// SDnCTL（24 位）。
pub const SD_CTL: u32 = 0x00;
/// SDnSTS（8 位，写 1 清除错误位）。
pub const SD_STS: u32 = 0x03;
/// SDnLPIB（32 位）：当前播放到的字节位置（在 CBL 内回绕）。
pub const SD_LPIB: u32 = 0x04;
/// SDnCBL（32 位）：循环缓冲区长度。
pub const SD_CBL: u32 = 0x08;
/// SDnLVI（16 位）：最后一个有效 BDL 条目下标。
pub const SD_LVI: u32 = 0x0C;
/// SDnFIFOW（16 位）。
pub const SD_FIFOW: u32 = 0x0E;
/// SDnFIFOS（16 位）。
pub const SD_FIFOS: u32 = 0x10;
/// SDnFMT（16 位）：本流的样本格式。
pub const SD_FMT: u32 = 0x12;
/// SDnBDPL（32 位）：BDL 低 32 位物理地址（0x14 是保留位，BDL 从 0x18 开始）。
pub const SD_BDPL: u32 = 0x18;
/// SDnBDPU（32 位）：BDL 高 32 位物理地址。
pub const SD_BDPU: u32 = 0x1C;

/// SDnCTL.SRST：流复位（bit0）。
pub const SD_CTL_RESET: u32 = 1 << 0;
/// SDnCTL.RUN：DMA 引擎运行（bit1）。
pub const SD_CTL_RUN: u32 = 1 << 1;
/// SDnCTL.IOCE：完成中断使能（bit2，本驱动只用轮询，故不置位）。
pub const SD_CTL_IOCE: u32 = 1 << 2;
/// SDnCTL 中 stream tag 的位偏移。
pub const SD_CTL_TAG_SHIFT: u32 = 20;
/// SDnCTL 中 stream number 的位偏移。
pub const SD_CTL_ID_SHIFT: u32 = 16;

/// 计算第 `stream` 个描述符中 `off` 寄存器的绝对 MMIO 偏移。
pub const fn sd_reg(stream: u8, off: u32) -> u32 {
    SD_BASE + (stream as u32) * SD_STRIDE + off
}

// ============================================================
// BDL（Buffer Descriptor List）
// ============================================================

/// 一个 BDLE 占 16 字节：64 位地址 + 32 位长度 + 32 位标志。
pub const BDLE_BYTES: u32 = 16;
/// 单个 BDLE 最大缓冲长度（spec 上限 4 KiB）。
pub const BDLE_MAX_BYTES: u32 = 4096;
/// BDLE 标志：IOC（Interrupt on Completion）。最后一条置位。
pub const BDLE_IOC: u32 = 1 << 0;

// ============================================================
// Codec verb / parameter
// ============================================================

/// Get Parameter（12 位 verb，8 位 payload = parameter id）。
pub const VERB_GET_PARAM: u16 = 0xF00;
/// Get Connection List（12 位 verb，payload = 起始条目下标 0/4/8…）。
pub const VERB_GET_CONN_LIST: u16 = 0xF02;
/// Get Config Default（12 位 verb，payload = 0）。
pub const VERB_GET_CONFIG_DEFAULT: u16 = 0xF1C;

/// Set Converter Format（4 位 verb，16 位 payload = 格式字）。
pub const VERB_SET_FORMAT4: u8 = 0x2;
/// Set Amp Gain/Mute（4 位 verb，16 位 payload = amp 参数）。
pub const VERB_SET_AMP4: u8 = 0x3;

/// Set Power State（12 位 verb，payload bit0..3 = D0..D3）。
pub const VERB_SET_POWER_STATE: u16 = 0x705;
/// Set Converter Stream/Channel（12 位 verb，payload 高 4 位 stream tag，低 4 位 channel）。
pub const VERB_SET_CHANNEL_STREAMID: u16 = 0x706;
/// Set Pin Widget Control（12 位 verb）。
pub const VERB_SET_PIN_WIDGET_CONTROL: u16 = 0x707;
/// Set EAPD/BTL Enable（12 位 verb）。
pub const VERB_SET_EAPD_BTLENABLE: u16 = 0x70C;

/// Get Parameter 0x00：Vendor ID（低 16 位）+ Device ID（高 16 位）。
pub const PARAM_VENDOR_ID: u8 = 0x00;
/// Get Parameter 0x04：Subordinate Node Count。
pub const PARAM_NODE_COUNT: u8 = 0x04;
/// Get Parameter 0x05：Function Group Type。
pub const PARAM_FUNCTION_TYPE: u8 = 0x05;
/// Get Parameter 0x09：Audio Widget Capabilities（低 4 位 = widget type）。
pub const PARAM_WIDGET_CAP: u8 = 0x09;
/// Get Parameter 0x0C：Pin Capabilities。
pub const PARAM_PIN_CAP: u8 = 0x0C;
/// Get Parameter 0x0E：Connection List Length。
pub const PARAM_CONN_LIST_LEN: u8 = 0x0E;

/// Widget type：Audio Output Converter（DAC）。
pub const WIDGET_AUDIO_OUTPUT: u8 = 0x0;
/// Widget type：Audio Input Converter（ADC）。
pub const WIDGET_AUDIO_INPUT: u8 = 0x1;
/// Widget type：Mixer。
pub const WIDGET_MIXER: u8 = 0x2;
/// Widget type：Selector。
pub const WIDGET_SELECTOR: u8 = 0x3;
/// Widget type：Pin Complex。
pub const WIDGET_PIN_COMPLEX: u8 = 0x4;

/// Pin Capabilities：输出能力。
pub const PIN_CAP_OUTPUT: u16 = 1 << 4;
/// Pin Capabilities：EAPD 支持。
pub const PIN_CAP_EAPD: u16 = 1 << 8;

/// Pin Widget Control：输出使能。
pub const PIN_CTL_OUT_EN: u8 = 0x40;
/// Pin Widget Control：耳机驱动使能（通常与输出使能一起置位）。
pub const PIN_CTL_HP_EN: u8 = 0x80;

/// Amp payload：bit15 = 输出放大器（0 = 输入放大器）。
pub const AMP_OUTPUT: u16 = 1 << 15;
/// Amp payload：bit7 = 静音。
pub const AMP_MUTE: u16 = 1 << 7;
/// Amp payload 中通道下标（每通道独立的 amp）的位偏移。
pub const AMP_INDEX_SHIFT: u16 = 8;

// ============================================================
// Verb / 格式字编码
// ============================================================

/// 编码一条 12 位 verb：`[31:28] codec, [27:20] nid, [19:8] verb, [7:0] payload`。
///
/// 用于 Get Parameter / Get Connection List / Set Pin Widget Control 等
/// 8 位 payload 的 verb。
pub const fn verb12(codec: u8, nid: u8, verb: u16, payload: u8) -> u32 {
    ((codec as u32) << 28) | ((nid as u32) << 20) | (((verb & 0x0FFF) as u32) << 8) | payload as u32
}

/// 编码一条 4 位 verb：`[31:28] codec, [27:20] nid, [19:16] verb, [15:0] payload`。
///
/// Set Converter Format 与 Set Amp Gain/Mute 使用 16 位 payload；如果按 12 位 verb
/// 编码（payload 只有 8 位）会丢掉高位，例如格式字 `0x4011` 的 bit14 就会丢失。
pub const fn verb4(codec: u8, nid: u8, verb: u8, payload: u16) -> u32 {
    ((codec as u32) << 28) | ((nid as u32) << 20) | (((verb & 0x0F) as u32) << 16) | payload as u32
}

/// 从 verb 响应里取 subordinate node count：低 8 位 = 起始 NID，高 8 位 = 数量。
pub const fn node_count(resp: u32) -> (u8, u8) {
    ((resp & 0xFF) as u8, ((resp >> 8) & 0xFF) as u8)
}

/// 从 Config Default 中取"默认设备"字段（bits 23:20）。
pub const fn config_default_device(config: u32) -> u8 {
    ((config >> 20) & 0xF) as u8
}

/// 把左右声道音量（0..=100）变成 amp gain（0 = 0 dB，越大越衰减）与静音位。
///
/// HDA 的 amp gain 寄存器是"衰减量"：0 表示 0 dB，0x7F 表示最大衰减。
/// 因此音量 100 映射到 gain 0，音量 0 直接置静音位。
pub fn volume_to_amp(volume: u8) -> (u8, bool) {
    let v = volume.min(100) as u16;
    if v == 0 {
        (0x7F, true)
    } else {
        (((100 - v) * 0x7F / 100) as u8, false)
    }
}

/// 构造 amp 参数 payload。
pub fn amp_payload(output: bool, index: u8, gain: u8, mute: bool) -> u16 {
    let mut v = (gain & 0x7F) as u16;
    v |= ((index as u16) & 0x0F) << AMP_INDEX_SHIFT;
    if output {
        v |= AMP_OUTPUT;
    }
    if mute {
        v |= AMP_MUTE;
    }
    v
}

/// HDA Stream Format 的位域编码。
///
/// 位域（Intel HDA spec, Stream Format）：
/// - `[3:0]`  声道数 - 1
/// - `[6:4]`  样本位宽：1 = 16 bit，2 = 20 bit，3 = 24 bit，4 = 32 bit
/// - `[10:8]` 分频 - 1（base rate / divisor）
/// - `[13:11]` 倍频 - 1（base rate * multiplier）
/// - `[14]`   base rate：1 = 48 kHz，0 = 44.1 kHz
///
/// 48 kHz / 16 bit / 2 声道 = `0x4011`（bit14 是 48 kHz base，不能漏）。
pub fn format_word(fmt: AudioFormat) -> Option<u16> {
    if fmt.channels == 0 || fmt.channels > 8 {
        return None;
    }
    let bits_code: u16 = match fmt.bits {
        16 => 1,
        20 => 2,
        24 => 3,
        32 => 4,
        _ => return None,
    };
    let (base48, mult, div) = rate_encoding(fmt.rate)?;
    // bit14 = Base Rate：0 = 48 kHz 基准，1 = 44.1 kHz 基准（注意别写反）。
    let base_bit = if base48 { 0u16 } else { 1u16 };
    Some(
        (base_bit << 14)
            | (((mult - 1) & 0x7) << 11)
            | (((div - 1) & 0x7) << 8)
            | (bits_code << 4)
            | ((fmt.channels as u16) - 1),
    )
}

/// 把采样率拆成 `(是否 48 kHz 基准, 倍频, 分频)`。
fn rate_encoding(rate: u32) -> Option<(bool, u16, u16)> {
    // 优先 48 kHz 基准，其次 44.1 kHz 基准；倍频/分频均在 1..=8。
    for (base48, base_rate) in [(true, 48_000u32), (false, 44_100u32)] {
        for mult in 1u16..=8 {
            for div in 1u16..=8 {
                if base_rate * mult as u32 == rate * div as u32 {
                    return Some((base48, mult, div));
                }
            }
        }
    }
    None
}
