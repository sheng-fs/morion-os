//! 键盘按键队列 (G4)
//!
//! 键盘域 (`kbd_srv`) 把 scancode 解码成一个字节后经 `SYS_KEY_PUSH` 推进队列; 消费方
//! (`gfx_srv` 的屏幕控制台) 经 `SYS_KEY_READ` **阻塞取**一个字节。
//!
//! 内核在这里**不做任何解释** —— 可打印字符、退格、回车都只是一个字节。行编辑、回显、
//! 行历史全在用户态, 这正是 G4「把输入搬出内核」的意思: 内核终端自此只剩输出。
//!
//! 队列满时**丢弃新键** (交互输入绝不阻塞内核)。

use crate::scheduler::{wake_one, KEY_WAIT};
use spin::Mutex;

/// 环形队列容量 (字节): 够兜住一次打字突发。
const QUEUE_LEN: usize = 64;

/// 按键环形队列 (纯数据结构, 便于单独单测)。
struct KeyQueue {
    buf: [u8; QUEUE_LEN],
    /// 最老字节的下标。
    head: usize,
    /// 下一个写入位置的下标。
    tail: usize,
    /// 当前字节数 (≤ `QUEUE_LEN`)。
    count: usize,
}

impl KeyQueue {
    const fn new() -> KeyQueue {
        KeyQueue {
            buf: [0; QUEUE_LEN],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    /// 推入一个字节; 队列已满返回 `false` (该字节被丢弃)。
    fn push(&mut self, c: u8) -> bool {
        if self.count >= QUEUE_LEN {
            return false;
        }
        self.buf[self.tail] = c;
        self.tail = (self.tail + 1) % QUEUE_LEN;
        self.count += 1;
        true
    }

    /// 取走最老的字节; 空队列返回 `None`。
    fn pop(&mut self) -> Option<u8> {
        if self.count == 0 {
            return None;
        }
        let c = self.buf[self.head];
        self.head = (self.head + 1) % QUEUE_LEN;
        self.count -= 1;
        Some(c)
    }
}

static QUEUE: Mutex<KeyQueue> = Mutex::new(KeyQueue::new());

/// 推进一个按键字节 (`SYS_KEY_PUSH`), 并唤醒阻塞在 [`KEY_WAIT`] 上的取键者。
pub fn push(c: u8) {
    QUEUE.lock().push(c);
    wake_one(KEY_WAIT);
}

/// 取走一个按键字节 (`SYS_KEY_READ`); 队列为空返回 `None`, 由调用方自行阻塞。
pub fn pop() -> Option<u8> {
    QUEUE.lock().pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pops_in_fifo_order() {
        let mut q = KeyQueue::new();
        assert_eq!(q.pop(), None);
        assert!(q.push(b'a'));
        assert!(q.push(b'b'));
        assert_eq!(q.pop(), Some(b'a'));
        assert_eq!(q.pop(), Some(b'b'));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn drops_new_keys_when_full() {
        let mut q = KeyQueue::new();
        for i in 0..QUEUE_LEN as u8 {
            assert!(q.push(i));
        }
        assert!(!q.push(0xFF)); // 满: 丢新键, 不覆盖最老的
        assert_eq!(q.pop(), Some(0));
    }

    /// 跨越环形缓冲末尾的读写顺序必须正确。
    #[test]
    fn wraps_around_the_ring() {
        let mut q = KeyQueue::new();
        for i in 0..40u8 {
            q.push(i);
        }
        for i in 0..40u8 {
            assert_eq!(q.pop(), Some(i));
        }
        // head = tail = 40: 再填满会跨过缓冲末尾回绕。
        for i in 0..QUEUE_LEN as u8 {
            assert!(q.push(i));
        }
        for i in 0..QUEUE_LEN as u8 {
            assert_eq!(q.pop(), Some(i));
        }
    }
}
