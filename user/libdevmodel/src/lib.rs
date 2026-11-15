//! 通用驱动模型（LibDevModel）。
//!
//! MorionOS 里每个驱动都是独立服务进程。本库只定义**驱动之间、驱动与服务之间**的共同
//! 契约，不放任何具体硬件逻辑：
//!
//! - [`device`]：归一化设备描述 [`DeviceInfo`](device::DeviceInfo) / 功能类 / 错误；
//! - [`driver`]：驱动契约 [`Driver`](driver::Driver)（probe → start → 能力接口）；
//! - [`ops`]：三条能力接口 —— [`AudioOps`](ops::AudioOps)、
//!   [`InputOps`](ops::InputOps)、[`WlanOps`](ops::WlanOps)；
//! - [`registry`]：固定容量注册表 + **覆盖注册**，厂商专有驱动以更高优先级替换通用驱动。
//!
//! 本库**零依赖**（不引 `libdevice` / `morion`），于是驱动核心既能编进服务进程，也能
//! 编进飞地/单测。
//!
//! # 三份实现（各自独立 crate）
//!
//! - `user/drivers/hda`：Intel HDA（AMD 同规范）通用音频控制器 + 可替换的编解码器槽；
//! - `user/drivers/input`：通用输入事件模型 + PS/2 鼠标 + I2C-HID 传输层契约；
//! - `user/drivers/wlan`：通用 WLAN station 模型 + 固件加载契约 + 厂商驱动槽。

#![cfg_attr(not(test), no_std)]

pub mod device;
pub mod driver;
pub mod ops;
pub mod registry;

pub use device::{Class, DeviceInfo, DriverError, Transport};
pub use driver::Driver;
pub use ops::{
    AudioFormat, AudioOps, BssEntry, FirmwareLoader, InputEvent, InputOps, LinkState, MacAddr,
    Security, Ssid, WlanOps,
};
pub use registry::{DriverDescriptor, Registry};

#[cfg(test)]
mod tests;
