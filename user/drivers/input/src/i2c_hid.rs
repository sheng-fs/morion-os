//! I2C-HID 触摸板：传输层契约 + 报告描述符/报告解析。
//!
//! 这里**不含任何厂商 I2C 控制器代码**：控制器由平台/厂商实现 [`I2cHidTransport`]
//! （PCH 的 DesignWare I2C、AMD FCH、ARM I2C 控制器…），本模块只负责 I2C-HID 协议层。
//! 于是换平台时只需换一份 transport 实现，解析与事件模型完全复用。
//!
//! 寄存器编号沿用任务约定的简化视图（真实 I2C-HID 的寄存器地址由 HID 描述符给出，
//! 这里 `read`/`write` 的 `reg` 参数原样透传给 transport，具体映射由厂商实现决定）：
//!
//! | 寄存器 | 用途 |
//! |--------|------|
//! | 0x01 | HID 描述符（30 字节） |
//! | 0x02 | 报告描述符（地址取自 HID 描述符） |
//! | 0x03 | 输入报告 |
//! | 0x08 | 电源（0x00 = ON） |
//!
//! 由于 [`I2cHidTransport`] 的 `reg` 是 `u8`，HID 描述符里若给出大于 0xFF 的报告描述符
//! 寄存器地址会被判为不支持（`NotSupported`），需要厂商在 transport 内部做 16 位映射。

use libdevmodel::DriverError;

// ============================================================
// 传输层
// ============================================================

/// I2C-HID 传输层契约（真正的 I2C 控制器驱动由平台/厂商实现）。
pub trait I2cHidTransport {
    /// 从 `reg` 读满 `out.len()` 字节。
    fn read(&mut self, addr: u8, reg: u8, out: &mut [u8]) -> Result<(), DriverError>;
    /// 向 `reg` 写 `data`。
    fn write(&mut self, addr: u8, reg: u8, data: &[u8]) -> Result<(), DriverError>;
    /// 忙等 `ms` 毫秒（上电/复位时序用）。
    fn sleep_ms(&mut self, ms: u32);
}

/// HID 描述符寄存器。
pub const REG_HID_DESCRIPTOR: u8 = 0x01;
/// 输入报告寄存器。
pub const REG_INPUT: u8 = 0x03;
/// 电源寄存器。
pub const REG_POWER: u8 = 0x08;

/// HID 描述符长度（I2C-HID 固定 30 字节）。
pub const HID_DESC_LEN: usize = 30;
/// 报告描述符缓存上限（足够覆盖绝大多数触摸板；超出返回 `Invalid`）。
pub const MAX_REPORT_DESCRIPTOR: usize = 512;
/// 输入报告缓存上限。
pub const MAX_INPUT_LEN: usize = 64;
/// 同时跟踪的报告 ID 数（触摸板通常 1~2 个）。
pub const MAX_REPORTS: usize = 4;

// ============================================================
// HID 描述符（30 字节）
// ============================================================

/// 从 HID 描述符中解析出的关键信息。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HidDescriptorInfo {
    /// 报告描述符字节数。
    pub report_desc_len: u16,
    /// 报告描述符寄存器（低 8 位）。
    pub report_desc_reg: u8,
    /// 输入报告寄存器。
    pub input_reg: u8,
    /// 单个输入报告的最大字节数（含 Report ID 字节）。
    pub max_input_len: u16,
    /// 触摸板厂商 ID（HID 描述符偏移 20 的 `wVendorID`）。
    pub vendor_id: u16,
    /// 产品 ID（偏移 22 的 `wProductID`）。
    pub product_id: u16,
    /// 固件版本（偏移 24 的 `wVersionID`）。
    pub version_id: u16,
}

/// 常见触摸板厂商（HID 描述符里的 `wVendorID`）。
///
/// I2C 从地址几乎都是 `0x2C`，无法据此区分芯片；厂商 ID 来自 HID 描述符，才是可用的
/// 身份信息。
pub fn vendor_name(vendor_id: u16) -> &'static str {
    match vendor_id {
        0x06CB => "Synaptics 触摸板",
        0x04F3 => "Elan 触摸板",
        0x27C6 => "Goodix 触摸板",
        0x03EB => "Atmel 触摸板",
        0x2808 => "FocalTech 触摸板",
        0x04B4 => "Cypress 触摸板",
        0x044E => "Alps 触摸板",
        0x046D => "Logitech 触摸板",
        0x0EEF => "eGalax 触摸板",
        _ => "未知触摸板厂商",
    }
}

fn le16(bytes: &[u8], off: usize) -> u16 {
    bytes[off] as u16 | ((bytes[off + 1] as u16) << 8)
}

/// 解析 I2C-HID 的 30 字节 HID 描述符。
pub fn parse_hid_descriptor(bytes: &[u8]) -> Result<HidDescriptorInfo, DriverError> {
    if bytes.len() < HID_DESC_LEN {
        return Err(DriverError::Invalid);
    }
    let report_desc_len = le16(bytes, 4);
    let report_desc_reg = le16(bytes, 6);
    let input_reg = le16(bytes, 8);
    let max_input_len = le16(bytes, 10);
    let vendor_id = le16(bytes, 20);
    let product_id = le16(bytes, 22);
    let version_id = le16(bytes, 24);
    if report_desc_reg > u8::MAX as u16 || input_reg > u8::MAX as u16 {
        return Err(DriverError::NotSupported);
    }
    Ok(HidDescriptorInfo {
        report_desc_len,
        report_desc_reg: report_desc_reg as u8,
        input_reg: if input_reg == 0 {
            REG_INPUT
        } else {
            input_reg as u8
        },
        max_input_len,
        vendor_id,
        product_id,
        version_id,
    })
}

// ============================================================
// 报告描述符解析
// ============================================================

/// 报告里一个字段的位置。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HidField {
    /// 相对**报告负载**（不含 Report ID 字节）的位偏移。
    pub bit_offset: u16,
    /// 位宽。
    pub bit_size: u8,
    /// 逻辑最大值（用于把原始值映射到坐标范围）。
    pub logical_max: u32,
}

/// 一份输入报告的布局。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HidReportLayout {
    pub report_id: u8,
    /// 报告负载总位数（不含 Report ID 字节）。
    pub bit_len: u16,
    pub x: Option<HidField>,
    pub y: Option<HidField>,
    pub contact_id: Option<HidField>,
    pub tip_switch: Option<HidField>,
    /// 是否属于 Digitizer（触摸）报告。
    pub is_touch: bool,
}

impl HidReportLayout {
    /// 占位值。
    pub const EMPTY: Self = Self {
        report_id: 0,
        bit_len: 0,
        x: None,
        y: None,
        contact_id: None,
        tip_switch: None,
        is_touch: false,
    };

    /// 是否同时具备 X/Y。
    pub fn has_xy(&self) -> bool {
        self.x.is_some() && self.y.is_some()
    }
}

/// 一份报告描述符里解析出的全部输入报告。
#[derive(Clone, Copy)]
pub struct HidLayout {
    reports: [HidReportLayout; MAX_REPORTS],
    len: usize,
}

impl HidLayout {
    /// 已解析的报告列表。
    pub fn reports(&self) -> &[HidReportLayout] {
        &self.reports[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 选出"主要输入报告"：优先带触摸标记（tip switch / Digitizer 用法）且含 X/Y 的报告。
    pub fn primary(&self) -> Option<&HidReportLayout> {
        let mut best: Option<&HidReportLayout> = None;
        let mut best_score = 0u8;
        for r in self.reports() {
            if !r.has_xy() {
                continue;
            }
            let score = if r.tip_switch.is_some() {
                3
            } else if r.is_touch {
                2
            } else {
                1
            };
            if best.is_none() || score > best_score {
                best = Some(r);
                best_score = score;
            }
        }
        best
    }
}

/// 本地 Usage 表容量（一个 Main item 前的用法列表）。
const MAX_LOCAL_USAGES: usize = 8;

struct Parser {
    usage_page: u32,
    logical_max: i32,
    report_size: u8,
    report_count: u8,
    report_id: u8,
    usages: [u32; MAX_LOCAL_USAGES],
    usage_len: usize,
    reports: [HidReportLayout; MAX_REPORTS],
    report_len: usize,
}

impl Parser {
    fn new() -> Self {
        Self {
            usage_page: 0,
            logical_max: 0,
            report_size: 0,
            report_count: 0,
            report_id: 0,
            usages: [0; MAX_LOCAL_USAGES],
            usage_len: 0,
            reports: [HidReportLayout::EMPTY; MAX_REPORTS],
            report_len: 0,
        }
    }

    fn reset_locals(&mut self) {
        self.usage_len = 0;
    }

    fn push_usage(&mut self, usage: u32) {
        if self.usage_len < MAX_LOCAL_USAGES {
            self.usages[self.usage_len] = usage;
            self.usage_len += 1;
        }
    }

    /// HID 规定：用法列表短于 report count 时，最后一个用法重复。
    fn usage_at(&self, index: usize) -> u32 {
        if self.usage_len == 0 {
            0
        } else if index < self.usage_len {
            self.usages[index]
        } else {
            self.usages[self.usage_len - 1]
        }
    }

    fn report_slot(&mut self, id: u8) -> Option<usize> {
        for i in 0..self.report_len {
            if self.reports[i].report_id == id {
                return Some(i);
            }
        }
        if self.report_len >= MAX_REPORTS {
            return None;
        }
        self.reports[self.report_len] = HidReportLayout {
            report_id: id,
            ..HidReportLayout::EMPTY
        };
        self.report_len += 1;
        Some(self.report_len - 1)
    }

    fn input_item(&mut self, flags: u32) {
        let Some(ri) = self.report_slot(self.report_id) else {
            return;
        };
        let constant = flags & 0x01 != 0;
        let size = self.report_size;
        let count = self.report_count;
        let logical_max = if self.logical_max > 0 {
            self.logical_max as u32
        } else {
            0
        };
        let mut bit = self.reports[ri].bit_len;
        for i in 0..count {
            let usage = self.usage_at(i as usize);
            if !constant && size != 0 {
                let field = HidField {
                    bit_offset: bit,
                    bit_size: size,
                    logical_max,
                };
                match usage {
                    // Generic Desktop X / Y。
                    0x0001_0030 => self.reports[ri].x = Some(field),
                    0x0001_0031 => self.reports[ri].y = Some(field),
                    // Digitizer Contact Identifier。
                    0x000D_0051 => self.reports[ri].contact_id = Some(field),
                    // Digitizer Tip Switch。
                    0x000D_0042 => {
                        self.reports[ri].tip_switch = Some(field);
                        self.reports[ri].is_touch = true;
                    }
                    // 其它 Digitizer 用法（Finger / In Range …）说明这是触摸报告。
                    u if u >> 16 == 0x0D => self.reports[ri].is_touch = true,
                    _ => {}
                }
            }
            bit = bit.saturating_add(size as u16);
        }
        self.reports[ri].bit_len = bit;
    }

    fn feed_item(&mut self, typ: u8, tag: u8, data: u32, size: usize) {
        match typ {
            // Main
            0 => {
                if tag == 0x8 {
                    self.input_item(data);
                }
                // Collection / End Collection / Output / Feature 之后本地用法都要清空。
                self.reset_locals();
            }
            // Global
            1 => match tag {
                0x0 => self.usage_page = data,
                // Logical Maximum（有符号解释：0xFF 在 1 字节时是 -1）。
                0x2 => self.logical_max = sign_extend(data, size),
                0x7 => self.report_size = data as u8,
                0x8 => self.report_id = data as u8,
                0x9 => self.report_count = data as u8,
                _ => {}
            },
            // Local：只处理 Usage（tag 0）。Usage Minimum/Maximum（tag 1/2）只
            // 描述范围，本简化解析器不需要。
            2 if tag == 0x0 => {
                // 4 字节 Extended Usage 自带 page（高 16 位）。
                let usage = if size == 4 {
                    data
                } else {
                    (self.usage_page << 16) | (data & 0xFFFF)
                };
                self.push_usage(usage);
            }
            _ => {}
        }
    }
}

fn read_uint(bytes: &[u8]) -> u32 {
    let mut v = 0u32;
    for (i, b) in bytes.iter().enumerate() {
        v |= (*b as u32) << (8 * i);
    }
    v
}

fn sign_extend(value: u32, size: usize) -> i32 {
    let bits = (size * 8) as u32;
    if bits == 0 {
        return 0;
    }
    if bits >= 32 {
        return value as i32;
    }
    let sign = 1u32 << (bits - 1);
    if value & sign != 0 {
        (value | !((1u32 << bits) - 1)) as i32
    } else {
        value as i32
    }
}

/// 解析 HID 报告描述符，提取主要输入报告（Report ID / X、Y 逻辑最大值 / 触摸字段）。
///
/// 只做触摸板需要的最小解析：正确处理长条目、全局/本地/主条目三类前缀，跳过未知条目；
/// 不做 collection 语义、usage range、push/pop 等完整 HID 状态机。
pub fn parse_report_descriptor(desc: &[u8]) -> Result<HidLayout, DriverError> {
    let mut p = Parser::new();
    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];
        // 长条目：0xFE、数据长度、tag，然后数据。
        if prefix == 0xFE {
            if i + 3 > desc.len() {
                return Err(DriverError::Invalid);
            }
            let size = desc[i + 1] as usize;
            i += 3 + size;
            if i > desc.len() {
                return Err(DriverError::Invalid);
            }
            continue;
        }
        let size = match prefix & 0x03 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let typ = (prefix >> 2) & 0x03;
        let tag = prefix >> 4;
        if i + 1 + size > desc.len() {
            return Err(DriverError::Invalid);
        }
        let data = read_uint(&desc[i + 1..i + 1 + size]);
        i += 1 + size;
        p.feed_item(typ, tag, data, size);
    }

    if p.report_len == 0 {
        return Err(DriverError::Invalid);
    }
    Ok(HidLayout {
        reports: p.reports,
        len: p.report_len,
    })
}

/// 从报告里按位取一个无符号值（HID 报告是小端位序）。
pub fn get_bits(data: &[u8], bit_offset: u16, bit_size: u8) -> u32 {
    let mut v = 0u32;
    for i in 0..bit_size as u16 {
        let bit = bit_offset.saturating_add(i);
        let byte = (bit / 8) as usize;
        let idx = bit % 8;
        if let Some(b) = data.get(byte) {
            if b & (1 << idx) != 0 {
                v |= 1u32 << i;
            }
        }
    }
    v
}

// ============================================================
// 触摸报告
// ============================================================

/// 触摸板工作模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TouchpadMode {
    /// 输出绝对坐标（[`libdevmodel::InputEvent::Touch`]），适合 GUI 直接定位。
    Absolute,
    /// 把触摸映射成相对移动 + 左键，兼容只认鼠标的上层。
    MouseEmulation,
}

/// 一条解析后的触摸报告。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TouchReport {
    /// 接触点 ID。
    pub id: u8,
    /// 原始 X（0..=X 逻辑最大值）。
    pub x: u16,
    /// 原始 Y。
    pub y: u16,
    /// 是否按下（tip switch）。
    pub tip: bool,
}

fn clamp_field(field: &HidField, raw: u32) -> u16 {
    let max = if field.logical_max == 0 {
        u16::MAX as u32
    } else {
        field.logical_max.min(u16::MAX as u32)
    };
    raw.min(max) as u16
}

/// 按布局解析一条输入报告。
///
/// `data` 是完整报告（含 Report ID 字节，若布局里 `report_id != 0`）。
pub fn parse_touch_report(
    layout: &HidReportLayout,
    data: &[u8],
) -> Result<TouchReport, DriverError> {
    let (x_field, y_field) = match (layout.x, layout.y) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(DriverError::NotSupported),
    };

    let payload = if layout.report_id != 0 {
        if data.first() != Some(&layout.report_id) {
            return Err(DriverError::Invalid);
        }
        &data[1..]
    } else {
        data
    };
    if payload.is_empty() || (payload.len() as u32) * 8 < layout.bit_len as u32 {
        return Err(DriverError::Invalid);
    }

    let x = clamp_field(
        &x_field,
        get_bits(payload, x_field.bit_offset, x_field.bit_size),
    );
    let y = clamp_field(
        &y_field,
        get_bits(payload, y_field.bit_offset, y_field.bit_size),
    );
    let tip = match layout.tip_switch {
        Some(f) => get_bits(payload, f.bit_offset, 1) != 0,
        // 没有 tip switch 的报告（例如纯坐标数字化仪）视为始终按下。
        None => true,
    };
    let id = match layout.contact_id {
        Some(f) => get_bits(payload, f.bit_offset, f.bit_size) as u8,
        None => 0,
    };

    Ok(TouchReport { id, x, y, tip })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::I2cHidTouchpadDriver;
    use libdevmodel::ops::BUTTON_LEFT;
    use libdevmodel::{DeviceInfo, Driver, InputEvent};

    const REPORT_DESC_REG: u8 = 0x02;

    /// 一份标准的单点触摸报告描述符（61 字节）：
    /// Report ID 1；tip(1 bit) + padding(7) + contact(8) + X(16) + Y(16)。
    const REPORT_DESC: [u8; 61] = [
        0x05, 0x0D, // Usage Page (Digitizer)
        0x09, 0x22, // Usage (Finger)
        0xA1, 0x02, // Collection (Logical)
        0x85, 0x01, //   Report ID (1)
        0x09, 0x42, //   Usage (Tip Switch)
        0x15, 0x00, //   Logical Minimum (0)
        0x25, 0x01, //   Logical Maximum (1)
        0x75, 0x01, //   Report Size (1)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x02, //   Input (Data,Var,Abs)
        0x75, 0x07, //   Report Size (7)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x03, //   Input (Const) —— 7 位对齐填充
        0x09, 0x51, //   Usage (Contact Identifier)
        0x25, 0x7F, //   Logical Maximum (127)
        0x75, 0x08, //   Report Size (8)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x02, //   Input (Data,Var,Abs)
        0x05, 0x01, //   Usage Page (Generic Desktop)
        0x09, 0x30, //   Usage (X)
        0x26, 0xFF, 0x0F, //   Logical Maximum (4095)
        0x75, 0x10, //   Report Size (16)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x02, //   Input (Data,Var,Abs)
        0x09, 0x31, //   Usage (Y)
        0x26, 0xFF, 0x0F, //   Logical Maximum (4095)
        0x75, 0x10, //   Report Size (16)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x02, //   Input (Data,Var,Abs)
        0xC0, // End Collection
    ];

    fn hid_descriptor(max_input_len: u16) -> [u8; HID_DESC_LEN] {
        let mut d = [0u8; HID_DESC_LEN];
        d[0] = HID_DESC_LEN as u8; // wHIDDescLength
        d[1] = 0;
        d[2] = 0x11; // bcdVersion 1.11
        d[3] = 0x01;
        d[4] = REPORT_DESC.len() as u8; // wReportDescLength
        d[5] = 0;
        d[6] = REPORT_DESC_REG; // wReportDescRegister
        d[7] = 0;
        d[8] = REG_INPUT; // wInputRegister
        d[9] = 0;
        d[10] = max_input_len as u8; // wMaxInputLength
        d[11] = (max_input_len >> 8) as u8;
        d[20] = 0x34; // wVendorID
        d[21] = 0x12;
        d[22] = 0x78; // wProductID
        d[23] = 0x56;
        d
    }

    #[test]
    fn parses_hid_descriptor() {
        let info = parse_hid_descriptor(&hid_descriptor(7)).unwrap();
        assert_eq!(info.report_desc_len, REPORT_DESC.len() as u16);
        assert_eq!(info.report_desc_reg, REPORT_DESC_REG);
        assert_eq!(info.input_reg, REG_INPUT);
        assert_eq!(info.max_input_len, 7);
        assert_eq!(info.vendor_id, 0x1234);
        assert_eq!(info.product_id, 0x5678);
        assert_eq!(vendor_name(0x06CB), "Synaptics 触摸板");
        assert_eq!(vendor_name(0xFFFF), "未知触摸板厂商");

        assert_eq!(
            parse_hid_descriptor(&[0u8; 10]),
            Err(DriverError::Invalid),
            "不足 30 字节应拒绝"
        );
    }

    #[test]
    fn parses_report_layout() {
        let layout = parse_report_descriptor(&REPORT_DESC).unwrap();
        assert_eq!(layout.len(), 1);
        let r = layout.primary().unwrap();
        assert_eq!(r.report_id, 1);
        assert!(r.is_touch);
        assert_eq!(r.bit_len, 48);
        assert_eq!(r.tip_switch.unwrap().bit_offset, 0);
        assert_eq!(r.contact_id.unwrap().bit_offset, 8);
        assert_eq!(r.contact_id.unwrap().bit_size, 8);
        assert_eq!(r.x.unwrap().bit_offset, 16);
        assert_eq!(r.x.unwrap().logical_max, 0x0FFF);
        assert_eq!(r.y.unwrap().bit_offset, 32);
        assert_eq!(r.y.unwrap().logical_max, 0x0FFF);
    }

    #[test]
    fn skips_long_items() {
        let mut desc = Vec::new();
        // 长条目：0xFE, 长度 2, tag 0x10, 两个数据字节。
        desc.extend_from_slice(&[0xFE, 0x02, 0x10, 0xAA, 0xBB]);
        desc.extend_from_slice(&REPORT_DESC);
        let layout = parse_report_descriptor(&desc).unwrap();
        assert_eq!(layout.primary().unwrap().x.unwrap().logical_max, 0x0FFF);
    }

    #[test]
    fn parses_touch_report() {
        let layout = parse_report_descriptor(&REPORT_DESC).unwrap();
        let r = *layout.primary().unwrap();

        let report = [0x01, 0x01, 0x05, 0x23, 0x01, 0x56, 0x04];
        assert_eq!(
            parse_touch_report(&r, &report),
            Ok(TouchReport {
                id: 5,
                x: 0x123,
                y: 0x456,
                tip: true,
            })
        );

        // 太短 / Report ID 不匹配。
        assert_eq!(
            parse_touch_report(&r, &[0x01, 0x01]),
            Err(DriverError::Invalid)
        );
        assert_eq!(
            parse_touch_report(&r, &[0x02, 0x01, 0x05, 0x23, 0x01, 0x56, 0x04]),
            Err(DriverError::Invalid)
        );

        // 无 report ID 的布局：负载直接开始。
        let raw_layout = HidReportLayout { report_id: 0, ..r };
        assert_eq!(
            parse_touch_report(&raw_layout, &[0x01, 0x05, 0x23, 0x01, 0x56, 0x04]),
            Ok(TouchReport {
                id: 5,
                x: 0x123,
                y: 0x456,
                tip: true,
            })
        );
    }

    /// 假 I2C-HID 传输层：按寄存器返回预设数据，记录写入。
    struct FakeTransport {
        hid: [u8; HID_DESC_LEN],
        rdesc: Vec<u8>,
        input: Vec<Vec<u8>>,
        writes: Vec<(u8, u8, Vec<u8>)>,
        sleeps: u32,
    }

    impl FakeTransport {
        fn new(reports: Vec<Vec<u8>>) -> Self {
            Self {
                hid: hid_descriptor(7),
                rdesc: REPORT_DESC.to_vec(),
                input: reports,
                writes: Vec::new(),
                sleeps: 0,
            }
        }
    }

    impl I2cHidTransport for FakeTransport {
        fn read(&mut self, _addr: u8, reg: u8, out: &mut [u8]) -> Result<(), DriverError> {
            match reg {
                REG_HID_DESCRIPTOR => {
                    let n = self.hid.len().min(out.len());
                    out[..n].copy_from_slice(&self.hid[..n]);
                    for b in &mut out[n..] {
                        *b = 0;
                    }
                }
                REG_INPUT => {
                    let rep = if self.input.is_empty() {
                        Vec::new()
                    } else {
                        self.input.remove(0)
                    };
                    let n = rep.len().min(out.len());
                    out[..n].copy_from_slice(&rep[..n]);
                    for b in &mut out[n..] {
                        *b = 0;
                    }
                }
                _ => {
                    let n = self.rdesc.len().min(out.len());
                    out[..n].copy_from_slice(&self.rdesc[..n]);
                    for b in &mut out[n..] {
                        *b = 0;
                    }
                }
            }
            Ok(())
        }

        fn write(&mut self, addr: u8, reg: u8, data: &[u8]) -> Result<(), DriverError> {
            self.writes.push((addr, reg, data.to_vec()));
            Ok(())
        }

        fn sleep_ms(&mut self, ms: u32) {
            self.sleeps += ms;
        }
    }

    #[test]
    fn init_reads_descriptors_and_powers_on() {
        let t = FakeTransport::new(Vec::new());
        let mut drv = I2cHidTouchpadDriver::new(t, 0x2C, TouchpadMode::Absolute);
        let dev = DeviceInfo::i2c_hid(0, 0x2C);
        assert!(drv.probe(&dev).unwrap());
        drv.start(&dev).unwrap();

        // 上电写：电源寄存器 0x08 = 0。
        let t = drv.transport();
        assert!(t
            .writes
            .iter()
            .any(|(_, reg, data)| *reg == REG_POWER && data == &[0x00]));
        assert!(t.sleeps >= 5);
        assert_eq!(drv.mode(), TouchpadMode::Absolute);
    }

    #[test]
    fn absolute_mode_emits_touch_events() {
        let touched = vec![0x01, 0x01, 0x05, 0x23, 0x01, 0x56, 0x04];
        let t = FakeTransport::new(vec![
            touched.clone(),
            touched,                                        // 相同报告：不重复产生事件
            vec![0x01, 0x00, 0x05, 0x23, 0x01, 0x56, 0x04], // 抬起
        ]);
        let mut drv = I2cHidTouchpadDriver::new(t, 0x2C, TouchpadMode::Absolute);
        let dev = DeviceInfo::i2c_hid(0, 0x2C);
        drv.start(&dev).unwrap();

        let mut out = [InputEvent::Motion { dx: 0, dy: 0 }; 4];
        let n = drv.input().unwrap().poll(&mut out).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            out[0],
            InputEvent::Touch {
                id: 5,
                x: 0x123,
                y: 0x456,
                pressure: 255,
            }
        );

        let n = drv.input().unwrap().poll(&mut out).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            out[0],
            InputEvent::Touch {
                id: 5,
                x: 0x123,
                y: 0x456,
                pressure: 0,
            }
        );
    }

    #[test]
    fn mouse_emulation_mode_maps_touch_to_motion() {
        let t = FakeTransport::new(vec![
            vec![0x01, 0x01, 0x05, 0x00, 0x08, 0x00, 0x08], // 按下 (0x800, 0x800)
            vec![0x01, 0x01, 0x05, 0x00, 0x09, 0x00, 0x08], // 右移 0x100
            vec![0x01, 0x00, 0x05, 0x00, 0x09, 0x00, 0x08], // 抬起
        ]);
        let mut drv = I2cHidTouchpadDriver::new(t, 0x2C, TouchpadMode::MouseEmulation);
        let dev = DeviceInfo::i2c_hid(0, 0x2C);
        drv.start(&dev).unwrap();

        let mut out = [InputEvent::Motion { dx: 0, dy: 0 }; 4];
        let n = drv.input().unwrap().poll(&mut out).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            out[0],
            InputEvent::Button {
                button: BUTTON_LEFT,
                pressed: true
            }
        );
        // 0x100 * 512 / 4095 = 32
        assert_eq!(out[1], InputEvent::Motion { dx: 32, dy: 0 });
        assert_eq!(
            out[2],
            InputEvent::Button {
                button: BUTTON_LEFT,
                pressed: false
            }
        );
    }

    #[test]
    fn probe_rejects_other_address_and_transport() {
        let t = FakeTransport::new(Vec::new());
        let mut drv = I2cHidTouchpadDriver::new(t, 0x2C, TouchpadMode::Absolute);
        assert!(!drv.probe(&DeviceInfo::i2c_hid(0, 0x2D)).unwrap());
        assert!(!drv.probe(&DeviceInfo::ps2()).unwrap());
    }

    #[test]
    fn poll_before_start_is_bad_state() {
        let t = FakeTransport::new(Vec::new());
        let mut drv = I2cHidTouchpadDriver::new(t, 0x2C, TouchpadMode::Absolute);
        let mut out = [InputEvent::Motion { dx: 0, dy: 0 }; 1];
        assert_eq!(
            drv.input().unwrap().poll(&mut out),
            Err(DriverError::BadState)
        );
    }
}
