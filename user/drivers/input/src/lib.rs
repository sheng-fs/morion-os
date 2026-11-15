//! MorionOS 通用输入驱动。
//!
//! 面向"用户态驱动服务"的输入核心：固定容量、无分配器、不依赖内核 crate，可在宿主机
//! 单测。当前支持两类设备：
//!
//! - **PS/2 鼠标**（[`ps2`]）：i8042 AUX 口，3 字节标准包 + 可选 4 字节 IntelliMouse
//!   滚轮包；
//! - **I2C-HID 触摸板**（[`i2c_hid`]）：只做协议层（HID 描述符解析 + 报告解析），
//!   厂商 I2C 控制器实现 [`I2cHidTransport`] 即可接入。
//!
//! # 模块划分
//!
//! - [`event_queue`]：固定容量环形事件队列（满时丢最旧）；
//! - [`ps2`]：i8042 传输层契约 [`Ps2Port`]、初始化序列、字节流解码器；
//! - [`i2c_hid`]：I2C-HID 传输层契约 [`I2cHidTransport`]、HID 描述符/报告描述符/
//!   触摸报告解析；
//! - [`driver`]：把协议层包成 `libdevmodel::Driver` + `InputOps`，并提供二选一的
//!   [`InputDriver`] 入口。
//!
//! # 与现有键盘服务的关系
//!
//! 本 crate **不抢键盘**：PS/2 键盘仍由 `user/srv` 的键盘服务负责；PS/2 驱动只消费
//! AUX（鼠标）字节，轮询时遇到非 AUX 字节会把数据留在 i8042 输出缓冲里。
//! [`libdevmodel::ops::key`] 的 evdev 风格键码只是为将来统一的 USB HID 输入服务预留。
//!
//! # 传输层替换
//!
//! 两个 trait 把"协议"与"总线访问"分开：
//!
//! - [`Ps2Port`]：`status` / `read_data` / `write_data` / `write_cmd`，服务层用内核
//!   `sys_port_in8` / `sys_port_out8` 实现；
//! - [`I2cHidTransport`]：`read` / `write` / `sleep_ms`，由平台或厂商 I2C 控制器驱动
//!   实现。
//!
//! 驱动核心只依赖这两个 trait，因此换平台、换控制器不需要改协议与事件代码。

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod driver;
pub mod event_queue;
pub mod i2c_hid;
pub mod ps2;

pub use driver::{I2cHidTouchpadDriver, InputDriver, Ps2MouseDriver};
pub use event_queue::EventQueue;
pub use i2c_hid::{
    get_bits, parse_hid_descriptor, parse_report_descriptor, parse_touch_report, vendor_name,
    HidDescriptorInfo, HidField, HidLayout, HidReportLayout, I2cHidTransport, TouchReport,
    TouchpadMode,
};
pub use ps2::{MousePacket, PacketEvents, Ps2MouseDecoder, Ps2MouseKind, Ps2Port};
