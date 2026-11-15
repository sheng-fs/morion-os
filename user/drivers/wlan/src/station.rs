//! 通用 station 层：状态机 + 扫描缓存 + 统计。
//!
//! [`WlanStation`] 把 [`WlanHw`] 的硬件动作组织成 `libdevmodel::WlanOps` 语义：
//!
//! - **状态机**：`Down → Scanning → Down（扫描完成）→ Associating → Linked / Failed`；
//!   非法顺序返回 [`DriverError::BadState`]；
//! - **扫描缓存**：固定容量 `[BssEntry; N]`，按 BSSID 去重（新结果覆盖 `RSSI`）；
//!   容量满时用"RSSI 更强者替换最弱者"策略，被丢弃的条数记入统计；
//! - **join 校验**：必须已完整扫描过一次；口令为空 = 开放网络，否则长度 8..=63；
//! - **统计**：扫描轮数、丢弃数、收发帧数、错误数，供诊断。

use core::cmp::min;

use libdevmodel::{BssEntry, DriverError, LinkState, MacAddr, Ssid, WlanOps};

use crate::hal::WlanHw;

/// 单次 `poll_scan` 的内部批量大小。
const SCAN_CHUNK: usize = 8;

/// 单次 `scan()` 最多轮询 HAL 的批次数，避免 HAL 永不返回短读时死循环。
const MAX_POLL_ROUNDS: usize = 16;

/// 固定容量扫描缓存，按 BSSID 去重。
///
/// 插入策略：
/// - BSSID 已存在 → 覆盖旧条目（新 RSSI/SSID 生效）；
/// - 未满 → 追加；
/// - 已满 → 若新条目 RSSI **严格大于**当前最弱条目则替换它，否则丢弃新条目。
///   信号强度相同也丢弃新条目，保证缓存内容稳定可预测。
pub struct ScanCache<const N: usize> {
    entries: [BssEntry; N],
    len: usize,
}

impl<const N: usize> ScanCache<N> {
    /// 空缓存。
    pub const fn new() -> Self {
        Self {
            entries: [BssEntry::EMPTY; N],
            len: 0,
        }
    }

    /// 清空（保留已分配数组）。
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// 已缓存条数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 已缓存的条目。
    pub fn entries(&self) -> &[BssEntry] {
        &self.entries[..self.len]
    }

    /// 插入一条扫描结果，返回是否被保留（`false` = 因容量策略丢弃）。
    pub fn insert(&mut self, entry: BssEntry) -> bool {
        if let Some(i) = self.position(entry.bssid) {
            self.entries[i] = entry;
            return true;
        }
        if self.len < N {
            self.entries[self.len] = entry;
            self.len += 1;
            return true;
        }
        if N == 0 {
            return false;
        }
        // 已满：找 RSSI 最弱的槽（RSSI 越小信号越弱）。
        let mut weakest = 0;
        let mut i = 1;
        while i < self.len {
            if self.entries[i].rssi < self.entries[weakest].rssi {
                weakest = i;
            }
            i += 1;
        }
        if entry.rssi > self.entries[weakest].rssi {
            self.entries[weakest] = entry;
            return true;
        }
        false
    }

    fn position(&self, bssid: MacAddr) -> Option<usize> {
        let mut i = 0;
        while i < self.len {
            if self.entries[i].bssid == bssid {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}

impl<const N: usize> Default for ScanCache<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// 诊断计数器（饱和相加，不会回绕）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WlanStats {
    /// 启动扫描的次数。
    pub scan_rounds: u32,
    /// 因缓存已满且新条目不强于最弱者而被直接丢弃的扫描条数
    /// （替换最弱者的条目不计入）。
    pub scan_dropped: u32,
    /// 成功取出的接收帧数。
    pub rx_frames: u32,
    /// 成功发出的发送帧数。
    pub tx_frames: u32,
    /// 收帧失败次数。
    pub rx_errors: u32,
    /// 发帧失败次数。
    pub tx_errors: u32,
}

/// 通用 station 层：状态机 + 扫描缓存 + 帧收发。
///
/// `H` 是厂商 HAL，`N` 是扫描缓存容量（BSS 条数）。结构体不依赖堆。
pub struct WlanStation<H: WlanHw, const N: usize> {
    hw: H,
    state: LinkState,
    /// 是否已经完整扫描过一次（`join` 的前置条件）。
    scanned: bool,
    cache: ScanCache<N>,
    stats: WlanStats,
}

impl<H: WlanHw, const N: usize> WlanStation<H, N> {
    /// 用已就绪的 HAL 构造 station（状态 `Down`）。
    pub fn new(hw: H) -> Self {
        Self {
            hw,
            state: LinkState::Down,
            scanned: false,
            cache: ScanCache::new(),
            stats: WlanStats::default(),
        }
    }

    /// 访问底层 HAL（固件加载、厂商私有诊断等）。
    pub fn hw(&mut self) -> &mut H {
        &mut self.hw
    }

    /// 拆出底层 HAL。
    pub fn into_hw(self) -> H {
        self.hw
    }

    /// 当前统计快照。
    pub fn stats(&self) -> WlanStats {
        self.stats
    }

    /// 扫描缓存。
    pub fn cache(&self) -> &ScanCache<N> {
        &self.cache
    }

    /// 是否已完整扫描过（`join` 前置条件）。
    pub fn is_scanned(&self) -> bool {
        self.scanned
    }

    /// 把 HAL 的扫描结果抽干进缓存，返回本轮是否已结束。
    fn drain_scan(&mut self) -> Result<bool, DriverError> {
        let mut chunk = [BssEntry::EMPTY; SCAN_CHUNK];
        let mut rounds = 0;
        while rounds < MAX_POLL_ROUNDS {
            let n = self.hw.poll_scan(&mut chunk)?;
            if n > chunk.len() {
                return Err(DriverError::Invalid);
            }
            let mut i = 0;
            while i < n {
                if !self.cache.insert(chunk[i]) {
                    self.stats.scan_dropped = self.stats.scan_dropped.saturating_add(1);
                }
                i += 1;
            }
            // 短读（含 0）= 本批已取完，交给 scan_done 判断整体是否结束。
            if n < chunk.len() {
                break;
            }
            rounds += 1;
        }
        Ok(self.hw.scan_done())
    }
}

impl<H: WlanHw, const N: usize> WlanOps for WlanStation<H, N> {
    fn mac(&self) -> MacAddr {
        self.hw.mac()
    }

    /// 驱动扫描。
    ///
    /// 首次调用会启动扫描并清空旧缓存；若 HAL 增量上报，本次调用可能只返回部分结果
    /// 且状态仍为 `Scanning`，调用方稍后再调一次即可。扫描结束后状态回到 `Down`，
    /// `is_scanned()` 变为 `true`。
    ///
    /// 结果按缓存顺序写入 `out`，`out` 不足时截断（返回值为实际写入条数）。
    fn scan(&mut self, out: &mut [BssEntry]) -> Result<usize, DriverError> {
        match self.state {
            LinkState::Scanning => {}
            LinkState::Down | LinkState::Failed(_) => {
                self.hw.start_scan()?;
                self.cache.clear();
                self.scanned = false;
                self.stats.scan_rounds = self.stats.scan_rounds.saturating_add(1);
                self.state = LinkState::Scanning;
            }
            // 关联中/已关联不允许扫描，保持状态机简单可预测。
            LinkState::Associating | LinkState::Linked { .. } => {
                return Err(DriverError::BadState);
            }
        }

        if self.drain_scan()? {
            self.state = LinkState::Down;
            self.scanned = true;
        }

        let n = min(out.len(), self.cache.len());
        out[..n].copy_from_slice(&self.cache.entries()[..n]);
        Ok(n)
    }

    /// 关联。
    ///
    /// 前置条件：已完整扫描过且当前不是关联中/已关联。
    /// `passphrase` 为空表示开放网络，否则必须是 8..=63 字节的 WPA2-PSK 口令。
    /// 口令只交给 HAL，**四次握手由 HAL 或上层 supplicant 完成**。
    fn join(&mut self, ssid: &Ssid, passphrase: &[u8]) -> Result<(), DriverError> {
        if !self.scanned {
            return Err(DriverError::BadState);
        }
        match self.state {
            LinkState::Down | LinkState::Failed(_) => {}
            _ => return Err(DriverError::BadState),
        }

        let pass_len = passphrase.len();
        if pass_len != 0 && !(8..=63).contains(&pass_len) {
            return Err(DriverError::Invalid);
        }

        self.state = LinkState::Associating;
        if let Err(e) = self.hw.associate(ssid, passphrase) {
            self.state = LinkState::Failed(e);
            return Err(e);
        }

        // 以 HAL 报告的链路状态为准；HAL 说没连上就不算成功。
        match self.hw.link_state() {
            LinkState::Linked { bssid } => {
                self.state = LinkState::Linked { bssid };
                Ok(())
            }
            LinkState::Failed(e) => {
                self.state = LinkState::Failed(e);
                Err(e)
            }
            _ => {
                self.state = LinkState::Failed(DriverError::BadState);
                Err(DriverError::BadState)
            }
        }
    }

    /// 断开连接。`Down` 状态下重复调用返回 `Ok(())`。
    fn leave(&mut self) -> Result<(), DriverError> {
        if self.state == LinkState::Down {
            return Ok(());
        }
        match self.hw.disconnect() {
            Ok(()) => {
                self.state = LinkState::Down;
                Ok(())
            }
            Err(e) => {
                self.state = LinkState::Failed(e);
                Err(e)
            }
        }
    }

    /// 当前链路状态。
    fn state(&self) -> LinkState {
        self.state
    }

    /// 发送一帧 802.11。必须先关联成功。
    fn tx(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        if !matches!(self.state, LinkState::Linked { .. }) {
            return Err(DriverError::BadState);
        }
        if frame.is_empty() {
            return Err(DriverError::Invalid);
        }
        match self.hw.tx_frame(frame) {
            Ok(()) => {
                self.stats.tx_frames = self.stats.tx_frames.saturating_add(1);
                Ok(())
            }
            Err(e) => {
                self.stats.tx_errors = self.stats.tx_errors.saturating_add(1);
                Err(e)
            }
        }
    }

    /// 收一帧 802.11，返回字节数（`0` = 当前无帧）。
    fn rx(&mut self, out: &mut [u8]) -> Result<usize, DriverError> {
        match self.hw.rx_frame(out) {
            Ok(n) if n > out.len() => {
                self.stats.rx_errors = self.stats.rx_errors.saturating_add(1);
                Err(DriverError::Invalid)
            }
            Ok(n) => {
                if n > 0 {
                    self.stats.rx_frames = self.stats.rx_frames.saturating_add(1);
                }
                Ok(n)
            }
            Err(e) => {
                self.stats.rx_errors = self.stats.rx_errors.saturating_add(1);
                Err(e)
            }
        }
    }
}
