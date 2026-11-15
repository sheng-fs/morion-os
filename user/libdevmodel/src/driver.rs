//! 驱动契约 —— 所有驱动（通用或厂商专有）都实现 [`Driver`]。
//!
//! 服务层只做三件事：`probe` → `start` → 通过 `audio()` / `input()` / `wlan()` 拿到能力
//! 接口。**厂商驱动替换通用驱动**时，服务层代码一行不改，只换注册表里的条目。

use crate::device::{Class, DeviceInfo, DriverError};
use crate::ops::{AudioOps, InputOps, WlanOps};

/// 一个设备驱动。
pub trait Driver {
    /// 驱动名（日志/注册表展示）。
    fn name(&self) -> &'static str;

    /// 功能类，决定服务层用哪条能力接口。
    fn class(&self) -> Class;

    /// 是否认领该设备。返回 `Ok(false)` 表示"不是我"，`Err` 表示"是我的但探测失败"。
    fn probe(&mut self, dev: &DeviceInfo) -> Result<bool, DriverError>;

    /// 初始化硬件。成功后能力接口可用。
    fn start(&mut self, dev: &DeviceInfo) -> Result<(), DriverError>;

    /// 停止硬件并释放状态（可重复调用）。
    fn stop(&mut self);

    /// 音频能力（`class == Audio` 时非空）。
    fn audio(&mut self) -> Option<&mut dyn AudioOps> {
        None
    }

    /// 输入能力（`class == Input` 时非空）。
    fn input(&mut self) -> Option<&mut dyn InputOps> {
        None
    }

    /// 无线能力（`class == Wlan` 时非空）。
    fn wlan(&mut self) -> Option<&mut dyn WlanOps> {
        None
    }
}
