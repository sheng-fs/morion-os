//! 厂商 HAL 契约 —— 通用 station 层与**具体芯片**之间唯一的接缝。
//!
//! 一个厂商驱动（iwlwifi / rtl88xx / ath …）只需要实现 [`WlanHw`]：
//!
//! 1. 寄存器/MMIO/DMA 访问、固件下载、微码启动；
//! 2. 扫描、关联、断开的具体命令；
//! 3. 802.11 帧的收发通道。
//!
//! 状态机、扫描结果去重/容量策略、调用顺序校验、统计计数都在
//! [`WlanStation`](crate::station::WlanStation) 里完成，厂商驱动不用重复实现。
//!
//! 注意：本 trait **不包含** WPA 四次握手。`associate` 把口令交给 HAL 后，握手由
//! HAL 内部固件或上层 supplicant 负责。

use libdevmodel::{BssEntry, DriverError, FirmwareLoader, LinkState, MacAddr, Ssid};

/// 一块无线网卡的硬件抽象层。
pub trait WlanHw {
    /// 设备 MAC（未从硬件读出时返回 [`MacAddr::ZERO`]）。
    fn mac(&self) -> MacAddr;

    /// 复位控制器到干净状态（清 DMA、停收发队列）。`Driver::start` 会调用一次。
    fn reset(&mut self) -> Result<(), DriverError>;

    /// 启动一次主动扫描。完成后用 [`WlanHw::poll_scan`] 取结果。
    fn start_scan(&mut self) -> Result<(), DriverError>;

    /// 取扫描结果，返回写入 `out` 的条数。
    ///
    /// - 返回 `0` 表示**当前没有更多结果**（扫描可能仍在进行，见
    ///   [`WlanHw::scan_done`]）；
    /// - 只要还有结果，实现应尽量填满 `out`；返回条数小于 `out.len()` 表示本批已取完；
    /// - 条数不能大于 `out.len()`，否则通用层按 [`DriverError::Invalid`] 处理。
    fn poll_scan(&mut self, out: &mut [BssEntry]) -> Result<usize, DriverError>;

    /// 扫描是否已经结束。
    ///
    /// 默认 `true`：大多数 HAL 的 `poll_scan` 是一次性取完的。若 HAL 做增量上报
    /// （一次 `poll_scan` 只吐一批），必须覆盖本方法，否则通用层会把"暂时没结果"
    /// 误判成"扫描结束"。
    fn scan_done(&self) -> bool {
        true
    }

    /// 关联到 `ssid`。`passphrase` 为空表示开放网络，否则是 8..=63 字节的 PSK 口令。
    ///
    /// 成功后 [`WlanHw::link_state`] 应报告 `Linked { .. }`；失败返回错误，通用层会把
    /// station 置为 `Failed(err)`。
    fn associate(&mut self, ssid: &Ssid, passphrase: &[u8]) -> Result<(), DriverError>;

    /// 断开当前连接（可重复调用；未连接时返回 `Ok(())`）。
    fn disconnect(&mut self) -> Result<(), DriverError>;

    /// 当前链路状态。通用层以本方法为准判断关联是否真的建立。
    fn link_state(&self) -> LinkState;

    /// 发送一帧 802.11（含 MAC 头）。
    fn tx_frame(&mut self, frame: &[u8]) -> Result<(), DriverError>;

    /// 收一帧 802.11，返回字节数；`0` 表示当前无帧。
    fn rx_frame(&mut self, out: &mut [u8]) -> Result<usize, DriverError>;

    /// 把厂商固件下载进芯片。
    ///
    /// 固件名、分块方式、校验都由 HAL 自己决定；`loader` 只负责"按名字取字节"。
    /// 默认返回 [`DriverError::NotSupported`]（不需要固件的芯片不用覆盖）。
    ///
    /// 通用层**不内置任何固件**：真实镜像是用户的财产，由服务层从 FAT32/initramfs
    /// 读出后通过 [`FirmwareLoader`] 提供。
    fn load_firmware(&mut self, loader: &mut dyn FirmwareLoader) -> Result<(), DriverError> {
        let _ = loader;
        Err(DriverError::NotSupported)
    }
}
