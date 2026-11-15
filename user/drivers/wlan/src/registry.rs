//! 注册表槽位 —— 通用 WLAN 驱动与厂商驱动的**替换点**。
//!
//! [`generic_descriptor`] 以较低优先级（[`GENERIC_PRIORITY`]）注册通用驱动，
//! 厂商驱动用 `Registry::register_override` 注册同一功能类、更高优先级
//! （建议 [`VENDOR_PRIORITY`] 及以上）的条目即可覆盖它，服务层代码不变。
//!
//! 若注册表已满，`register_override` 会顶掉同功能类中优先级最低的条目——正是通用
//! 条目——所以厂商驱动总能挤进固定容量的槽位。
//!
//! # 示例
//!
//! ```
//! use libdevmodel::{Class, DeviceInfo, DriverDescriptor, Registry};
//! use morion_wlan::registry::{generic_descriptor, VENDOR_PRIORITY};
//!
//! let mut reg = Registry::<8>::new();
//! reg.register(generic_descriptor()).unwrap();
//!
//! fn claims_vendor(_dev: &DeviceInfo) -> bool {
//!     true // 厂商驱动自己决定认领规则
//! }
//! reg.register_override(DriverDescriptor {
//!     id: "wlan-iwlwifi",
//!     class: Class::Wlan,
//!     priority: VENDOR_PRIORITY,
//!     vendor_id: 0x8086,
//!     device_id: 0x2723,
//!     matches: claims_vendor,
//! })
//! .unwrap();
//!
//! let dev = DeviceInfo::pci(0, 0x1C, 0, 0x8086, 0x2723, 0x02, 0x80, 0, 0, 0);
//! assert_eq!(reg.resolve(Class::Wlan, &dev).unwrap().id, "wlan-iwlwifi");
//! ```

use libdevmodel::{Class, DeviceInfo, DriverDescriptor};

/// 通用驱动优先级（与 `libdevmodel` 的约定一致：通用 10..=49）。
pub const GENERIC_PRIORITY: u8 = 10;

/// 厂商专有驱动建议的最低优先级（>= 50 才能稳定覆盖通用条目）。
pub const VENDOR_PRIORITY: u8 = 50;

/// 通用驱动的设备判定：PCI `class=0x02 / subclass=0x80`（Other network controller）。
///
/// 注意不认领 `0x02/0x00`（有线以太网），避免把 eth 抢走。
pub fn claims_generic(dev: &DeviceInfo) -> bool {
    dev.matches_class(0x02, 0x80)
}

/// 已知无线芯片判定（查 `chips` 表）。
pub fn claims_known_chip(dev: &DeviceInfo) -> bool {
    crate::chips::is_known(dev.vendor_id, dev.device_id)
}

/// 厂商驱动的设备判定：标准 WLAN class **或** 已知芯片 ID。
///
/// `DriverDescriptor` 的 VID/PID 字段会再做一次精确过滤，所以这里只负责"看起来像 WiFi"。
pub fn claims_vendor(dev: &DeviceInfo) -> bool {
    claims_generic(dev) || claims_known_chip(dev)
}

/// 通用 WLAN 驱动的注册条目。
///
/// 厂商驱动替换时**不要**修改它，而是注册自己的 `DriverDescriptor`（更高
/// `priority`）。示例见本模块文档。
pub const fn generic_descriptor() -> DriverDescriptor {
    DriverDescriptor {
        id: "wlan-generic",
        class: Class::Wlan,
        priority: GENERIC_PRIORITY,
        vendor_id: 0,
        device_id: 0,
        matches: claims_generic,
    }
}

/// 厂商驱动注册条目：按 **VID:PID 精确认领**，优先级为 [`VENDOR_PRIORITY`]。
///
/// 一行注册即可覆盖通用槽位：
///
/// ```
/// use libdevmodel::{Class, DeviceInfo, Registry};
/// use morion_wlan::registry::{generic_descriptor, vendor_descriptor};
///
/// let mut reg = Registry::<8>::new();
/// reg.register(generic_descriptor()).unwrap();
/// reg.register_override(vendor_descriptor("wlan-iwlwifi", 0x8086, 0x2723))
///     .unwrap();
///
/// // AX200 即使 class 不是 0x02/0x80，也由厂商条目认领。
/// let dev = DeviceInfo::pci(3, 0, 0, 0x8086, 0x2723, 0x02, 0x00, 0, 0, 0);
/// assert_eq!(reg.resolve(Class::Wlan, &dev).unwrap().id, "wlan-iwlwifi");
/// ```
pub const fn vendor_descriptor(
    id: &'static str,
    vendor_id: u16,
    device_id: u16,
) -> DriverDescriptor {
    DriverDescriptor {
        id,
        class: Class::Wlan,
        priority: VENDOR_PRIORITY,
        vendor_id,
        device_id,
        matches: claims_vendor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libdevmodel::Registry;

    #[test]
    fn vendor_descriptor_overrides_generic_for_known_chip() {
        let mut reg = Registry::<8>::new();
        reg.register(generic_descriptor()).unwrap();
        reg.register_override(vendor_descriptor("wlan-iwlwifi", 0x8086, 0x2723))
            .unwrap();

        // 已知芯片 + 非标准 class：厂商条目仍认领。
        let dev = DeviceInfo::pci(3, 0, 0, 0x8086, 0x2723, 0x02, 0x00, 0, 0, 0);
        assert_eq!(reg.resolve(Class::Wlan, &dev).unwrap().id, "wlan-iwlwifi");

        // 别的厂商芯片仍落回通用条目（class 0x02/0x80）。
        let other = DeviceInfo::pci(3, 0, 0, 0x14C3, 0x7961, 0x02, 0x80, 0, 0, 0);
        assert_eq!(reg.resolve(Class::Wlan, &other).unwrap().id, "wlan-generic");
    }

    #[test]
    fn vendor_descriptor_handles_new_chip_by_class() {
        let mut reg = Registry::<4>::new();
        reg.register(generic_descriptor()).unwrap();
        // 表里没有的新型号，但 class 是 0x02/0x80，VID/PID 精确匹配。
        reg.register_override(vendor_descriptor("wlan-future", 0x8086, 0xFFFF))
            .unwrap();
        let dev = DeviceInfo::pci(3, 0, 0, 0x8086, 0xFFFF, 0x02, 0x80, 0, 0, 0);
        assert_eq!(reg.resolve(Class::Wlan, &dev).unwrap().id, "wlan-future");
    }
}
