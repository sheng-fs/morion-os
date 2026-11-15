//! 通用 WLAN 驱动模型（`morion-wlan`）。
//!
//! 真实无线网卡必须由「厂商 HAL + 固件 + WPA 握手」三部分共同驱动，一份代码不可能
//! 适配所有芯片。本 crate 只把**与芯片无关的部分**做成可复用实现：
//!
//! - [`hal::WlanHw`]：厂商 HAL —— 唯一的替换点。iwlwifi / rtl88xx / ath 等驱动只实现
//!   这一个 trait，就能接进通用 station 层；
//! - [`station::WlanStation`]：station 状态机、扫描缓存、帧收发与统计，实现
//!   `libdevmodel::WlanOps`；
//! - [`driver::WlanDriver`]：把 HAL + station 包装成 `libdevmodel::Driver`
//!   （`Class::Wlan`），服务层只认这个契约；
//! - [`fw`]：固件加载适配器（固定切片 / 固定缓冲 / 分块），**不内置任何固件**；
//! - [`chips`]：常见无线芯片 PCI ID 表（Intel / Realtek / Atheros / MediaTek / Broadcom），
//!   提供可读名字与厂商固件文件名提示；
//! - [`registry`]：通用驱动注册条目，厂商驱动以更高优先级覆盖它。
//!
//! # 状态机
//!
//! ```text
//!              scan()                       poll_scan 取完且 scan_done()
//!   Down ───────────────► Scanning ──────────────────────────────► Down（scanned = true）
//!     │  ▲                    │
//!     │  └──── leave() ───────┘
//!     │
//!     └── join()（必须已扫描）──► Associating ──► Linked { bssid }
//!                                       │
//!                                       └──► Failed(err)
//! ```
//!
//! 非法顺序（未扫描就 join、扫描中 join、已关联再 join/scan）统一返回
//! [`DriverError::BadState`]。
//!
//! # WPA 边界
//!
//! 通用层**不做** WPA/WPA2/WPA3 四次握手：`join` 只校验口令形态并把 SSID/口令交给
//! HAL；真正的握手属于厂商 HAL 内的固件/微码，或上层 supplicant 服务。扫描结果里的
//! `Security` 只做识别。
//!
//! # 固件
//!
//! 固件有版权且体积大，本 crate 不附带任何镜像。服务层从 FAT32/initramfs 读出字节，
//! 实现 [`FirmwareLoader`](libdevmodel::FirmwareLoader) 传给 HAL 即可。
//!
//! QEMU 没有通用 802.11 设备，本 crate 的验证全部来自单元测试与回环后端，
//! **不代表任何真实硬件可用**。

#![cfg_attr(not(test), no_std)]

pub mod chips;
pub mod driver;
pub mod fw;
pub mod hal;
pub mod registry;
pub mod station;

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;

pub use driver::WlanDriver;
pub use fw::{ChunkedLoader, FirmwareBuffer, SliceFirmware};
pub use hal::WlanHw;
pub use registry::{claims_generic, generic_descriptor, GENERIC_PRIORITY, VENDOR_PRIORITY};
pub use station::{ScanCache, WlanStation, WlanStats};
