//! 用户态行编辑器 (G4)
//!
//! 内核只把按键字节搬进队列 ([`crate::syscall::sys_key_read`]), **行编辑在这里**:
//! 可打印字符插入到光标处并立刻回显 (走 [`crate::syscall::print`], 于是串口与可选的
//! 屏幕镜像都会看到), 退格发 `\b` 让屏幕控制台擦掉光标前一格, 回车结束本行。
//!
//! # 光标与重绘
//!
//! SAFE 变体的屏幕是内核终端；内核终端本身维护行缓冲和编辑光标，本模块用几个控制字节
//! 让它移动/删除：
//!
//! - `\x02` (Ctrl+B) = 光标左移
//! - `\x06` (Ctrl+F) = 光标右移
//! - `\x04` (Ctrl+D) = 删除光标处字符
//! - `\x05` (Ctrl+E) = 光标移到行尾
//! - `\b`   = 删除光标前一个字符
//!
//! 插入字符直接 `print`，内核终端会插在它自己的光标处。这样屏幕上的行始终和这里的
//! `buf` 一致，不需要知道提示符宽度。
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

use crate::syscall::{flush, print, sys_console_line_edit, sys_key_read};

/// 退格键 (BS)。
const BS: u8 = 0x08;
/// Ctrl+B / ←：光标左移。
const LEFT: u8 = 0x02;
/// Ctrl+F / →：光标右移。
const RIGHT: u8 = 0x06;
/// Ctrl+D / Delete：删除光标处字符。
const DELETE: u8 = 0x04;
/// Ctrl+E / End：光标移到行尾。
const END: u8 = 0x05;
/// Tab：命令补全。
const TAB: u8 = 0x09;
/// Ctrl+C：取消当前行。
const CANCEL: u8 = 0x03;

/// 阻塞读一行到 `buf`, 返回行长度 (字节, 不含换行); 失败返回 `u64::MAX`。
///
/// - `prompt`：当前提示符，补全需要换行重画时用它重新打印；
/// - `commands`：第一段命令名的补全表（只在这个列表里做前缀补全）。
///
/// 超过 `buf.len()` 的按键被丢弃 (行不会溢出)。回显即时提交 (`flush`), 否则要等下一次
/// 换行才看得见自己敲的字。
///
/// 编辑期间置「行编辑」标志：真机 SAFE 变体下屏幕就是内核终端，各驱动的异步日志
/// 与这里的回显共用同一行，不隔离就会把用户半打完的命令冲成乱码。
pub fn readline(buf: &mut [u8], prompt: &str, commands: &[&str]) -> u64 {
    let mut len = 0usize;
    let mut cur = 0usize;
    sys_console_line_edit(true);
    loop {
        let c = match sys_key_read() {
            c if c <= 0xFF => c as u8,
            _ => {
                sys_console_line_edit(false);
                return u64::MAX; // 非字节值 (只可能是内核侧异常)
            }
        };

        match c {
            b'\n' | b'\r' => {
                print("\n");
                flush();
                sys_console_line_edit(false); // 先把新行提交, 再补显编辑期间的日志
                return len as u64;
            }
            BS | 0x7F => {
                if cur > 0 {
                    // 光标前一个字符（ASCII）左移，后缀在 buf 里左移，屏幕上由内核终端删。
                    buf.copy_within(cur..len, cur - 1);
                    cur -= 1;
                    len -= 1;
                    print("\x08");
                    flush();
                }
            }
            LEFT => {
                if cur > 0 {
                    cur -= 1;
                    print("\x02");
                    flush();
                }
            }
            RIGHT => {
                if cur < len {
                    cur += 1;
                    print("\x06");
                    flush();
                }
            }
            DELETE => {
                if cur < len {
                    buf.copy_within(cur + 1..len, cur);
                    len -= 1;
                    print("\x04");
                    flush();
                }
            }
            END => {
                while cur < len {
                    cur += 1;
                    print("\x06");
                }
                flush();
            }
            CANCEL => {
                print("^C\n");
                flush();
                sys_console_line_edit(false);
                return 0;
            }
            TAB => {
                complete(buf, &mut len, &mut cur, prompt, commands);
            }
            b if (0x20..=0x7E).contains(&b) && len < buf.len() => {
                // 插到光标处：后缀右移，屏幕上的内核终端也按同样的位置插入。
                buf.copy_within(cur..len, cur + 1);
                buf[cur] = b;
                cur += 1;
                len += 1;
                echo(b);
            }
            // Home / Ctrl+A：逐格左移，不能直接发 0x01 —— 内核终端会把光标移到提示符前面。
            0x01 => {
                while cur > 0 {
                    cur -= 1;
                    print("\x02");
                }
                flush();
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

/// Tab 补全：只看第一段（光标前没有空格），只补命令名。
fn complete(buf: &mut [u8], len: &mut usize, cur: &mut usize, prompt: &str, commands: &[&str]) {
    // 只补第一个词，且光标在行尾；带参数时不做路径补全（避免误改参数）。
    if *cur == 0 || *cur != *len || buf[..*cur].contains(&b' ') {
        return;
    }
    let prefix_len = *cur;
    let old_len = *len;

    let mut count = 0usize;
    let mut first: Option<&str> = None;
    let mut lcp_len = 0usize;
    for &cmd in commands {
        if cmd.as_bytes().starts_with(&buf[..prefix_len]) {
            count += 1;
            if let Some(first_cmd) = first {
                let a = first_cmd.as_bytes();
                let mut n = 0;
                while n < lcp_len && n < cmd.len() && a[n] == cmd.as_bytes()[n] {
                    n += 1;
                }
                lcp_len = n;
            } else {
                first = Some(cmd);
                lcp_len = cmd.len();
            }
        }
    }

    let Some(first) = first else {
        return; // 没有匹配：什么也不做
    };

    if count == 1 {
        // 唯一匹配：补全并加一个空格（光标在行尾，后缀为空）。
        let word = first.as_bytes();
        let mut n = 0;
        for &b in word {
            if n < buf.len() {
                buf[n] = b;
                n += 1;
            }
        }
        if n < buf.len() {
            buf[n] = b' ';
            n += 1;
        }
        *len = n;
        *cur = n;
        replace_line(old_len, buf, n, n);
        return;
    }

    if lcp_len > prefix_len {
        // 多个匹配但有更长的公共前缀：先补到公共前缀。
        let word = &first.as_bytes()[..lcp_len];
        let mut n = 0;
        for &b in word {
            if n < buf.len() {
                buf[n] = b;
                n += 1;
            }
        }
        *len = n;
        *cur = n;
        replace_line(old_len, buf, n, n);
        return;
    }

    // 无法再扩展：换行列出候选，再恢复提示符和当前输入。
    print("\n");
    for &cmd in commands {
        if cmd.as_bytes().starts_with(&buf[..prefix_len]) {
            print(cmd);
            print("  ");
        }
    }
    print("\n");
    print(prompt);
    if let Ok(s) = core::str::from_utf8(&buf[..*len]) {
        print(s);
    }
    for _ in *cur..*len {
        print("\x02");
    }
    flush();
}

/// 把屏幕上 `old_len` 个输入字符擦掉，换成 `buf[..new_len]`，光标停在 `new_cur`。
///
/// 只在补全时调用（此时旧光标在行尾），所以先退格擦掉旧输入区，再重画。
/// 提示符本身不擦：`old_len` 正好是输入字符数，退格不会碰到提示符。
fn replace_line(old_len: usize, buf: &[u8], new_len: usize, new_cur: usize) {
    for _ in 0..old_len {
        print("\x08");
    }
    if let Ok(s) = core::str::from_utf8(&buf[..new_len]) {
        print(s);
    }
    for _ in new_cur..new_len {
        print("\x02");
    }
    flush();
}
