//! 驱动封装：把协议层包成 `libdevmodel::Driver` + `InputOps`。
//!
//! - [`Ps2MouseDriver`]：轮询 i8042 AUX 口（也提供 [`Ps2MouseDriver::feed_byte`] 供 IRQ12
//!   路径喂字节）；
//! - [`I2cHidTouchpadDriver`]：I2C-HID 触摸板，支持绝对坐标与鼠标模拟两种模式；
//! - [`InputDriver`]：两者二选一的统一入口，服务层只按 `libdevmodel::Driver` 编程。
//!
//! 本 crate **不接管键盘**：PS/2 键盘仍由现有 `kbd` 服务处理，这里只消费 AUX 字节；
//! `InputEvent::Key` / evdev 风格键码只是为将来统一的 USB HID 输入服务预留。

use libdevmodel::ops::BUTTON_LEFT;
use libdevmodel::{Class, DeviceInfo, Driver, DriverError, InputEvent, InputOps, Transport};

use crate::event_queue::EventQueue;
use crate::i2c_hid::{
    parse_hid_descriptor, parse_report_descriptor, parse_touch_report, HidReportLayout,
    I2cHidTransport, TouchpadMode, HID_DESC_LEN, MAX_INPUT_LEN, MAX_REPORT_DESCRIPTOR,
    REG_HID_DESCRIPTOR, REG_INPUT, REG_POWER,
};
use crate::ps2::{self, PacketEvents, Ps2MouseDecoder, Ps2MouseKind, Ps2Port, MAX_EVENTS_PER_PACKET};

/// 默认事件队列容量。
pub const DEFAULT_QUEUE: usize = 32;

/// 触摸板映射成相对移动时，整幅逻辑范围对应的移动量（像素）。
const MOTION_SPAN: i32 = 512;

// ============================================================
// PS/2 鼠标
// ============================================================

/// PS/2 鼠标驱动。
///
/// 服务层把内核的端口访问 syscall 包成 [`Ps2Port`] 传进来即可；驱动内部轮询 AUX 字节，
/// 不要求中断，也不读键盘字节。
pub struct Ps2MouseDriver<P: Ps2Port> {
    port: P,
    decoder: Ps2MouseDecoder,
    queue: EventQueue<DEFAULT_QUEUE>,
    kind: Ps2MouseKind,
    running: bool,
}

impl<P: Ps2Port> Ps2MouseDriver<P> {
    pub fn new(port: P) -> Self {
        Self {
            port,
            decoder: Ps2MouseDecoder::new(),
            queue: EventQueue::new(),
            kind: Ps2MouseKind::Unknown(0xFF),
            running: false,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// 设备类型（`start` 之后由 `GET_DEVICE_ID` 判定）。
    pub fn kind(&self) -> Ps2MouseKind {
        self.kind
    }

    /// 设备类型名（日志用）。
    pub fn kind_name(&self) -> &'static str {
        self.kind.name()
    }

    /// 是否按 4 字节 IntelliMouse 包解析。
    pub fn wheel_enabled(&self) -> bool {
        self.decoder.wheel()
    }

    /// 传输层访问（诊断/复用端口）。
    pub fn port(&mut self) -> &mut P {
        &mut self.port
    }

    /// 当前积压事件数。
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// 直接喂一个 AUX 字节。IRQ12 路径由服务层在中断上下文读走 0x60 后调用它。
    pub fn feed_byte(&mut self, byte: u8) {
        let mut evs = PacketEvents::new();
        if self.decoder.feed(byte, &mut evs) {
            for ev in evs.as_slice() {
                self.queue.push(*ev);
            }
        }
    }

    /// 从端口取一个 AUX 字节；返回是否取到。
    fn pump(&mut self) -> Result<bool, DriverError> {
        match ps2::read_poll_byte(&mut self.port)? {
            Some(b) => {
                self.feed_byte(b);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

impl<P: Ps2Port> Driver for Ps2MouseDriver<P> {
    fn name(&self) -> &'static str {
        "ps2-mouse"
    }

    fn class(&self) -> Class {
        Class::Input
    }

    fn probe(&mut self, dev: &DeviceInfo) -> Result<bool, DriverError> {
        Ok(matches!(dev.transport, Transport::Ps2))
    }

    fn start(&mut self, dev: &DeviceInfo) -> Result<(), DriverError> {
        if !matches!(dev.transport, Transport::Ps2) {
            return Err(DriverError::NotSupported);
        }
        let wheel = ps2::init(&mut self.port, true)?;
        // GET_DEVICE_ID：区分标准三键 / IntelliMouse / 4D。失败不致命（保留 Unknown）。
        self.kind = match ps2::read_device_id(&mut self.port) {
            Ok(id) => Ps2MouseKind::from_id(id),
            Err(_) => Ps2MouseKind::Unknown(0xFF),
        };
        self.decoder.reset();
        self.decoder.set_wheel(wheel);
        self.queue.clear();
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) {
        if self.running {
            // 尽力关闭上报；端口可能已经不可用，失败忽略。
            let _ = ps2::disable(&mut self.port);
        }
        self.running = false;
        self.decoder.reset();
        self.queue.clear();
    }

    fn input(&mut self) -> Option<&mut dyn InputOps> {
        Some(self)
    }
}

impl<P: Ps2Port> InputOps for Ps2MouseDriver<P> {
    fn name(&self) -> &'static str {
        "PS/2 鼠标"
    }

    fn poll(&mut self, out: &mut [InputEvent]) -> Result<usize, DriverError> {
        if !self.running {
            return Err(DriverError::BadState);
        }
        if out.is_empty() {
            return Ok(0);
        }
        // 先吐出积压事件（可能是 IRQ12 路径塞进来的）。
        let mut n = self.queue.poll(out);
        // 只有队里还放得下一个完整包时才读硬件，避免 poll 自己触发"丢最旧"。
        while n < out.len() && self.queue.len() + MAX_EVENTS_PER_PACKET <= DEFAULT_QUEUE {
            if !self.pump()? {
                break;
            }
            n += self.queue.poll(&mut out[n..]);
        }
        Ok(n)
    }
}

// ============================================================
// I2C-HID 触摸板
// ============================================================

/// I2C-HID 触摸板驱动。
///
/// `T` 是厂商/平台的 I2C 控制器实现；协议、描述符解析与事件映射都在本 crate 内。
pub struct I2cHidTouchpadDriver<T: I2cHidTransport> {
    transport: T,
    addr: u8,
    mode: TouchpadMode,
    layout: HidReportLayout,
    report_len: usize,
    last_tip: bool,
    last_x: u16,
    last_y: u16,
    queue: EventQueue<DEFAULT_QUEUE>,
    running: bool,
}

impl<T: I2cHidTransport> I2cHidTouchpadDriver<T> {
    pub fn new(transport: T, addr: u8, mode: TouchpadMode) -> Self {
        Self {
            transport,
            addr,
            mode,
            layout: HidReportLayout::EMPTY,
            report_len: 0,
            last_tip: false,
            last_x: 0,
            last_y: 0,
            queue: EventQueue::new(),
            running: false,
        }
    }

    pub fn mode(&self) -> TouchpadMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: TouchpadMode) {
        self.mode = mode;
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// 传输层访问（诊断/复用控制器）。
    pub fn transport(&mut self) -> &mut T {
        &mut self.transport
    }

    /// 解析出的主要输入报告布局（`start` 之前是空布局）。
    pub fn layout(&self) -> &HidReportLayout {
        &self.layout
    }

    /// 初始化：HID 描述符 → 上电 → 报告描述符 → 校验输入报告长度。
    pub fn init(&mut self) -> Result<(), DriverError> {
        let mut hid = [0u8; HID_DESC_LEN];
        self.transport
            .read(self.addr, REG_HID_DESCRIPTOR, &mut hid)?;
        let info = parse_hid_descriptor(&hid)?;

        // SET_POWER ON：I2C-HID 约定 0x00 = 全开。
        self.transport.write(self.addr, REG_POWER, &[0x00])?;
        self.transport.sleep_ms(5);

        let desc_len = info.report_desc_len as usize;
        if desc_len == 0 || desc_len > MAX_REPORT_DESCRIPTOR {
            return Err(DriverError::Invalid);
        }
        let mut rdesc = [0u8; MAX_REPORT_DESCRIPTOR];
        self.transport
            .read(self.addr, info.report_desc_reg, &mut rdesc[..desc_len])?;
        let layout = parse_report_descriptor(&rdesc[..desc_len])?;
        let primary = *layout.primary().ok_or(DriverError::NotSupported)?;
        if !primary.has_xy() {
            return Err(DriverError::NotSupported);
        }

        // 输入报告长度来自 HID 描述符的 wMaxInputLength（含 Report ID 字节）。
        let report_len = info.max_input_len as usize;
        if report_len == 0 || report_len > MAX_INPUT_LEN {
            return Err(DriverError::Invalid);
        }
        let id_bytes = if primary.report_id != 0 { 1 } else { 0 };
        if report_len < id_bytes || (report_len - id_bytes) * 8 < primary.bit_len as usize {
            return Err(DriverError::Invalid);
        }

        self.layout = primary;
        self.report_len = report_len;
        self.queue.clear();
        self.last_tip = false;
        self.last_x = 0;
        self.last_y = 0;
        self.running = true;
        Ok(())
    }

    /// 处理一条完整输入报告；返回是否产生了事件。
    fn handle_report(&mut self, data: &[u8]) -> bool {
        let rep = match parse_touch_report(&self.layout, data) {
            Ok(r) => r,
            Err(_) => return false,
        };
        let moved = rep.x != self.last_x || rep.y != self.last_y;
        let tip_changed = rep.tip != self.last_tip;
        if !moved && !tip_changed {
            return false;
        }

        match self.mode {
            TouchpadMode::Absolute => {
                self.queue.push(InputEvent::Touch {
                    id: rep.id,
                    x: rep.x,
                    y: rep.y,
                    // 最小 HID 布局没有压感字段，用 tip 映射满量程/零。
                    pressure: if rep.tip { 255 } else { 0 },
                });
            }
            TouchpadMode::MouseEmulation => {
                if tip_changed {
                    self.queue.push(InputEvent::Button {
                        button: BUTTON_LEFT,
                        pressed: rep.tip,
                    });
                } else if rep.tip && moved {
                    let x_max = self.layout.x.map(|f| f.logical_max).unwrap_or(0);
                    let y_max = self.layout.y.map(|f| f.logical_max).unwrap_or(0);
                    let dx = scale_delta(self.last_x, rep.x, x_max);
                    let dy = scale_delta(self.last_y, rep.y, y_max);
                    if dx != 0 || dy != 0 {
                        self.queue.push(InputEvent::Motion { dx, dy });
                    }
                }
            }
        }

        self.last_tip = rep.tip;
        self.last_x = rep.x;
        self.last_y = rep.y;
        true
    }
}

/// 把绝对坐标差映射成相对位移：整幅逻辑范围对应 [`MOTION_SPAN`] 个像素。
fn scale_delta(prev: u16, cur: u16, logical_max: u32) -> i16 {
    let max = logical_max.max(1).min(i32::MAX as u32) as i32;
    let delta = cur as i32 - prev as i32;
    let scaled = delta * MOTION_SPAN / max;
    scaled.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

impl<T: I2cHidTransport> Driver for I2cHidTouchpadDriver<T> {
    fn name(&self) -> &'static str {
        "i2c-hid-touchpad"
    }

    fn class(&self) -> Class {
        Class::Input
    }

    fn probe(&mut self, dev: &DeviceInfo) -> Result<bool, DriverError> {
        Ok(matches!(
            dev.transport,
            Transport::I2cHid { address, .. } if address == self.addr
        ))
    }

    fn start(&mut self, dev: &DeviceInfo) -> Result<(), DriverError> {
        match dev.transport {
            Transport::I2cHid { address, .. } if address == self.addr => self.init(),
            _ => Err(DriverError::NotSupported),
        }
    }

    fn stop(&mut self) {
        if self.running {
            // 尽力进入 OFF（I2C-HID: 0 = ON, 1 = SLEEP, 2 = OFF）。
            let _ = self.transport.write(self.addr, REG_POWER, &[0x02]);
        }
        self.running = false;
        self.queue.clear();
        self.last_tip = false;
        self.last_x = 0;
        self.last_y = 0;
    }

    fn input(&mut self) -> Option<&mut dyn InputOps> {
        Some(self)
    }
}

impl<T: I2cHidTransport> InputOps for I2cHidTouchpadDriver<T> {
    fn name(&self) -> &'static str {
        "I2C-HID 触摸板"
    }

    fn poll(&mut self, out: &mut [InputEvent]) -> Result<usize, DriverError> {
        if !self.running {
            return Err(DriverError::BadState);
        }
        if out.is_empty() {
            return Ok(0);
        }
        let mut n = self.queue.poll(out);
        // 一条报告最多产生 3 条事件（Touch 或 按键+移动）。
        while n < out.len() && self.queue.len() + 3 <= DEFAULT_QUEUE {
            let mut buf = [0u8; MAX_INPUT_LEN];
            let len = self.report_len.min(MAX_INPUT_LEN);
            self.transport.read(self.addr, REG_INPUT, &mut buf[..len])?;
            if !self.handle_report(&buf[..len]) {
                // 无触摸/无变化：本轮结束，避免空转。
                break;
            }
            n += self.queue.poll(&mut out[n..]);
        }
        Ok(n)
    }
}

// ============================================================
// 统一入口
// ============================================================

/// 服务层二选一的输入驱动入口。
///
/// 变体由服务层按 `DeviceInfo::transport` 选择；之后只按 `Driver` / `InputOps` 使用，
/// 与具体总线无关。
pub enum InputDriver<P: Ps2Port, T: I2cHidTransport> {
    Ps2(Ps2MouseDriver<P>),
    I2cHid(I2cHidTouchpadDriver<T>),
}

impl<P: Ps2Port, T: I2cHidTransport> InputDriver<P, T> {
    /// PS/2 鼠标（QEMU 默认可测）。
    pub fn ps2(port: P) -> Self {
        Self::Ps2(Ps2MouseDriver::new(port))
    }

    /// I2C-HID 触摸板；`mode` 决定输出绝对坐标还是鼠标事件。
    pub fn i2c_hid(transport: T, addr: u8, mode: TouchpadMode) -> Self {
        Self::I2cHid(I2cHidTouchpadDriver::new(transport, addr, mode))
    }
}

impl<P: Ps2Port, T: I2cHidTransport> Driver for InputDriver<P, T> {
    fn name(&self) -> &'static str {
        match self {
            InputDriver::Ps2(d) => Driver::name(d),
            InputDriver::I2cHid(d) => Driver::name(d),
        }
    }

    fn class(&self) -> Class {
        Class::Input
    }

    fn probe(&mut self, dev: &DeviceInfo) -> Result<bool, DriverError> {
        match self {
            InputDriver::Ps2(d) => d.probe(dev),
            InputDriver::I2cHid(d) => d.probe(dev),
        }
    }

    fn start(&mut self, dev: &DeviceInfo) -> Result<(), DriverError> {
        match self {
            InputDriver::Ps2(d) => d.start(dev),
            InputDriver::I2cHid(d) => d.start(dev),
        }
    }

    fn stop(&mut self) {
        match self {
            InputDriver::Ps2(d) => d.stop(),
            InputDriver::I2cHid(d) => d.stop(),
        }
    }

    fn input(&mut self) -> Option<&mut dyn InputOps> {
        match self {
            InputDriver::Ps2(d) => Some(d),
            InputDriver::I2cHid(d) => Some(d),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubPort;

    impl Ps2Port for StubPort {
        fn status(&mut self) -> u8 {
            0
        }
        fn read_data(&mut self) -> u8 {
            0
        }
        fn write_data(&mut self, _b: u8) -> Result<(), DriverError> {
            Ok(())
        }
        fn write_cmd(&mut self, _b: u8) -> Result<(), DriverError> {
            Ok(())
        }
    }

    struct StubTransport;

    impl I2cHidTransport for StubTransport {
        fn read(&mut self, _addr: u8, _reg: u8, _out: &mut [u8]) -> Result<(), DriverError> {
            Err(DriverError::NotFound)
        }
        fn write(&mut self, _addr: u8, _reg: u8, _data: &[u8]) -> Result<(), DriverError> {
            Err(DriverError::NotFound)
        }
        fn sleep_ms(&mut self, _ms: u32) {}
    }

    #[test]
    fn enum_routes_probe_and_ops() {
        let mut d: InputDriver<StubPort, StubTransport> = InputDriver::ps2(StubPort);
        assert_eq!(d.name(), "ps2-mouse");
        assert_eq!(d.class(), Class::Input);
        assert!(d.probe(&DeviceInfo::ps2()).unwrap());
        assert!(!d.probe(&DeviceInfo::i2c_hid(0, 0x2C)).unwrap());
        assert!(d.input().is_some());
    }

    #[test]
    fn ps2_driver_drains_decoded_events() {
        let mut drv = Ps2MouseDriver::new(StubPort);
        drv.running = true;
        // 左键按下 + 右移 3。
        for b in [0x09u8, 0x03, 0x00] {
            drv.feed_byte(b);
        }
        assert_eq!(drv.pending(), 2);

        let mut out = [InputEvent::Motion { dx: 0, dy: 0 }; 4];
        let n = drv.input().unwrap().poll(&mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            out[0],
            InputEvent::Button {
                button: BUTTON_LEFT,
                pressed: true
            }
        );
        assert_eq!(out[1], InputEvent::Motion { dx: 3, dy: 0 });

        drv.stop();
        assert!(!drv.is_running());
        assert_eq!(drv.pending(), 0);
    }

    #[test]
    fn poll_before_start_is_bad_state() {
        let mut drv = Ps2MouseDriver::new(StubPort);
        let mut out = [InputEvent::Motion { dx: 0, dy: 0 }; 1];
        assert_eq!(
            drv.input().unwrap().poll(&mut out),
            Err(DriverError::BadState)
        );
    }

    #[test]
    fn scale_delta_clamps_and_scales() {
        assert_eq!(scale_delta(0, 4095, 4095), MOTION_SPAN as i16);
        assert_eq!(scale_delta(4095, 0, 4095), -(MOTION_SPAN as i16));
        assert_eq!(scale_delta(10, 10, 4095), 0);
        // 逻辑最大值为 0 时按 1 处理，避免除零；结果再截断到 i16。
        assert_eq!(scale_delta(0, 1, 0), MOTION_SPAN as i16);
        assert_eq!(scale_delta(0, 1000, 0), i16::MAX);
    }
}
