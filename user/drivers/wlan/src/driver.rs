//! 驱动层接线：把厂商 HAL + 通用 station 包装成 `libdevmodel::Driver`。
//!
//! 服务层只与 `Driver` / `WlanOps` 打交道；替换厂商驱动时这里一行不改，只换注册表
//! 里的条目（见 [`crate::registry`]）。

use libdevmodel::{Class, DeviceInfo, Driver, DriverError, FirmwareLoader, WlanOps};

use crate::hal::WlanHw;
use crate::station::WlanStation;

/// PCI class code：网络控制器。
const PCI_CLASS_NETWORK: u8 = 0x02;
/// PCI subclass：其它网络控制器（802.11 设备通常落在这里）。
const PCI_SUBCLASS_OTHER_NETWORK: u8 = 0x80;

/// 通用 WLAN 驱动。
///
/// `H` 是厂商 HAL，`N` 是扫描缓存容量。构造时传入 HAL；`start` 会复位硬件并把 HAL
/// 交给 [`WlanStation`]。
pub struct WlanDriver<H: WlanHw, const N: usize> {
    id: &'static str,
    vendor_id: u16,
    device_id: u16,
    /// `start` 之前的 HAL；`start` 之后被移进 `station`。
    hw: Option<H>,
    /// `start` 之后存在的能力接口。
    station: Option<WlanStation<H, N>>,
}

impl<H: WlanHw, const N: usize> WlanDriver<H, N> {
    /// 用厂商 HAL 构造驱动（默认只按 PCI class 认领设备）。
    pub fn new(hw: H) -> Self {
        Self {
            id: "wlan-generic",
            vendor_id: 0,
            device_id: 0,
            hw: Some(hw),
            station: None,
        }
    }

    /// 除 PCI class 外，再按 VID:PID 认领设备（可用于平台/非标准 class 的无线网卡）。
    pub fn with_ids(mut self, vendor_id: u16, device_id: u16) -> Self {
        self.vendor_id = vendor_id;
        self.device_id = device_id;
        self
    }

    /// 是否认领该设备。
    ///
    /// 默认认领 PCI `class=0x02 / subclass=0x80`（Other network controller）；
    /// 显式设置了 VID/PID 后，也认领 ID 精确匹配的设备；此外认领芯片表里的已知无线芯片
    /// （某些平台把 WiFi 报成 `0x02/0x00` 或厂商自定义 class）。
    pub fn claims(&self, dev: &DeviceInfo) -> bool {
        if dev.matches_class(PCI_CLASS_NETWORK, PCI_SUBCLASS_OTHER_NETWORK) {
            return true;
        }
        if crate::chips::is_known(dev.vendor_id, dev.device_id) {
            return true;
        }
        let has_id = self.vendor_id != 0 || self.device_id != 0;
        has_id && dev.matches_ids(self.vendor_id, self.device_id)
    }

    /// `start` 之后的 station（含统计/缓存访问）。
    pub fn station(&mut self) -> Option<&mut WlanStation<H, N>> {
        self.station.as_mut()
    }

    /// 让 HAL 下载固件（`start` 前后都可调用）。
    ///
    /// 固件来源由调用方实现 [`FirmwareLoader`] 决定；通用层不内置任何镜像。
    pub fn load_firmware(&mut self, loader: &mut dyn FirmwareLoader) -> Result<(), DriverError> {
        match self.station.as_mut() {
            Some(station) => station.hw().load_firmware(loader),
            None => match self.hw.as_mut() {
                Some(hw) => hw.load_firmware(loader),
                None => Err(DriverError::BadState),
            },
        }
    }
}

impl<H: WlanHw, const N: usize> Driver for WlanDriver<H, N> {
    fn name(&self) -> &'static str {
        self.id
    }

    fn class(&self) -> Class {
        Class::Wlan
    }

    fn probe(&mut self, dev: &DeviceInfo) -> Result<bool, DriverError> {
        Ok(self.claims(dev))
    }

    fn start(&mut self, dev: &DeviceInfo) -> Result<(), DriverError> {
        if self.station.is_some() {
            return Ok(());
        }
        if !self.claims(dev) {
            return Err(DriverError::NotSupported);
        }
        let mut hw = self.hw.take().ok_or(DriverError::BadState)?;
        if let Err(e) = hw.reset() {
            // 复位失败时把 HAL 还回去，允许服务层重试。
            self.hw = Some(hw);
            return Err(e);
        }
        self.station = Some(WlanStation::new(hw));
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut station) = self.station.take() {
            let _ = station.hw().disconnect();
            self.hw = Some(station.into_hw());
        }
    }

    fn wlan(&mut self) -> Option<&mut dyn WlanOps> {
        match self.station.as_mut() {
            Some(station) => Some(station as &mut dyn WlanOps),
            None => None,
        }
    }
}
