//! PS/2 鼠标（i8042 AUX 口）驱动核心。
//!
//! 只处理**鼠标**字节：PS/2 键盘仍由现有的 `user/srv` 键盘服务（`kbd.rs`）负责，本 crate
//! 不抢 IRQ1、不读键盘数据，避免两个进程抢同一个 0x60 数据口。轮询时若状态口的 AUX 位为 0
//! 就立刻停手，把键盘字节留在输出缓冲里。
//!
//! # 分层
//!
//! - [`Ps2Port`]：**传输层契约**。服务层用内核 syscall `sys_port_in8` / `sys_port_out8`
//!   实现它；库本身不依赖 `morion`，于是能在宿主机单测。
//! - [`Ps2MouseDecoder`]：纯字节流解码器（3 字节标准包 / 4 字节 IntelliMouse 滚轮包），
//!   不碰硬件，可在单测里直接喂字节。
//! - [`init`]：i8042 初始化序列（启用 AUX、开 IRQ12、清时钟禁止位、0xF6/0xF4、ACK 重试）。
//! - [`read_poll_byte`]：从状态口/数据口取一个 AUX 字节，供轮询模式使用；
//!   IRQ12 路径由服务层读走字节后调用 [`Ps2MouseDecoder::feed`] 即可（无需改动核心）。
//!
//! # 与键盘服务的边界
//!
//! `kbd.rs` 目前也会读 0x64/0x60，但显式丢弃 AUX 字节；本驱动只读 AUX 字节、遇到非 AUX
//! 字节就退出。两者可并存（一个进程读 IRQ1、一个轮询 AUX），前提是服务层把 AUX 数据路由到
//! 本驱动而不是键盘进程。

use libdevmodel::ops::{BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT};
use libdevmodel::{DriverError, InputEvent};

// ============================================================
// 传输层
// ============================================================

/// i8042 端口访问契约。
///
/// 服务层用 `sys_port_in8` / `sys_port_out8` 实现；测试用假端口实现。核心代码只经由它
/// 访问硬件，因此同一份解码/初始化逻辑既能进服务进程，也能在宿主机单测。
pub trait Ps2Port {
    /// 读状态口（0x64）。bit0 = OBF（有字节可读），bit5 = AUX（来自鼠标口）。
    fn status(&mut self) -> u8;
    /// 读数据口（0x60）。调用前应先确认 `status()` 的 OBF 位。
    fn read_data(&mut self) -> u8;
    /// 写数据口（0x60）。命令字节之后的参数字节也走这里。
    fn write_data(&mut self, b: u8) -> Result<(), DriverError>;
    /// 写命令口（0x64）。
    fn write_cmd(&mut self, b: u8) -> Result<(), DriverError>;
}

/// i8042 状态口。
pub const PS2_STATUS_PORT: u16 = 0x64;
/// i8042 数据口。
pub const PS2_DATA_PORT: u16 = 0x60;
/// 状态口 bit0：输出缓冲满。
pub const STATUS_OBF: u8 = 0x01;
/// 状态口 bit5：字节来自 AUX（鼠标）口。
pub const STATUS_AUX: u8 = 0x20;

/// 读命令字节。
pub const CMD_READ_CONFIG: u8 = 0x20;
/// 写命令字节。
pub const CMD_WRITE_CONFIG: u8 = 0x60;
/// 启用 AUX 口。
pub const CMD_ENABLE_AUX: u8 = 0xA8;
/// 下一个数据字节发往 AUX 口。
pub const CMD_WRITE_AUX: u8 = 0xD4;

/// 命令字节 bit1：启用 IRQ12。
pub const CFG_IRQ12: u8 = 0x02;
/// 命令字节 bit5：禁止 AUX 时钟（置 1 = 关）。初始化必须清零。
pub const CFG_AUX_CLOCK_OFF: u8 = 0x20;

/// 设备 ACK。
pub const ACK: u8 = 0xFA;
/// 设备要求重发上一条命令。
pub const RESEND: u8 = 0xFE;

/// 设为默认参数（采样率、分辨率、缩放）。
pub const CMD_SET_DEFAULTS: u8 = 0xF6;
/// 打开数据上报。
pub const CMD_ENABLE_REPORTING: u8 = 0xF4;
/// 关闭数据上报。
pub const CMD_DISABLE_REPORTING: u8 = 0xF5;
/// 设置采样率（后跟一个参数字节）。
pub const CMD_SET_SAMPLE_RATE: u8 = 0xF3;
/// 读设备 ID（IntelliMouse 返回 0x03）。
pub const CMD_GET_DEVICE_ID: u8 = 0xF2;

/// IntelliMouse 探测用的 magic 采样率序列。
const WHEEL_RATES: [u8; 3] = [200, 100, 80];

/// PS/2 指针设备类型（由 `GET_DEVICE_ID` 判定）。
///
/// 三种是业界事实标准：`0x00` 标准三键、`0x03` IntelliMouse（滚轮）、`0x04` 4D/五键。
/// 触摸板/小红点在 PS/2 上通常也回 `0x00` 后再用厂商私有命令扩展，这里按标准处理。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ps2MouseKind {
    /// 标准三键鼠标（3 字节包）。
    Standard,
    /// IntelliMouse（4 字节包，带滚轮）。
    IntelliMouse,
    /// 4D 鼠标（五键 + 滚轮）。
    FourD,
    /// 未识别或未查询（原始 ID 保留）。
    Unknown(u8),
}

impl Ps2MouseKind {
    /// 从 `GET_DEVICE_ID` 的返回值归类。
    pub fn from_id(id: u8) -> Self {
        match id {
            0x00 => Self::Standard,
            0x03 => Self::IntelliMouse,
            0x04 => Self::FourD,
            other => Self::Unknown(other),
        }
    }

    /// 可读名称（日志/诊断）。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Standard => "标准三键鼠标",
            Self::IntelliMouse => "IntelliMouse（滚轮）",
            Self::FourD => "4D 鼠标（五键+滚轮）",
            Self::Unknown(_) => "PS/2 指针设备",
        }
    }

    /// 是否按 4 字节包解析（滚轮）。
    pub fn wheel(&self) -> bool {
        matches!(self, Self::IntelliMouse | Self::FourD)
    }

    /// 按键数（标准 3、IntelliMouse 3、4D 5）。
    pub fn buttons(&self) -> u8 {
        match self {
            Self::FourD => 5,
            _ => 3,
        }
    }
}

/// 单次读状态口最多自旋次数（没有中断时靠它兜底，避免死等）。
const READ_SPINS: u32 = 100_000;
/// 一条 AUX 命令最多重发次数（应对 0xFE resend）。
const AUX_RETRIES: u32 = 3;
/// 等待 ACK 时最多跳过的杂散字节数。
const ACK_MAX_BYTES: u32 = 16;

// ============================================================
// 解码器
// ============================================================

/// 一个解好的鼠标包。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MousePacket {
    /// 按键位（bit0 左、bit1 右、bit2 中）。
    pub buttons: u8,
    /// 相对位移（屏幕坐标：右/下为正）。
    pub dx: i16,
    pub dy: i16,
    /// 滚轮格数（正 = 下滚）。
    pub wheel: i8,
}

/// 一个包最多产生的归一化事件数：3 个按键跳变 + 1 条位移 + 1 条滚轮。
pub const MAX_EVENTS_PER_PACKET: usize = 5;

/// 一个数据包解码出的事件集合（固定容量，不分配）。
#[derive(Clone, Copy)]
pub struct PacketEvents {
    buf: [InputEvent; MAX_EVENTS_PER_PACKET],
    len: usize,
}

impl PacketEvents {
    pub const fn new() -> Self {
        Self {
            buf: [InputEvent::Motion { dx: 0, dy: 0 }; MAX_EVENTS_PER_PACKET],
            len: 0,
        }
    }

    pub fn as_slice(&self) -> &[InputEvent] {
        &self.buf[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    fn push(&mut self, ev: InputEvent) {
        if self.len < self.buf.len() {
            self.buf[self.len] = ev;
            self.len += 1;
        }
    }
}

impl Default for PacketEvents {
    fn default() -> Self {
        Self::new()
    }
}

/// PS/2 鼠标字节流解码器。
///
/// 标准包 3 字节：
///
/// ```text
/// byte0: Y溢出 X溢出 Y符号 X符号 恒1 中键 右键 左键
/// byte1: X 位移（8 位，符号由 byte0.bit4 给出）
/// byte2: Y 位移（8 位，符号由 byte0.bit5 给出）
/// ```
///
/// IntelliMouse 在第 4 字节低 4 位给出滚轮（有符号 4 位）。
pub struct Ps2MouseDecoder {
    buf: [u8; 4],
    len: usize,
    wheel: bool,
    buttons: u8,
    dropped: u32,
}

impl Ps2MouseDecoder {
    pub const fn new() -> Self {
        Self {
            buf: [0; 4],
            len: 0,
            wheel: false,
            buttons: 0,
            dropped: 0,
        }
    }

    /// 是否按 4 字节 IntelliMouse 包解析。
    pub fn set_wheel(&mut self, enabled: bool) {
        if self.wheel != enabled {
            self.reset();
        }
        self.wheel = enabled;
    }

    pub fn wheel(&self) -> bool {
        self.wheel
    }

    /// 当前包需要多少字节。
    pub fn bytes_needed(&self) -> usize {
        if self.wheel {
            4
        } else {
            3
        }
    }

    /// 累计丢弃的包数（同步位错位、溢出包）。
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// 清空半包状态与按键记录（`stop`/重新初始化时调用）。
    pub fn reset(&mut self) {
        self.len = 0;
        self.buttons = 0;
    }

    /// 喂入一个字节。凑满一个包且校验通过时返回 `true`，事件追加到 `out`。
    ///
    /// 丢弃策略：
    /// - 包首 bit3 不为 1 → 同步错位，丢弃该字节并计数；
    /// - X/Y 溢出位置位 → 整个包丢弃（避免光标瞬移），并计数。
    pub fn feed(&mut self, byte: u8, out: &mut PacketEvents) -> bool {
        if self.len == 0 {
            // 包首必须带 bit3（恒为 1）。不满足说明字节流错位。
            if byte & 0x08 == 0 {
                self.dropped += 1;
                return false;
            }
        }
        self.buf[self.len] = byte;
        self.len += 1;
        if self.len < self.bytes_needed() {
            return false;
        }
        self.len = 0;

        let b0 = self.buf[0];
        if b0 & 0xC0 != 0 {
            // bit6 = X 溢出，bit7 = Y 溢出。整包丢弃。
            self.dropped += 1;
            return false;
        }

        let dx = if b0 & 0x10 != 0 {
            self.buf[1] as i16 - 256
        } else {
            self.buf[1] as i16
        };
        // PS/2 的 Y 向上为正；归一化事件统一为屏幕坐标（下为正）。
        let dy_raw = if b0 & 0x20 != 0 {
            self.buf[2] as i16 - 256
        } else {
            self.buf[2] as i16
        };
        let dy = -dy_raw;

        out.clear();
        let buttons = b0 & 0x07;
        let changed = buttons ^ self.buttons;
        for (bit, button) in [
            (0x01u8, BUTTON_LEFT),
            (0x02, BUTTON_RIGHT),
            (0x04, BUTTON_MIDDLE),
        ] {
            if changed & bit != 0 {
                out.push(InputEvent::Button {
                    button,
                    pressed: buttons & bit != 0,
                });
            }
        }
        self.buttons = buttons;

        if dx != 0 || dy != 0 {
            out.push(InputEvent::Motion { dx, dy });
        }

        if self.wheel {
            let nib = self.buf[3] & 0x0F;
            let w = if nib & 0x08 != 0 {
                nib as i8 - 16
            } else {
                nib as i8
            };
            if w != 0 {
                // 滚轮向上为正，归一化事件约定正方向 = 下滚。
                out.push(InputEvent::Wheel { dx: 0, dy: -w });
            }
        }

        true
    }
}

impl Default for Ps2MouseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================
// 初始化 / 轮询
// ============================================================

/// 初始化 i8042 的 AUX 口并打开鼠标上报。
///
/// `probe_wheel = true` 时用采样率 200/100/80 + GET_ID 探测 IntelliMouse 4 字节包；
/// 探测失败（老设备不响应）不会让整个初始化失败，只是退回 3 字节包。
///
/// 返回是否启用滚轮包。
pub fn init<P: Ps2Port>(port: &mut P, probe_wheel: bool) -> Result<bool, DriverError> {
    // 上电残留（自检结果、上一条 ACK）先清干净，否则后面会错位。
    flush(port);

    // 1. 启用 AUX 口。
    port.write_cmd(CMD_ENABLE_AUX)
        .map_err(|_| DriverError::Other("ps2: enable-aux"))?;
    flush(port);

    // 2. 命令字节（尽力而为）：开 IRQ12、清 AUX 时钟禁止位。
    //
    //    注意：读配置字节（0x20）的响应是 **AUX=0** 的普通输出字节，内核的 IRQ1
    //    处理器会把它当键盘数据读走（它无法区分控制器响应与键盘字节）。因此这里
    //    允许失败并继续：固件通常已使能 AUX 时钟，鼠标命令（下面 0xF6/0xF4）的
    //    ACK 都带 AUX 标志，不会被 IRQ1 抢走，能真正验证设备在线。
    port.write_cmd(CMD_READ_CONFIG)
        .map_err(|_| DriverError::Other("ps2: read-config-cmd"))?;
    match read_byte(port) {
        Ok(cfg) => {
            let cfg = (cfg | CFG_IRQ12) & !CFG_AUX_CLOCK_OFF;
            port.write_cmd(CMD_WRITE_CONFIG)
                .map_err(|_| DriverError::Other("ps2: write-config-cmd"))?;
            port.write_data(cfg)
                .map_err(|_| DriverError::Other("ps2: write-config-byte"))?;
        }
        // 配置字节被 IRQ1 抢走 / 固件不响应：保持现有配置继续。
        // 没有 i8042（status=0xFF）则仍按 NotFound 上报，不把"控制器不存在"吞掉。
        Err(e) => {
            if e == DriverError::NotFound {
                return Err(e);
            }
            flush(port);
        }
    }
    flush(port);

    // 3. 可选：IntelliMouse 探测（magic 采样率序列）。
    let mut wheel = false;
    if probe_wheel {
        match probe_wheel_mode(port) {
            Ok(true) => wheel = true,
            Ok(false) => {}
            // 设备不支持/超时：回到默认参数继续，不影响基本鼠标功能。
            Err(_) => flush(port),
        }
    }

    // 4. 设为默认参数，再打开上报。
    aux_command(port, CMD_SET_DEFAULTS).map_err(|_| DriverError::Other("ps2: set-defaults"))?;
    aux_command(port, CMD_ENABLE_REPORTING)
        .map_err(|_| DriverError::Other("ps2: enable-reporting"))?;

    // 5. 清掉 ACK 之后可能残留的字节，避免第一包被污染。
    flush(port);
    Ok(wheel)
}

/// 关闭数据上报（`stop` 时尽力而为）。
pub fn disable<P: Ps2Port>(port: &mut P) -> Result<(), DriverError> {
    aux_command(port, CMD_DISABLE_REPORTING)
}

/// 读取 PS/2 指针设备 ID（`0x00` 标准 / `0x03` IntelliMouse / `0x04` 4D）。
///
/// 必须在 `init` 之后调用（设备已打开上报）；返回原始 ID，分类交给
/// [`Ps2MouseKind::from_id`]。
pub fn read_device_id<P: Ps2Port>(port: &mut P) -> Result<u8, DriverError> {
    aux_command(port, CMD_GET_DEVICE_ID)?;
    read_byte(port)
}

/// 从 i8042 取一个**来自 AUX** 的字节。没有可读字节（或下一个字节属于键盘）返回
/// `Ok(None)`，键盘字节保持原样留给键盘服务。
pub fn read_poll_byte<P: Ps2Port>(port: &mut P) -> Result<Option<u8>, DriverError> {
    let st = port.status();
    if st == 0xFF {
        // 没有 i8042 控制器。
        return Err(DriverError::NotFound);
    }
    if st & STATUS_OBF == 0 || st & STATUS_AUX == 0 {
        return Ok(None);
    }
    Ok(Some(port.read_data()))
}

fn probe_wheel_mode<P: Ps2Port>(port: &mut P) -> Result<bool, DriverError> {
    for rate in WHEEL_RATES {
        aux_command(port, CMD_SET_SAMPLE_RATE)?;
        aux_command(port, rate)?;
    }
    aux_command(port, CMD_GET_DEVICE_ID)?;
    // GET_ID 的应答紧跟在 ACK 后面：0x00 = 标准鼠标，0x03 = IntelliMouse。
    let id = read_byte(port)?;
    Ok(id == 0x03)
}

/// 读一个字节，最多自旋 [`READ_SPINS`] 次。
fn read_byte<P: Ps2Port>(port: &mut P) -> Result<u8, DriverError> {
    for _ in 0..READ_SPINS {
        let st = port.status();
        if st == 0xFF {
            return Err(DriverError::NotFound);
        }
        if st & STATUS_OBF != 0 {
            return Ok(port.read_data());
        }
    }
    Err(DriverError::Timeout)
}

/// 丢弃输出缓冲里所有残留字节（上限防止设备异常时死循环）。
fn flush<P: Ps2Port>(port: &mut P) {
    for _ in 0..64 {
        let st = port.status();
        if st == 0xFF || st & STATUS_OBF == 0 {
            break;
        }
        let _ = port.read_data();
    }
}

enum Ack {
    Acked,
    Resend,
}

fn wait_ack<P: Ps2Port>(port: &mut P) -> Result<Ack, DriverError> {
    for _ in 0..ACK_MAX_BYTES {
        match read_byte(port)? {
            ACK => return Ok(Ack::Acked),
            RESEND => return Ok(Ack::Resend),
            // 自检结果之类的杂散字节，跳过。
            _ => {}
        }
    }
    Err(DriverError::Timeout)
}

/// 向 AUX 口发一条命令并等 ACK，遇到 0xFE 有限次重发。
fn aux_command<P: Ps2Port>(port: &mut P, cmd: u8) -> Result<(), DriverError> {
    for _ in 0..AUX_RETRIES {
        port.write_cmd(CMD_WRITE_AUX)?;
        port.write_data(cmd)?;
        match wait_ack(port)? {
            Ack::Acked => return Ok(()),
            Ack::Resend => continue,
        }
    }
    Err(DriverError::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 可编程的假 i8042：记录写出的命令/数据，并按命令注入应答字节。
    struct FakePort {
        cmds: Vec<u8>,
        data: Vec<u8>,
        rx: Vec<u8>,
        cfg: u8,
        device_id: u8,
        resends: usize,
        last_cmd: u8,
    }

    impl FakePort {
        fn new() -> Self {
            Self {
                cmds: Vec::new(),
                data: Vec::new(),
                rx: Vec::new(),
                // bit0 = 键盘中断开，bit2 = 键盘时钟开，bit5 = AUX 时钟禁止（待清除）。
                cfg: 0x25,
                device_id: 0x03,
                resends: 0,
                last_cmd: 0,
            }
        }
    }

    impl Ps2Port for FakePort {
        fn status(&mut self) -> u8 {
            if self.rx.is_empty() {
                0
            } else {
                STATUS_OBF | STATUS_AUX
            }
        }

        fn read_data(&mut self) -> u8 {
            if self.rx.is_empty() {
                0
            } else {
                self.rx.remove(0)
            }
        }

        fn write_data(&mut self, b: u8) -> Result<(), DriverError> {
            self.data.push(b);
            match self.last_cmd {
                // 0x60 的参数由 8042 自己消费，不回 ACK。
                CMD_WRITE_CONFIG => {}
                CMD_WRITE_AUX => {
                    if self.resends > 0 {
                        self.resends -= 1;
                        self.rx.push(RESEND);
                    } else {
                        self.rx.push(ACK);
                        if b == CMD_GET_DEVICE_ID {
                            self.rx.push(self.device_id);
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        }

        fn write_cmd(&mut self, b: u8) -> Result<(), DriverError> {
            self.cmds.push(b);
            if b == CMD_READ_CONFIG {
                self.rx.push(self.cfg);
            }
            self.last_cmd = b;
            Ok(())
        }
    }

    fn decode(bytes: &[u8]) -> (Vec<InputEvent>, Ps2MouseDecoder) {
        let mut dec = Ps2MouseDecoder::new();
        let mut evs = PacketEvents::new();
        let mut all = Vec::new();
        for &b in bytes {
            if dec.feed(b, &mut evs) {
                all.extend_from_slice(evs.as_slice());
            }
        }
        (all, dec)
    }

    #[test]
    fn decodes_motion_and_buttons() {
        let mut dec = Ps2MouseDecoder::new();
        let mut evs = PacketEvents::new();
        // 左键按下 + 右移 5、上移 2（PS/2 上为正 → 屏幕 dy = -2）。
        assert!(!dec.feed(0x09, &mut evs));
        assert!(evs.is_empty()); // 半包
        assert!(!dec.feed(0x05, &mut evs));
        assert!(dec.feed(0x02, &mut evs));
        assert_eq!(
            evs.as_slice(),
            &[
                InputEvent::Button {
                    button: BUTTON_LEFT,
                    pressed: true
                },
                InputEvent::Motion { dx: 5, dy: -2 },
            ]
        );

        // 松开左键，无位移。
        assert!(!dec.feed(0x08, &mut evs));
        dec.feed(0x00, &mut evs);
        assert!(dec.feed(0x00, &mut evs));
        assert_eq!(
            evs.as_slice(),
            &[InputEvent::Button {
                button: BUTTON_LEFT,
                pressed: false
            }]
        );
    }

    #[test]
    fn decodes_negative_motion() {
        let (all, dec) = decode(&[0x38, 0xFB, 0xFD]);
        // X 符号位 = 1 → 0xFB - 256 = -5；Y 符号位 = 1 → 0xFD - 256 = -3，
        // 归一化到屏幕坐标后 dy = +3。
        assert_eq!(all, vec![InputEvent::Motion { dx: -5, dy: 3 }]);
        assert_eq!(dec.dropped(), 0);
    }

    #[test]
    fn drops_overflow_and_desync() {
        let mut dec = Ps2MouseDecoder::new();
        let mut evs = PacketEvents::new();
        // X 溢出（bit6）：整包丢弃。
        assert!(!dec.feed(0x48, &mut evs));
        dec.feed(0x10, &mut evs);
        assert!(!dec.feed(0x10, &mut evs));
        assert_eq!(dec.dropped(), 1);
        assert!(evs.is_empty());

        // 同步错位：包首 bit3 = 0，直接丢，随后恢复正常。
        assert!(!dec.feed(0x00, &mut evs));
        assert_eq!(dec.dropped(), 2);
        assert!(!dec.feed(0x08, &mut evs));
        assert!(!dec.feed(0x01, &mut evs));
        assert!(dec.feed(0x00, &mut evs));
        assert_eq!(evs.as_slice(), &[InputEvent::Motion { dx: 1, dy: 0 }]);
    }

    #[test]
    fn decodes_intellimouse_wheel() {
        let mut dec = Ps2MouseDecoder::new();
        dec.set_wheel(true);
        assert_eq!(dec.bytes_needed(), 4);
        let mut evs = PacketEvents::new();
        // 滚轮 +1（上滚）→ 归一化 Wheel.dy = -1。
        assert!(!dec.feed(0x08, &mut evs));
        assert!(!dec.feed(0x00, &mut evs));
        assert!(!dec.feed(0x00, &mut evs));
        assert!(dec.feed(0x01, &mut evs));
        assert_eq!(evs.as_slice(), &[InputEvent::Wheel { dx: 0, dy: -1 }]);

        // 低 4 位 0x0F = -1（下滚）→ Wheel.dy = +1。
        dec.feed(0x08, &mut evs);
        dec.feed(0x00, &mut evs);
        dec.feed(0x00, &mut evs);
        assert!(dec.feed(0x0F, &mut evs));
        assert_eq!(evs.as_slice(), &[InputEvent::Wheel { dx: 0, dy: 1 }]);
    }

    #[test]
    fn init_sequence_enables_aux_and_reports_wheel() {
        let mut p = FakePort::new();
        // 上电残留一个字节，初始化应先清掉。
        p.rx.push(0xAA);
        let wheel = init(&mut p, true).unwrap();
        assert!(wheel);

        assert_eq!(p.cmds[0], CMD_ENABLE_AUX);
        assert!(p.cmds.contains(&CMD_READ_CONFIG));
        assert!(p.cmds.contains(&CMD_WRITE_CONFIG));
        // 0x25 → 清 bit5 并置 bit1 = 0x07。
        assert!(p.data.contains(&0x07));
        // 滚轮 magic 序列 + GET_ID + 默认参数 + 开上报。
        for rate in WHEEL_RATES {
            assert!(p.data.contains(&rate));
        }
        assert_eq!(
            p.data.iter().filter(|b| **b == CMD_SET_SAMPLE_RATE).count(),
            3
        );
        assert!(p.data.contains(&CMD_GET_DEVICE_ID));
        assert!(p.data.contains(&CMD_SET_DEFAULTS));
        assert!(p.data.contains(&CMD_ENABLE_REPORTING));
        // 初始化结束时输出缓冲应为空。
        assert!(p.rx.is_empty(), "residual bytes: {:?}", p.rx);
    }

    #[test]
    fn init_retries_on_resend() {
        let mut p = FakePort::new();
        p.resends = 1; // 第一条 AUX 命令先收到 0xFE
        assert!(init(&mut p, true).unwrap());
        // 首个 0xF3 被重发一次 → 采样率命令共 4 次（3 个 magic + 1 次重发）。
        assert_eq!(
            p.data.iter().filter(|b| **b == CMD_SET_SAMPLE_RATE).count(),
            4
        );
        assert!(p.rx.is_empty());
    }

    #[test]
    fn init_without_controller_reports_not_found() {
        struct NoController;
        impl Ps2Port for NoController {
            fn status(&mut self) -> u8 {
                0xFF
            }
            fn read_data(&mut self) -> u8 {
                0xFF
            }
            fn write_data(&mut self, _b: u8) -> Result<(), DriverError> {
                Ok(())
            }
            fn write_cmd(&mut self, _b: u8) -> Result<(), DriverError> {
                Ok(())
            }
        }
        // flush 遇到 0xFF 直接退出；随后第一次读命令字节就报 NotFound。
        assert_eq!(init(&mut NoController, false), Err(DriverError::NotFound));
    }
}
