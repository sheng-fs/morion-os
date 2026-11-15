use crate::common::Message;
use libdevmodel::{DeviceInfo, Driver, DriverError, InputEvent, InputOps};
use morion::syscall::*;
use morion_input::{Ps2MouseDriver, Ps2Port};

/// scancode set 1 基础键 → (无 shift, 有 shift) 字节；`0` 表示非字符键。
/// 索引即 scancode (0..0x60)。修饰键 (shift/ctrl/alt/caps) 与扩展键不在此列。
const KEYMAP: [(u8, u8); 0x60] = {
    let mut m = [(0u8, 0u8); 0x60];
    m[0x02] = (b'1', b'!');
    m[0x03] = (b'2', b'@');
    m[0x04] = (b'3', b'#');
    m[0x05] = (b'4', b'$');
    m[0x06] = (b'5', b'%');
    m[0x07] = (b'6', b'^');
    m[0x08] = (b'7', b'&');
    m[0x09] = (b'8', b'*');
    m[0x0A] = (b'9', b'(');
    m[0x0B] = (b'0', b')');
    m[0x0C] = (b'-', b'_');
    m[0x0D] = (b'=', b'+');
    m[0x0F] = (b'\t', b'\t');
    m[0x10] = (b'q', b'Q');
    m[0x11] = (b'w', b'W');
    m[0x12] = (b'e', b'E');
    m[0x13] = (b'r', b'R');
    m[0x14] = (b't', b'T');
    m[0x15] = (b'y', b'Y');
    m[0x16] = (b'u', b'U');
    m[0x17] = (b'i', b'I');
    m[0x18] = (b'o', b'O');
    m[0x19] = (b'p', b'P');
    m[0x1A] = (b'[', b'{');
    m[0x1B] = (b']', b'}');
    m[0x1E] = (b'a', b'A');
    m[0x1F] = (b's', b'S');
    m[0x20] = (b'd', b'D');
    m[0x21] = (b'f', b'F');
    m[0x22] = (b'g', b'G');
    m[0x23] = (b'h', b'H');
    m[0x24] = (b'j', b'J');
    m[0x25] = (b'k', b'K');
    m[0x26] = (b'l', b'L');
    m[0x27] = (b';', b':');
    m[0x28] = (b'\'', b'"');
    m[0x29] = (b'`', b'~');
    m[0x2B] = (b'\\', b'|');
    m[0x2C] = (b'z', b'Z');
    m[0x2D] = (b'x', b'X');
    m[0x2E] = (b'c', b'C');
    m[0x2F] = (b'v', b'V');
    m[0x30] = (b'b', b'B');
    m[0x31] = (b'n', b'N');
    m[0x32] = (b'm', b'M');
    m[0x33] = (b',', b'<');
    m[0x34] = (b'.', b'>');
    m[0x35] = (b'/', b'?');
    m[0x39] = (b' ', b' ');
    m
};

/// 查询 scancode 对应的字符字节 (按 shift 状态)；非字符键返回 `None`。
fn key_char(sc: u8, shift: bool) -> Option<u8> {
    let i = sc as usize;
    if i >= KEYMAP.len() {
        return None;
    }
    let (base, shifted) = KEYMAP[i];
    let c = if shift { shifted } else { base };
    if c == 0 {
        None
    } else {
        Some(c)
    }
}

/// i8042 状态口 / 数据口。
const PS2_STATUS: u16 = 0x64;
const PS2_DATA: u16 = 0x60;
/// 状态口 bit0 = 输出缓冲满 (有字节可读); bit5 = 这个字节来自鼠标/AUX 口。
const PS2_OBF: u8 = 0x01;
const PS2_AUX: u8 = 0x20;

/// scancode (set 1) → 按键 的解码器: 维护 E0 前缀与修饰键状态。
///
/// 抽成结构体是因为 **scancode 有两个来源**: IRQ1 投递的消息, 以及直接轮询 i8042。
struct Decoder {
    first: bool,
    ext: bool,
    shift: bool,
    ctrl: bool,
}

impl Decoder {
    const fn new() -> Self {
        Self {
            first: true,
            ext: false,
            shift: false,
            ctrl: false,
        }
    }

    fn feed(&mut self, sc: u8) {
        // 0x00 / 0xFF 不是合法 scancode (i8042 空读/出错时常见): 直接丢, 免得轮询与
        // IRQ 抢同一个字节时凭空造出一个键。
        if sc == 0x00 || sc == 0xFF {
            return;
        }
        if self.first {
            self.first = false;
            println("kbd: first scancode received (input path alive)");
        }

        // E0 扩展前缀: 标记后续字节为扩展键码。
        if sc == 0xE0 {
            self.ext = true;
            return;
        }
        // 释放码 (bit7 置位): 只处理按下码; shift 释放时清除状态。
        if sc & 0x80 != 0 {
            let base = sc & 0x7F;
            if base == 0x2A || base == 0x36 {
                self.shift = false;
            }
            if base == 0x1D {
                self.ctrl = false;
            }
            self.ext = false;
            return;
        }

        // 扩展按下码: 方向键 / 翻页键翻内核终端日志, 数字键盘回车照常送 '\n'。
        //
        // 真机 SAFE 变体没有串口, 屏幕是唯一能看到早期日志的地方 —— PS/2 键盘的
        // 方向键若在这里被丢掉, 用户就完全无法回看启动过程 (USB HID 键盘那条路
        // 已经在 `xhci_srv` 里映射了同样的键)。
        if self.ext {
            self.ext = false;
            match sc {
                0x1C => {
                    sys_key_push(b'\n'); // 数字键盘 Enter
                }
                0x48 => {
                    sys_console_scroll(1); // ↑
                }
                0x50 => {
                    sys_console_scroll(-1); // ↓
                }
                0x49 => {
                    sys_console_scroll(20); // PageUp
                }
                0x51 => {
                    sys_console_scroll(-20); // PageDown
                }
                0x4B => {
                    sys_key_push(0x02); // ← / Ctrl+B: 行编辑光标左移
                }
                0x4D => {
                    sys_key_push(0x06); // → / Ctrl+F: 行编辑光标右移
                }
                0x47 => {
                    sys_key_push(0x01); // Home / Ctrl+A
                }
                0x4F => {
                    sys_key_push(0x05); // End / Ctrl+E
                }
                0x53 => {
                    sys_key_push(0x04); // Delete / Ctrl+D
                }
                _ => {}
            }
            return;
        }

        // 普通按下码。
        match sc {
            0x1D => self.ctrl = true,         // 左 Ctrl 按下
            0x2A | 0x36 => self.shift = true, // 左右 shift 按下
            0x0E => {
                sys_key_push(0x08); // 退格
            }
            0x0F => {
                sys_key_push(0x09); // Tab: shell 命令补全
            }
            0x1C => {
                sys_key_push(b'\n'); // 回车
            }
            // Ctrl+C: 行编辑与长输出 (dmesg) 据此中止。
            0x2E if self.ctrl => {
                sys_key_push(0x03);
            }
            _ => {
                if let Some(c) = key_char(sc, self.shift) {
                    sys_key_push(c);
                }
            }
        }
    }
}

/// i8042 是**一个共享的硬件协议**（数据口 0x60 / 命令口 0x64）：键盘与鼠标的
/// 初始化 ACK、配置字节都走同一个输出缓冲。两个进程各开一个「输入服务」会互相
/// 吃掉对方的 ACK（实测：AUX 初始化超时），所以 PS/2 键盘与鼠标由**同一个服务**
/// （本域，域 4）持有；鼠标协议/解码复用 `morion_input`，这里只提供端口实现。
struct KernelPs2;

impl Ps2Port for KernelPs2 {
    fn status(&mut self) -> u8 {
        sys_port_in8(PS2_STATUS)
    }

    fn read_data(&mut self) -> u8 {
        sys_port_in8(PS2_DATA)
    }

    fn write_data(&mut self, b: u8) -> Result<(), DriverError> {
        sys_port_out8(PS2_DATA, b);
        Ok(())
    }

    fn write_cmd(&mut self, b: u8) -> Result<(), DriverError> {
        sys_port_out8(PS2_STATUS, b);
        Ok(())
    }
}

/// 域 4 — 用户态键盘驱动: 注册接收 IRQ1, 循环取 scancode, 解码成字节推进内核键队列。
///
/// **G4 起内核不解释按键**: 这里只做 scancode → 字节的翻译 —— 可打印字符按 shift 状态取
/// 对应字符, 退格发 `0x08`, (小/数字键盘的) 回车发 `'\n'`; 方向键 / PgUp / PgDn 直接
/// 翻内核终端的历史日志 (行编辑 / 行历史在 `gfx_srv` 的屏幕控制台)。
///
/// **两条来源缺一不可**:
///   - IRQ1 消息 (QEMU / 中断路由正常的机器);
///   - 直接轮询 i8042 数据口 —— 真机上固件/ACPI 可能把 IRQ1 交给 IOAPIC 而内核只用了
///     8259A, 中断根本到不了 CPU; 但按键字节仍躺在 0x60 的输出缓冲里, 轮询状态口的 OBF
///     位就能取出来。IRQ 正常时内核已在中断里读走字节, 这里读不到 (OBF=0), 不会重复。
pub fn run() {
    // 先初始化 PS/2 鼠标：此时**还没注册 IRQ1**，内核的中断处理器不会读走 i8042 的
    // 控制器响应字节（配置字节 AUX=0，会被 IRQ1 当成键盘数据取走，导致初始化超时）。
    let mut mouse = Ps2MouseDriver::new(KernelPs2);
    match Driver::start(&mut mouse, &DeviceInfo::ps2()) {
        Ok(()) => {
            print("INPUT1 ps2 aux enabled wheel=");
            print(if mouse.wheel_enabled() { "1" } else { "0" });
            print(" kind=");
            println(mouse.kind_name());
        }
        Err(e) => {
            print("INPUT1 ps2 aux init FAILED: ");
            println(match e {
                DriverError::NotSupported => "NotSupported",
                DriverError::NotFound => "NotFound",
                DriverError::Timeout => "Timeout",
                DriverError::Io => "Io",
                DriverError::BadState => "BadState",
                DriverError::Busy => "Busy",
                DriverError::Invalid => "Invalid",
                DriverError::Other(s) => s,
            });
        }
    }
    let mut mouse_events = 0u64;
    let mut evs = [InputEvent::Motion { dx: 0, dy: 0 }; 8];

    if sys_register_irq(1) != 1 {
        println("kbd: register irq1 FAILED");
        return;
    }
    println("kbd: irq1 registered, waiting for scancodes (press a key)");

    let mut dec = Decoder::new();
    let mut idle_ms = 0u64;
    let mut diag_at = 5000u64;
    loop {
        let mut got = false;

        // 路径 1: IRQ1 投递的消息 (tag = scancode; 内核在中断里已经读走了字节)。
        loop {
            let mut msg = Message {
                from: 0,
                to: 0,
                tag: 0,
                // 96 = 内核 `ipc::PAYLOAD_LEN`; 两者不一致时这里会编译失败。
                payload: [0; 96],
            };
            let tag = sys_try_recv(&mut msg as *mut Message as *mut u8);
            if tag == u64::MAX {
                break;
            }
            dec.feed(tag as u8);
            got = true;
        }

        // 路径 2: 直接轮询 i8042。**AUX（鼠标）字节留给下面的鼠标驱动** ——
        // 两者在同一个服务里顺序消费同一个输出缓冲，不会再出现跨进程抢字节。
        // 键盘字节由这里读走；AUX 字节最多滞留一个轮询周期。
        let st = sys_port_in8(PS2_STATUS);
        if st != u8::MAX && st & PS2_OBF != 0 && st & PS2_AUX == 0 {
            let b = sys_port_in8(PS2_DATA);
            if b != u8::MAX {
                dec.feed(b);
                got = true;
            }
        }

        // 路径 3: PS/2 鼠标事件（通用输入驱动）。事件目前只做计数/首次日志 ——
        // 把指针事件接到 GUI 是下一步（需要 shell/gfx 的输入路由），驱动层已就绪。
        if let Ok(n) = mouse.poll(&mut evs) {
            if n > 0 {
                if mouse_events == 0 {
                    println("INPUT2 ps2 mouse event received");
                }
                mouse_events += n as u64;
            }
        }

        if got {
            idle_ms = 0;
            continue;
        }

        idle_ms += 5;
        // 真机上"按键毫无反应"时, 这行能区分三种情况: 没有 i8042 (status=255)、
        // IRQ 没投递但轮询可用、以及键盘本身没信号。5 s 打一次, 60 s 再打一次
        // (后者通常落在屏幕最后几行, 不翻页也看得见)。
        if idle_ms == diag_at {
            print("kbd: no key in ");
            print_u64(idle_ms / 1000);
            print("s - i8042 status=");
            print_u64(st as u64);
            println(" (255 = no controller)");
            if diag_at == 5000 {
                diag_at = 60000;
            }
        }
        sys_sleep(5);
    }
}
