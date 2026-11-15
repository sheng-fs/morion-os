//! **仅测试用**的 WLAN 后端。
//!
//! 本模块只在 `cfg(test)` 下编译。两个实现都不做任何真实无线电收发，
//! 生产代码不得引用，也不代表对任何芯片的支持：
//!
//! - [`FakeWlanHw`]：可编排扫描结果、行/列式增量上报、关联成功/失败、回环帧，
//!   用来覆盖状态机与缓存策略；
//! - [`LoopbackHw`]：最小回环后端，`tx` 的帧从 `rx` 原样取回，用来验证帧通路。

use libdevmodel::{BssEntry, DriverError, FirmwareLoader, LinkState, MacAddr, Security, Ssid};

use crate::hal::WlanHw;

/// Fake 设备的 MAC。
pub const FAKE_MAC: MacAddr = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
/// Fake 关联成功后的 BSSID。
pub const FAKE_BSSID: MacAddr = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0xA0]);
/// Fake 设备期望的固件名。
pub const FAKE_FIRMWARE: &str = "fake.bin";

/// 可编排的测试 HAL。
pub struct FakeWlanHw {
    pub mac: MacAddr,
    /// `reset` 被调用次数（验证 `Driver::start` 行为）。
    pub reset_calls: u32,
    /// 编排的扫描结果。
    pub scan_results: [BssEntry; 8],
    pub scan_len: usize,
    scan_pos: usize,
    scan_finished: bool,
    /// 每次 `poll_scan` 最多返回几条（< 结果总数可模拟增量扫描）。
    pub scan_batch: usize,
    /// 非空时 `start_scan` 直接失败。
    pub scan_fail: Option<DriverError>,
    /// 非空时 `poll_scan` 直接失败。
    pub poll_fail: Option<DriverError>,
    /// 非空时 `associate` 失败。
    pub associate_fail: Option<DriverError>,
    /// 关联成功时报告的 BSSID。
    pub link_bssid: MacAddr,
    /// 当前链路状态。
    pub link: LinkState,
    /// 最近一次 `associate` 收到的 SSID 与口令长度（验证参数委托）。
    pub last_ssid: Option<Ssid>,
    pub last_pass_len: usize,
    /// 最近一次 `tx_frame` 的内容。
    pub tx: [u8; 64],
    pub tx_len: usize,
    /// 待 `rx_frame` 取走的帧。
    pub rx_pending: [u8; 64],
    pub rx_len: usize,
    /// 置位后 `tx_frame` 把帧回环到 `rx_pending`。
    pub loopback: bool,
    /// 固件加载结果。
    pub fw_len: usize,
    pub fw_loaded: bool,
}

impl Default for FakeWlanHw {
    fn default() -> Self {
        Self {
            mac: FAKE_MAC,
            reset_calls: 0,
            scan_results: [BssEntry::EMPTY; 8],
            scan_len: 0,
            scan_pos: 0,
            scan_finished: false,
            scan_batch: 8,
            scan_fail: None,
            poll_fail: None,
            associate_fail: None,
            link_bssid: FAKE_BSSID,
            link: LinkState::Down,
            last_ssid: None,
            last_pass_len: 0,
            tx: [0; 64],
            tx_len: 0,
            rx_pending: [0; 64],
            rx_len: 0,
            loopback: false,
            fw_len: 0,
            fw_loaded: false,
        }
    }
}

impl FakeWlanHw {
    /// 追加一条编排好的扫描结果。
    pub fn push_scan(&mut self, entry: BssEntry) {
        if self.scan_len < self.scan_results.len() {
            self.scan_results[self.scan_len] = entry;
            self.scan_len += 1;
        }
    }

    /// 预置一帧供 `rx_frame` 取走。
    pub fn queue_rx(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
        if bytes.len() > self.rx_pending.len() {
            return Err(DriverError::Invalid);
        }
        self.rx_pending[..bytes.len()].copy_from_slice(bytes);
        self.rx_len = bytes.len();
        Ok(())
    }
}

impl WlanHw for FakeWlanHw {
    fn mac(&self) -> MacAddr {
        self.mac
    }

    fn reset(&mut self) -> Result<(), DriverError> {
        self.reset_calls += 1;
        self.scan_pos = 0;
        self.scan_finished = false;
        self.link = LinkState::Down;
        Ok(())
    }

    fn start_scan(&mut self) -> Result<(), DriverError> {
        if let Some(e) = self.scan_fail {
            return Err(e);
        }
        self.scan_pos = 0;
        self.scan_finished = false;
        Ok(())
    }

    fn poll_scan(&mut self, out: &mut [BssEntry]) -> Result<usize, DriverError> {
        if let Some(e) = self.poll_fail {
            return Err(e);
        }
        if self.scan_finished || out.is_empty() {
            return Ok(0);
        }
        let remaining = self.scan_len - self.scan_pos;
        let batch = if self.scan_batch == 0 {
            1
        } else {
            self.scan_batch
        };
        let mut n = core::cmp::min(batch, out.len());
        if n > remaining {
            n = remaining;
        }
        let mut i = 0;
        while i < n {
            out[i] = self.scan_results[self.scan_pos + i];
            i += 1;
        }
        self.scan_pos += n;
        if self.scan_pos >= self.scan_len {
            self.scan_finished = true;
        }
        Ok(n)
    }

    fn scan_done(&self) -> bool {
        self.scan_finished
    }

    fn associate(&mut self, ssid: &Ssid, passphrase: &[u8]) -> Result<(), DriverError> {
        self.last_ssid = Some(*ssid);
        self.last_pass_len = passphrase.len();
        if let Some(e) = self.associate_fail {
            self.link = LinkState::Failed(e);
            return Err(e);
        }
        self.link = LinkState::Linked {
            bssid: self.link_bssid,
        };
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), DriverError> {
        self.link = LinkState::Down;
        Ok(())
    }

    fn link_state(&self) -> LinkState {
        self.link
    }

    fn tx_frame(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        if frame.len() > self.tx.len() {
            return Err(DriverError::Invalid);
        }
        self.tx[..frame.len()].copy_from_slice(frame);
        self.tx_len = frame.len();
        if self.loopback {
            self.rx_pending[..frame.len()].copy_from_slice(frame);
            self.rx_len = frame.len();
        }
        Ok(())
    }

    fn rx_frame(&mut self, out: &mut [u8]) -> Result<usize, DriverError> {
        if self.rx_len == 0 {
            return Ok(0);
        }
        if out.len() < self.rx_len {
            return Err(DriverError::Invalid);
        }
        let n = self.rx_len;
        out[..n].copy_from_slice(&self.rx_pending[..n]);
        self.rx_len = 0;
        Ok(n)
    }

    fn load_firmware(&mut self, loader: &mut dyn FirmwareLoader) -> Result<(), DriverError> {
        let mut buf = [0u8; 32];
        let n = loader.load(FAKE_FIRMWARE, &mut buf)?;
        self.fw_len = n;
        self.fw_loaded = true;
        Ok(())
    }
}

/// **仅测试用**回环后端：`tx` 写入的帧由 `rx` 原样取回，不做任何无线电收发。
pub struct LoopbackHw {
    pub mac: MacAddr,
    pub bssid: MacAddr,
    link: LinkState,
    frame: [u8; 64],
    len: usize,
    pending_scan: bool,
}

impl LoopbackHw {
    /// 构造回环后端（初始 `Down`）。
    pub fn new() -> Self {
        Self {
            mac: MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]),
            bssid: MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0xB0]),
            link: LinkState::Down,
            frame: [0; 64],
            len: 0,
            pending_scan: false,
        }
    }

    fn loopback_bss(&self) -> BssEntry {
        BssEntry {
            bssid: self.bssid,
            ssid: Ssid::new(b"loopback").unwrap(),
            channel: 1,
            rssi: -40,
            security: Security::Open,
        }
    }
}

impl Default for LoopbackHw {
    fn default() -> Self {
        Self::new()
    }
}

impl WlanHw for LoopbackHw {
    fn mac(&self) -> MacAddr {
        self.mac
    }

    fn reset(&mut self) -> Result<(), DriverError> {
        self.link = LinkState::Down;
        self.len = 0;
        self.pending_scan = false;
        Ok(())
    }

    fn start_scan(&mut self) -> Result<(), DriverError> {
        self.pending_scan = true;
        Ok(())
    }

    fn poll_scan(&mut self, out: &mut [BssEntry]) -> Result<usize, DriverError> {
        if !self.pending_scan || out.is_empty() {
            return Ok(0);
        }
        out[0] = self.loopback_bss();
        self.pending_scan = false;
        Ok(1)
    }

    fn associate(&mut self, _ssid: &Ssid, _passphrase: &[u8]) -> Result<(), DriverError> {
        self.link = LinkState::Linked { bssid: self.bssid };
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), DriverError> {
        self.link = LinkState::Down;
        Ok(())
    }

    fn link_state(&self) -> LinkState {
        self.link
    }

    fn tx_frame(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        if frame.len() > self.frame.len() {
            return Err(DriverError::Invalid);
        }
        self.frame[..frame.len()].copy_from_slice(frame);
        self.len = frame.len();
        Ok(())
    }

    fn rx_frame(&mut self, out: &mut [u8]) -> Result<usize, DriverError> {
        if self.len == 0 {
            return Ok(0);
        }
        if out.len() < self.len {
            return Err(DriverError::Invalid);
        }
        let n = self.len;
        out[..n].copy_from_slice(&self.frame[..n]);
        self.len = 0;
        Ok(n)
    }
}
