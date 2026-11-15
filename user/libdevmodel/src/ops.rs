//! 能力接口（ops）—— 驱动**对外能做什么**。
//!
//! 每个功能类一条 trait：音频 [`AudioOps`]、输入 [`InputOps`]、无线 [`WlanOps`]。
//! 驱动在自己的结构体上实现它们，服务层只按 trait 编程；**厂商专有驱动只要实现同一条
//! trait 就能替换通用驱动**，上层（shell / netstack / 混音器）不需要改代码。
//!
//! 所有方法都只用固定容量缓冲区，不依赖堆 —— 与 MorionOS 用户态环境一致。

use crate::device::DriverError;

// ============================================================
// 音频
// ============================================================

/// PCM 输出格式（当前只支持整数 PCM；`bits` 指样本位宽）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AudioFormat {
    pub channels: u8,
    pub rate: u32,
    pub bits: u8,
}

impl AudioFormat {
    /// 最常见的桌面格式：16 bit / 48 kHz / 双声道。
    pub const S16_48K_STEREO: Self = Self {
        channels: 2,
        rate: 48_000,
        bits: 16,
    };
    /// 电话/语音格式：16 bit / 16 kHz / 单声道。
    pub const S16_16K_MONO: Self = Self {
        channels: 1,
        rate: 16_000,
        bits: 16,
    };
}

/// PCM 播放能力。样本为**本机字节序**的 16 位（x86 即小端）。
pub trait AudioOps {
    /// 控制器/编解码器名字（用于日志与注册表）。
    fn name(&self) -> &'static str;
    /// 支持的格式列表（静态表）。
    fn formats(&self) -> &'static [AudioFormat];
    /// 按 `fmt` 打开一条输出流。重复打开返回 [`DriverError::BadState`]。
    fn open(&mut self, fmt: AudioFormat) -> Result<(), DriverError>;
    /// 写入 PCM 样本，返回**实际接收**的样本数（允许短写，调用方续写）。
    fn write_pcm(&mut self, samples: &[i16]) -> Result<usize, DriverError>;
    /// 等待已写入的样本播完（DMA 指针追平）。
    fn drain(&mut self) -> Result<(), DriverError>;
    /// 关闭输出流（可重复调用）。
    fn close(&mut self);
    /// 当前左右声道音量（0..=100）。
    fn volume(&self) -> (u8, u8);
    /// 设置左右声道音量（0..=100，超界按上限截断）。
    fn set_volume(&mut self, left: u8, right: u8) -> Result<(), DriverError>;
}

// ============================================================
// 输入
// ============================================================

/// 按键码（对齐 Linux evdev 的 `KEY_*`，便于将来接 xHCI HID / I2C-HID）。
pub mod key {
    pub const ESC: u16 = 1;
    pub const BACKSPACE: u16 = 14;
    pub const TAB: u16 = 15;
    pub const ENTER: u16 = 28;
    pub const LEFT_CTRL: u16 = 29;
    pub const LEFT_SHIFT: u16 = 42;
    pub const LEFT_ALT: u16 = 56;
    pub const SPACE: u16 = 57;
    pub const RIGHT_CTRL: u16 = 97;
    pub const RIGHT_SHIFT: u16 = 54;
    pub const RIGHT_ALT: u16 = 100;
    pub const UP: u16 = 103;
    pub const LEFT: u16 = 105;
    pub const RIGHT: u16 = 106;
    pub const DOWN: u16 = 108;
    pub const DELETE: u16 = 111;
    /// 字母 `a`..`z` 的起点（Linux `KEY_A = 30`）。
    pub const A: u16 = 30;
    /// 数字 `1`..`0` 的起点（Linux `KEY_1 = 2`）。
    pub const NUM_1: u16 = 2;
}

/// 鼠标按键编号。
pub const BUTTON_LEFT: u8 = 0;
pub const BUTTON_RIGHT: u8 = 1;
pub const BUTTON_MIDDLE: u8 = 2;

/// 一条归一化输入事件。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputEvent {
    /// 键盘按键；`code` 见 [`key`]。
    Key { code: u16, pressed: bool },
    /// 鼠标按键；`button` 见 `BUTTON_*`。
    Button { button: u8, pressed: bool },
    /// 相对位移（鼠标/触摸板移动）。
    Motion { dx: i16, dy: i16 },
    /// 滚轮/双指滚动（正方向 = 下/右）。
    Wheel { dx: i8, dy: i8 },
    /// 绝对坐标（触摸屏；`x`/`y` 由驱动按自身分辨率归一化前的原始值）。
    Touch {
        id: u8,
        x: u16,
        y: u16,
        pressure: u8,
    },
}

/// 输入设备能力。调用方轮询 [`InputOps::poll`] 直到返回 0。
pub trait InputOps {
    /// 设备名（如 `"PS/2 鼠标"`、`"I2C-HID 触摸板"`）。
    fn name(&self) -> &'static str;
    /// 取事件填入 `out`，返回写入条数（0 = 当前无事件）。
    fn poll(&mut self, out: &mut [InputEvent]) -> Result<usize, DriverError>;
}

// ============================================================
// 无线
// ============================================================

/// 48 位 MAC 地址。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    /// 全零地址（尚未从硬件读出）。
    pub const ZERO: Self = Self([0; 6]);

    /// 是否是可用于收发的单播地址（非全零、非广播、非组播）。
    pub fn is_unicast(&self) -> bool {
        let b = self.0;
        b != [0; 6] && b != [0xFF; 6] && b[0] & 0x01 == 0
    }
}

/// SSID：最长 32 字节，不保证 UTF-8。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ssid {
    len: u8,
    bytes: [u8; 32],
}

impl Ssid {
    /// 从字节构造；长度 1..=32，否则 [`DriverError::Invalid`]。
    pub fn new(bytes: &[u8]) -> Result<Self, DriverError> {
        if bytes.is_empty() || bytes.len() > 32 {
            return Err(DriverError::Invalid);
        }
        let mut b = [0u8; 32];
        b[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            len: bytes.len() as u8,
            bytes: b,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// SSID 是否为空。构造时已拒绝空 SSID，故恒为 `false`；保留它是为了让 `len()`
    /// 符合惯用约定（clippy `len_without_is_empty`）。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// 加密方式（当前只做识别，WPA 握手在服务层）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Security {
    Open,
    Wpa2Psk,
    Wpa3Sae,
    Unknown,
}

/// 一次扫描得到的一个 BSS。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BssEntry {
    pub bssid: MacAddr,
    pub ssid: Ssid,
    pub channel: u8,
    /// 信号强度（dBm，通常 -30..=-95）。
    pub rssi: i8,
    pub security: Security,
}

impl BssEntry {
    /// 空条目，供驱动填充固定容量数组。
    pub const EMPTY: Self = Self {
        bssid: MacAddr::ZERO,
        ssid: Ssid {
            len: 0,
            bytes: [0; 32],
        },
        channel: 0,
        rssi: 0,
        security: Security::Unknown,
    };
}

/// 链路状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkState {
    Down,
    Scanning,
    Associating,
    Linked { bssid: MacAddr },
    Failed(DriverError),
}

/// 固件加载器 —— 厂商驱动从文件系统/内置镜像取固件时使用。
///
/// 通用驱动**不假设固件来源**：服务层把 FAT32/initramfs 读文件实现成这个 trait，
/// 传给厂商驱动即可。
pub trait FirmwareLoader {
    /// 读取固件 `name` 到 `out`，返回字节数；放不下返回 [`DriverError::Invalid`]。
    fn load(&mut self, name: &str, out: &mut [u8]) -> Result<usize, DriverError>;
}

/// 无线 station 能力。
pub trait WlanOps {
    /// 设备 MAC（未就绪时返回 [`MacAddr::ZERO`]）。
    fn mac(&self) -> MacAddr;
    /// 主动扫描，把结果填入 `out`，返回条数。
    fn scan(&mut self, out: &mut [BssEntry]) -> Result<usize, DriverError>;
    /// 关联到 `ssid`；`passphrase` 为空表示开放网络。
    fn join(&mut self, ssid: &Ssid, passphrase: &[u8]) -> Result<(), DriverError>;
    /// 断开当前连接。
    fn leave(&mut self) -> Result<(), DriverError>;
    /// 当前链路状态。
    fn state(&self) -> LinkState;
    /// 发送一帧 802.11（已封装，含 MAC 头）。
    fn tx(&mut self, frame: &[u8]) -> Result<(), DriverError>;
    /// 收一帧 802.11，返回字节数（0 = 当前无帧）。
    fn rx(&mut self, out: &mut [u8]) -> Result<usize, DriverError>;
}
