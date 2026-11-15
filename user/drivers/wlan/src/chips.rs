//! 常见无线芯片 PCI ID 数据库。
//!
//! 这里**不实现任何芯片的 HAL**：真实 WiFi 需要厂商固件、HAL 与 WPA 握手。本表的作用是
//! 让通用层能**认出设备是谁**，并把「厂商 HAL + 固件」的接入点固定下来：
//!
//! - `chips::lookup(vid, did)`：识别常见 Intel / Realtek / Qualcomm Atheros / MediaTek /
//!   Broadcom 无线网卡；
//! - [`ChipInfo::firmware_hint`]：厂商 HAL 通常需要的固件文件名形状（仅供驱动实现参考，
//!   本 crate 不内置任何固件）；
//! - `registry::vendor_descriptor`：厂商驱动注册自己的 `DriverDescriptor` 时，按 VID:PID
//!   精确认领，优先级高于通用槽位。
//!
//! 认不出不代表不能用：厂商驱动仍可注册自己的匹配规则（例如按 ACPI HID 或平台设备）。

/// 无线芯片家族（决定厂商 HAL 的实现归属）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WlanFamily {
    IntelIwlwifi,
    RealtekRtl8xxx,
    QualcommAtheros,
    MediaTekMt76,
    BroadcomBrcmfmac,
}

impl WlanFamily {
    pub fn name(&self) -> &'static str {
        match self {
            Self::IntelIwlwifi => "Intel iwlwifi",
            Self::RealtekRtl8xxx => "Realtek rtl8xxx",
            Self::QualcommAtheros => "Qualcomm Atheros",
            Self::MediaTekMt76 => "MediaTek mt76",
            Self::BroadcomBrcmfmac => "Broadcom brcmfmac",
        }
    }
}

/// 一条无线芯片条目。
#[derive(Clone, Copy)]
pub struct ChipInfo {
    pub vendor_id: u16,
    pub device_id: u16,
    /// 市场名（日志/诊断）。
    pub name: &'static str,
    pub family: WlanFamily,
    /// 厂商 HAL 通常需要的固件文件形状；**不是**本 crate 内置的资源。
    pub firmware_hint: &'static str,
}

/// 常见无线芯片表。
///
/// 只收录公开资料里稳定、常见的 PCI ID；同家族的新型号即使不在表里，厂商 HAL 仍可用
/// `WlanHw` + 自己的匹配规则接入。
pub const CHIPS: &[ChipInfo] = &[
    // ---- Intel iwlwifi ----
    ChipInfo {
        vendor_id: 0x8086,
        device_id: 0x08B1,
        name: "Intel Wireless 7260",
        family: WlanFamily::IntelIwlwifi,
        firmware_hint: "iwlwifi-7260-*.ucode",
    },
    ChipInfo {
        vendor_id: 0x8086,
        device_id: 0x095A,
        name: "Intel Wireless 7265",
        family: WlanFamily::IntelIwlwifi,
        firmware_hint: "iwlwifi-7265D-*.ucode",
    },
    ChipInfo {
        vendor_id: 0x8086,
        device_id: 0x24F3,
        name: "Intel Wireless 8260",
        family: WlanFamily::IntelIwlwifi,
        firmware_hint: "iwlwifi-8000C-*.ucode",
    },
    ChipInfo {
        vendor_id: 0x8086,
        device_id: 0x24FD,
        name: "Intel Wireless 8265",
        family: WlanFamily::IntelIwlwifi,
        firmware_hint: "iwlwifi-8265-*.ucode",
    },
    ChipInfo {
        vendor_id: 0x8086,
        device_id: 0x2526,
        name: "Intel Wireless 9260",
        family: WlanFamily::IntelIwlwifi,
        firmware_hint: "iwlwifi-9260-*.ucode",
    },
    ChipInfo {
        vendor_id: 0x8086,
        device_id: 0x2723,
        name: "Intel Wi-Fi 6 AX200",
        family: WlanFamily::IntelIwlwifi,
        firmware_hint: "iwlwifi-cc-a0-*.ucode",
    },
    ChipInfo {
        vendor_id: 0x8086,
        device_id: 0x2725,
        name: "Intel Wi-Fi 6E AX210",
        family: WlanFamily::IntelIwlwifi,
        firmware_hint: "iwlwifi-ty-a0-gf-a0-*.ucode",
    },
    ChipInfo {
        vendor_id: 0x8086,
        device_id: 0x51F0,
        name: "Intel Wi-Fi 6 AX201",
        family: WlanFamily::IntelIwlwifi,
        firmware_hint: "iwlwifi-QuZ-a0-hr-b0-*.ucode",
    },
    // ---- Realtek rtl8xxx ----
    ChipInfo {
        vendor_id: 0x10EC,
        device_id: 0x8179,
        name: "Realtek RTL8188EE",
        family: WlanFamily::RealtekRtl8xxx,
        firmware_hint: "rtlwifi/rtl8188efw.bin",
    },
    ChipInfo {
        vendor_id: 0x10EC,
        device_id: 0xB723,
        name: "Realtek RTL8723BE",
        family: WlanFamily::RealtekRtl8xxx,
        firmware_hint: "rtlwifi/rtl8723befw.bin",
    },
    ChipInfo {
        vendor_id: 0x10EC,
        device_id: 0xD723,
        name: "Realtek RTL8723DE",
        family: WlanFamily::RealtekRtl8xxx,
        firmware_hint: "rtlwifi/rtl8723defw.bin",
    },
    ChipInfo {
        vendor_id: 0x10EC,
        device_id: 0xC821,
        name: "Realtek RTL8821CE",
        family: WlanFamily::RealtekRtl8xxx,
        firmware_hint: "rtlwifi/rtl8821cefw.bin",
    },
    ChipInfo {
        vendor_id: 0x10EC,
        device_id: 0xB822,
        name: "Realtek RTL8822BE",
        family: WlanFamily::RealtekRtl8xxx,
        firmware_hint: "rtlwifi/rtl8822befw.bin",
    },
    ChipInfo {
        vendor_id: 0x10EC,
        device_id: 0xC822,
        name: "Realtek RTL8822CE",
        family: WlanFamily::RealtekRtl8xxx,
        firmware_hint: "rtw88/rtw8822c_fw.bin",
    },
    ChipInfo {
        vendor_id: 0x10EC,
        device_id: 0x8852,
        name: "Realtek RTL8852AE",
        family: WlanFamily::RealtekRtl8xxx,
        firmware_hint: "rtw89/rtw8852a_fw.bin",
    },
    // ---- Qualcomm Atheros ----
    ChipInfo {
        vendor_id: 0x168C,
        device_id: 0x002B,
        name: "Qualcomm Atheros AR9565",
        family: WlanFamily::QualcommAtheros,
        firmware_hint: "ath10k/QCA9565/hw1.0/firmware-6.bin",
    },
    ChipInfo {
        vendor_id: 0x168C,
        device_id: 0x003E,
        name: "Qualcomm Atheros QCA6174",
        family: WlanFamily::QualcommAtheros,
        firmware_hint: "ath10k/QCA6174/hw3.0/firmware-6.bin",
    },
    ChipInfo {
        vendor_id: 0x168C,
        device_id: 0x0042,
        name: "Qualcomm Atheros QCA9377",
        family: WlanFamily::QualcommAtheros,
        firmware_hint: "ath10k/QCA9377/hw1.0/firmware-5.bin",
    },
    // ---- MediaTek mt76 ----
    ChipInfo {
        vendor_id: 0x14C3,
        device_id: 0x7615,
        name: "MediaTek MT7615",
        family: WlanFamily::MediaTekMt76,
        firmware_hint: "mediatek/mt7615/*.bin",
    },
    ChipInfo {
        vendor_id: 0x14C3,
        device_id: 0x7961,
        name: "MediaTek MT7921",
        family: WlanFamily::MediaTekMt76,
        firmware_hint: "mediatek/WIFI_RAM_CODE_MT7961_*.bin",
    },
    ChipInfo {
        vendor_id: 0x14C3,
        device_id: 0x7922,
        name: "MediaTek MT7922",
        family: WlanFamily::MediaTekMt76,
        firmware_hint: "mediatek/WIFI_RAM_CODE_MT7922_*.bin",
    },
    // ---- Broadcom brcmfmac ----
    ChipInfo {
        vendor_id: 0x14E4,
        device_id: 0x43A3,
        name: "Broadcom BCM4350",
        family: WlanFamily::BroadcomBrcmfmac,
        firmware_hint: "brcm/brcmfmac4350-pcie.bin",
    },
    ChipInfo {
        vendor_id: 0x14E4,
        device_id: 0x43B1,
        name: "Broadcom BCM4351",
        family: WlanFamily::BroadcomBrcmfmac,
        firmware_hint: "brcm/brcmfmac4351-pcie.bin",
    },
    ChipInfo {
        vendor_id: 0x14E4,
        device_id: 0x4464,
        name: "Broadcom BCM4364",
        family: WlanFamily::BroadcomBrcmfmac,
        firmware_hint: "brcm/brcmfmac4364-pcie.bin",
    },
];

/// 按 PCI VID:DID 查芯片。
pub fn lookup(vendor_id: u16, device_id: u16) -> Option<&'static ChipInfo> {
    CHIPS
        .iter()
        .find(|c| c.vendor_id == vendor_id && c.device_id == device_id)
}

/// 是否在已知芯片表里。
pub fn is_known(vendor_id: u16, device_id: u16) -> bool {
    lookup(vendor_id, device_id).is_some()
}

/// 芯片可读名字；未知返回 `"未知无线芯片"`。
pub fn name(vendor_id: u16, device_id: u16) -> &'static str {
    lookup(vendor_id, device_id).map_or("未知无线芯片", |c| c.name)
}

/// 已知芯片数量（服务启动日志用）。
pub fn known_count() -> usize {
    CHIPS.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_common_chips() {
        assert_eq!(name(0x8086, 0x2723), "Intel Wi-Fi 6 AX200");
        assert_eq!(name(0x10EC, 0xC821), "Realtek RTL8821CE");
        assert_eq!(name(0x168C, 0x003E), "Qualcomm Atheros QCA6174");
        assert_eq!(name(0x14C3, 0x7961), "MediaTek MT7921");
        assert_eq!(name(0x14E4, 0x43A3), "Broadcom BCM4350");
    }

    #[test]
    fn unknown_chip_falls_back() {
        assert!(lookup(0xFFFF, 0xFFFF).is_none());
        assert_eq!(name(0xFFFF, 0xFFFF), "未知无线芯片");
    }

    #[test]
    fn families_and_hints() {
        let intel = lookup(0x8086, 0x2723).unwrap();
        assert_eq!(intel.family, WlanFamily::IntelIwlwifi);
        assert!(intel.firmware_hint.starts_with("iwlwifi-"));

        let realtek = lookup(0x10EC, 0xC821).unwrap();
        assert_eq!(realtek.family, WlanFamily::RealtekRtl8xxx);
        assert!(realtek.firmware_hint.contains("rtl8821ce"));
    }

    #[test]
    fn table_is_nonempty_and_unique() {
        assert!(known_count() >= 20);
        for (i, a) in CHIPS.iter().enumerate() {
            for b in &CHIPS[i + 1..] {
                assert!(
                    !(a.vendor_id == b.vendor_id && a.device_id == b.device_id),
                    "重复的 PCI ID: {:04x}:{:04x}",
                    a.vendor_id,
                    a.device_id
                );
            }
        }
    }
}
