//! 已知 HDA 控制器 / codec 芯片表。
//!
//! 通用发现（root → AFG → DAC → Pin）不依赖这张表也能工作在**任何**符合 HDA 规范的
//! codec 上；这里补的是"**认得出是谁**"以及少量厂商级 quirks：
//!
//! - 控制器表：把 PCI VID:DID 映射成可读的名字（Intel ICH/5~300 系、AMD FCH、NVIDIA MCP、
//!   VIA/SiS 等），服务层日志与 `/proc` 诊断用；
//! - codec 表：Realtek ALC 系列、Conexant、IDT/Sigmatel、VIA、ADI、C-Media，以及
//!   Intel/AMD/NVIDIA 的 **HDMI/DP 音频 codec**（这类 codec 没有传统 EAPD，强制关闭）；
//! - 厂商 profile：同一厂商的不同型号共享一组默认 quirks，避免逐型号重复。
//!
//! 用户要接自己的 codec 时**不需要改这里**：实现 `CodecProfile` 并传给 `HdaDriver`
//! 即可覆盖本表的任何判定。

use crate::codec::CodecQuirks;

/// 一条 PCI 音频控制器条目。
#[derive(Clone, Copy)]
pub struct ControllerChip {
    pub vendor_id: u16,
    pub device_id: u16,
    pub name: &'static str,
}

/// 已知的 HDA 控制器（PCI 设备）。
pub const CONTROLLERS: &[ControllerChip] = &[
    // ---- Intel ICH / PCH ----
    ControllerChip { vendor_id: 0x8086, device_id: 0x2668, name: "Intel ICH6 HDA" },
    ControllerChip { vendor_id: 0x8086, device_id: 0x27D8, name: "Intel ICH7 HDA" },
    ControllerChip { vendor_id: 0x8086, device_id: 0x284B, name: "Intel ICH8 HDA" },
    ControllerChip { vendor_id: 0x8086, device_id: 0x293E, name: "Intel ICH9 HDA" },
    ControllerChip { vendor_id: 0x8086, device_id: 0x3A3E, name: "Intel ICH10 HDA" },
    ControllerChip { vendor_id: 0x8086, device_id: 0x3B56, name: "Intel 5 Series HDA" },
    ControllerChip { vendor_id: 0x8086, device_id: 0x1C20, name: "Intel 6 Series HDA" },
    ControllerChip { vendor_id: 0x8086, device_id: 0x1E20, name: "Intel 7 Series HDA" },
    ControllerChip { vendor_id: 0x8086, device_id: 0x8C20, name: "Intel 8 Series HDA" },
    ControllerChip { vendor_id: 0x8086, device_id: 0x9C20, name: "Intel 9 Series HDA" },
    ControllerChip { vendor_id: 0x8086, device_id: 0xA170, name: "Intel 100 Series HDA" },
    ControllerChip { vendor_id: 0x8086, device_id: 0xA348, name: "Intel 300 Series HDA" },
    // ---- AMD FCH / Hudson ----
    ControllerChip { vendor_id: 0x1002, device_id: 0x4383, name: "AMD SB600 HDA" },
    ControllerChip { vendor_id: 0x1002, device_id: 0x780D, name: "AMD FCH HDA" },
    ControllerChip { vendor_id: 0x1002, device_id: 0x15E3, name: "AMD Family 17h HDA" },
    // ---- NVIDIA MCP ----
    ControllerChip { vendor_id: 0x10DE, device_id: 0x0BE2, name: "NVIDIA MCP79 HDA" },
    ControllerChip { vendor_id: 0x10DE, device_id: 0x0D07, name: "NVIDIA MCP89 HDA" },
    // ---- VIA / SiS ----
    ControllerChip { vendor_id: 0x1106, device_id: 0x3288, name: "VIA VT8237 HDA" },
    ControllerChip { vendor_id: 0x1039, device_id: 0x7502, name: "SiS 966 HDA" },
];

/// 一条 codec 条目（`vendor_id` 为 codec 厂商，`device_id` 为型号）。
#[derive(Clone, Copy)]
pub struct CodecChip {
    pub vendor_id: u16,
    pub device_id: u16,
    pub name: &'static str,
    pub quirks: CodecQuirks,
}

/// HDMI/DP 音频 codec 的公共 quirks：没有传统 EAPD，pin 也不需要耳机驱动位。
const HDMI_QUIRKS: CodecQuirks = CodecQuirks {
    eapd: Some(false),
    ..CodecQuirks::GENERIC
};

/// 已知 codec 型号（精确匹配）。
pub const CODECS: &[CodecChip] = &[
    // ---- Realtek ALC ----
    CodecChip { vendor_id: 0x10EC, device_id: 0x0256, name: "Realtek ALC256", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x10EC, device_id: 0x0269, name: "Realtek ALC269", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x10EC, device_id: 0x0283, name: "Realtek ALC283", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x10EC, device_id: 0x0295, name: "Realtek ALC295", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x10EC, device_id: 0x0298, name: "Realtek ALC298", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x10EC, device_id: 0x0887, name: "Realtek ALC887", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x10EC, device_id: 0x0892, name: "Realtek ALC892", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x10EC, device_id: 0x0897, name: "Realtek ALC897", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x10EC, device_id: 0x0900, name: "Realtek ALC1150", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x10EC, device_id: 0x1220, name: "Realtek ALC1220", quirks: CodecQuirks::GENERIC },
    // ---- Conexant ----
    CodecChip { vendor_id: 0x14F1, device_id: 0x506E, name: "Conexant CX20590", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x14F1, device_id: 0x5045, name: "Conexant CX20561", quirks: CodecQuirks::GENERIC },
    // ---- IDT / Sigmatel ----
    CodecChip { vendor_id: 0x111D, device_id: 0x76E7, name: "IDT 92HD87", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x111D, device_id: 0x76DF, name: "IDT 92HD81", quirks: CodecQuirks::GENERIC },
    // ---- VIA / ADI / C-Media ----
    CodecChip { vendor_id: 0x1106, device_id: 0x8446, name: "VIA VT1708S", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x11D4, device_id: 0x1889, name: "ADI AD1989A", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x13F6, device_id: 0x8168, name: "C-Media CMI9880", quirks: CodecQuirks::GENERIC },
    // ---- HDMI / DP 音频 codec ----
    CodecChip { vendor_id: 0x8086, device_id: 0x2802, name: "Intel HDMI/DP Audio", quirks: HDMI_QUIRKS },
    CodecChip { vendor_id: 0x8086, device_id: 0x2803, name: "Intel HDMI/DP Audio", quirks: HDMI_QUIRKS },
    CodecChip { vendor_id: 0x1002, device_id: 0xAA01, name: "AMD HDMI/DP Audio", quirks: HDMI_QUIRKS },
    CodecChip { vendor_id: 0x1002, device_id: 0xAA10, name: "AMD HDMI/DP Audio", quirks: HDMI_QUIRKS },
    CodecChip { vendor_id: 0x10DE, device_id: 0x0001, name: "NVIDIA HDMI/DP Audio", quirks: HDMI_QUIRKS },
    // ---- 虚拟机 ----
    CodecChip { vendor_id: 0x1AF4, device_id: 0x0011, name: "QEMU HDA output", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x1AF4, device_id: 0x0012, name: "QEMU HDA output (mixer)", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x1AF4, device_id: 0x0021, name: "QEMU HDA duplex", quirks: CodecQuirks::GENERIC },
    CodecChip { vendor_id: 0x1AF4, device_id: 0x0022, name: "QEMU HDA duplex (mixer)", quirks: CodecQuirks::GENERIC },
];

/// 厂商级默认 profile（型号未精确命中时使用）。
#[derive(Clone, Copy)]
pub struct VendorProfile {
    pub vendor_id: u16,
    pub name: &'static str,
    pub quirks: CodecQuirks,
}

pub const VENDOR_PROFILES: &[VendorProfile] = &[
    VendorProfile { vendor_id: 0x10EC, name: "Realtek ALC 系列", quirks: CodecQuirks::GENERIC },
    VendorProfile { vendor_id: 0x14F1, name: "Conexant 系列", quirks: CodecQuirks::GENERIC },
    VendorProfile { vendor_id: 0x111D, name: "IDT/Sigmatel 系列", quirks: CodecQuirks::GENERIC },
    VendorProfile { vendor_id: 0x1106, name: "VIA 系列", quirks: CodecQuirks::GENERIC },
    VendorProfile { vendor_id: 0x11D4, name: "Analog Devices 系列", quirks: CodecQuirks::GENERIC },
    VendorProfile { vendor_id: 0x13F6, name: "C-Media 系列", quirks: CodecQuirks::GENERIC },
    VendorProfile { vendor_id: 0x8086, name: "Intel HDMI/DP 系列", quirks: HDMI_QUIRKS },
    VendorProfile { vendor_id: 0x1002, name: "AMD HDMI/DP 系列", quirks: HDMI_QUIRKS },
    VendorProfile { vendor_id: 0x10DE, name: "NVIDIA HDMI/DP 系列", quirks: HDMI_QUIRKS },
    VendorProfile { vendor_id: 0x1AF4, name: "QEMU 虚拟 codec", quirks: CodecQuirks::GENERIC },
];

/// 按 PCI VID:DID 查控制器名；不认识返回 `"未知 HDA 控制器"`。
pub fn controller_name(vendor_id: u16, device_id: u16) -> &'static str {
    CONTROLLERS
        .iter()
        .find(|c| c.vendor_id == vendor_id && c.device_id == device_id)
        .map_or("未知 HDA 控制器", |c| c.name)
}

/// 精确型号条目。
pub fn codec_chip(vendor_id: u16, device_id: u16) -> Option<&'static CodecChip> {
    CODECS
        .iter()
        .find(|c| c.vendor_id == vendor_id && c.device_id == device_id)
}

/// codec 可读名字：精确型号 → 厂商系列 → `"未知 HDA codec"`。
pub fn codec_name(vendor_id: u16, device_id: u16) -> &'static str {
    if let Some(c) = codec_chip(vendor_id, device_id) {
        return c.name;
    }
    VENDOR_PROFILES
        .iter()
        .find(|p| p.vendor_id == vendor_id)
        .map_or("未知 HDA codec", |p| p.name)
}

/// codec 的 quirks：精确型号 → 厂商默认 → 完全通用。
pub fn codec_quirks(vendor_id: u16, device_id: u16) -> CodecQuirks {
    if let Some(c) = codec_chip(vendor_id, device_id) {
        return c.quirks;
    }
    VENDOR_PROFILES
        .iter()
        .find(|p| p.vendor_id == vendor_id)
        .map_or(CodecQuirks::GENERIC, |p| p.quirks)
}

/// 已知 codec 型号数量（服务启动日志用）。
pub fn known_codec_count() -> usize {
    CODECS.len()
}

/// 已知控制器数量。
pub fn known_controller_count() -> usize {
    CONTROLLERS.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_codec_names() {
        assert_eq!(codec_name(0x10EC, 0x0256), "Realtek ALC256");
        assert_eq!(codec_name(0x10EC, 0x1220), "Realtek ALC1220");
        assert_eq!(codec_name(0x1AF4, 0x0022), "QEMU HDA duplex (mixer)");
    }

    #[test]
    fn vendor_fallback_and_unknown() {
        // 未列出的 Realtek 型号仍能归到厂商系列
        assert_eq!(codec_name(0x10EC, 0xFFFF), "Realtek ALC 系列");
        assert_eq!(codec_name(0xABCD, 0x0001), "未知 HDA codec");
        assert_eq!(codec_quirks(0xABCD, 0x0001).eapd, None);
    }

    #[test]
    fn hdmi_codecs_disable_eapd() {
        assert_eq!(codec_quirks(0x8086, 0x2802).eapd, Some(false));
        assert_eq!(codec_quirks(0x1002, 0xAA01).eapd, Some(false));
        // 同厂商但未知型号也走 HDMI 厂商 profile
        assert_eq!(codec_quirks(0x10DE, 0xFFFF).eapd, Some(false));
    }

    #[test]
    fn controller_names() {
        assert_eq!(controller_name(0x8086, 0x293E), "Intel ICH9 HDA");
        assert_eq!(controller_name(0x1002, 0x780D), "AMD FCH HDA");
        assert_eq!(controller_name(0x1234, 0x5678), "未知 HDA 控制器");
    }
}
