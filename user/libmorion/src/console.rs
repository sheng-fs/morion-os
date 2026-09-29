//! 用户态行编辑器 (G4)
//!
//! 内核只把按键字节搬进队列 ([`crate::syscall::sys_key_read`]), **行编辑在这里**:
//! 可打印字符追加进调用方缓冲并立刻回显 (走 [`crate::syscall::print`], 于是串口与可选的
//! 屏幕镜像都会看到), 退格发 `\b` 让屏幕控制台擦掉那一格, 回车结束本行。
//!
//! # 为什么编辑在客户端而不是控制台服务里
//!
//! `gfx_srv` 是**单线程服务**: 它一旦阻塞在"等按键"上, 就到了自己唯一的请求循环里出不来,
//! 别的客户端 (比如 app 自测的 `GFX_OP_FILL`/`GFX_OP_TEXT`) 会一直被饿到有人按键为止。
//! 而 `SYS_KEY_READ` 在**客户端**阻塞是免费的 —— 内核把任务睡下, 有键再唤醒, 不占任何
//! 服务的时间片, 屏幕控制台照旧随时响应绘图请求。
//!
//! 键盘目前只产 **ASCII** (`kbd_srv` 的 scancode 映射表里没有非 ASCII), 故退格恒为 1 列。
//! 将来若接入非 ASCII 输入, 这里要按字形的**显示列数**重复发 `\b` (列数只有服务端字库知道)。

use crate::syscall::{flush, print, sys_key_read};

/// 退格键 (BS)。
const BS: u8 = 0x08;

/// 阻塞读一行到 `buf`, 返回行长度 (字节, 不含换行); 失败返回 `u64::MAX`。
///
/// 超过 `buf.len()` 的按键被丢弃 (行不会溢出)。回显即时提交 (`flush`), 否则要等下一次
/// 换行才看得见自己敲的字。
pub fn readline(buf: &mut [u8]) -> u64 {
    let mut len = 0usize;
    loop {
        let c = sys_key_read();
        if c > 0xFF {
            return u64::MAX; // 非字节值 (只可能是内核侧异常)
        }
        match c as u8 {
            b'\n' | b'\r' => {
                print("\n");
                flush();
                return len as u64;
            }
            BS | 0x7F => {
                if len > 0 {
                    len -= 1;
                    // `\b` = 屏幕控制台光标左移一列并擦掉那一格 (串口里也能看到退格意图)。
                    print("\x08");
                    flush();
                }
            }
            b if (0x20..=0x7E).contains(&b) => {
                if len < buf.len() {
                    buf[len] = b;
                    len += 1;
                    echo(b);
                }
            }
            _ => {} // 其它控制键与非 ASCII 字节: 忽略
        }
    }
}

/// 回显一个可打印 ASCII 字节 (调用方已保证落在 `0x20..=0x7E`)。
fn echo(b: u8) {
    if let Ok(s) = core::str::from_utf8(&[b]) {
        print(s);
        flush();
    }
}
