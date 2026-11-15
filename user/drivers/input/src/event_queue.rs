//! 固定容量环形事件队列。
//!
//! 用户态没有分配器，所有驱动都用固定数组缓存事件。这里给出一个通用实现，
//! PS/2 鼠标与 I2C-HID 触摸板共用：
//!
//! - [`EventQueue::push`] 在**满时丢最旧**并返回被丢的事件；
//! - [`EventQueue::poll`] 批量取事件，实现 `InputOps::poll` 的语义；
//! - [`EventQueue::dropped`] 记录累计丢弃条数，便于诊断"事件风暴导致上层来不及取"。
//!
//! 为什么丢最旧而不是丢新：输入是实时状态流，用户看到的光标位置取决于**最新**的
//! 相对位移；保留旧事件会让光标滞后于手，丢弃新事件则表现为"卡住"。

use libdevmodel::InputEvent;

/// 环形事件队列。
pub struct EventQueue<const N: usize> {
    buf: [InputEvent; N],
    head: usize,
    len: usize,
    dropped: u64,
}

impl<const N: usize> EventQueue<N> {
    /// 空队列。
    pub const fn new() -> Self {
        Self {
            // InputEvent 是 Copy 的普通枚举，用无副作用的占位值初始化。
            buf: [InputEvent::Motion { dx: 0, dy: 0 }; N],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }

    /// 队列容量（常量，调用方可据此判断是否还有空间塞下一个完整包）。
    pub const fn capacity(&self) -> usize {
        N
    }

    /// 当前事件数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 是否已满。
    pub fn is_full(&self) -> bool {
        self.len >= N
    }

    /// 累计因队列满被丢弃的事件数。
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// 清空事件（不清零丢弃计数 —— 它是驱动整个生命周期的统计）。
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// 压入一条事件。队列满时**丢弃最旧**并把它返回；未满返回 `None`。
    ///
    /// `N == 0` 时任何事件都会被立即丢弃并返回。
    pub fn push(&mut self, ev: InputEvent) -> Option<InputEvent> {
        if N == 0 {
            self.dropped += 1;
            return Some(ev);
        }
        if self.len == N {
            let old = self.pop();
            self.dropped += 1;
            self.buf[(self.head + self.len) % N] = ev;
            self.len += 1;
            return old;
        }
        self.buf[(self.head + self.len) % N] = ev;
        self.len += 1;
        None
    }

    /// 取出一条事件（FIFO）。
    pub fn pop(&mut self) -> Option<InputEvent> {
        if N == 0 || self.len == 0 {
            return None;
        }
        let ev = self.buf[self.head];
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(ev)
    }

    /// 批量取出事件填入 `out`，返回实际条数（0 表示当前无事件）。
    ///
    /// 最多写到 `out.len()` 条，剩余事件留在队列里供下次轮询。
    pub fn poll(&mut self, out: &mut [InputEvent]) -> usize {
        let mut n = 0;
        while n < out.len() {
            match self.pop() {
                Some(ev) => {
                    out[n] = ev;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }
}

impl<const N: usize> Default for EventQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: InputEvent = InputEvent::Motion { dx: 1, dy: 2 };
    const B: InputEvent = InputEvent::Motion { dx: 3, dy: 4 };
    const C: InputEvent = InputEvent::Motion { dx: 5, dy: 6 };

    #[test]
    fn fifo_order_and_empty() {
        let mut q: EventQueue<4> = EventQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.pop(), None);
        assert_eq!(q.push(A), None);
        assert_eq!(q.push(B), None);
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop(), Some(A));
        assert_eq!(q.pop(), Some(B));
        assert_eq!(q.pop(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn full_drops_oldest() {
        let mut q: EventQueue<2> = EventQueue::new();
        assert_eq!(q.push(A), None);
        assert_eq!(q.push(B), None);
        assert!(q.is_full());
        // 满时丢最旧（A），保留 B 并追加 C。
        assert_eq!(q.push(C), Some(A));
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped(), 1);
        let mut out = [A; 2];
        assert_eq!(q.poll(&mut out), 2);
        assert_eq!(out, [B, C]);
    }

    #[test]
    fn poll_respects_output_length() {
        let mut q: EventQueue<4> = EventQueue::new();
        for ev in [A, B, C] {
            q.push(ev);
        }
        let mut small = [A; 2];
        assert_eq!(q.poll(&mut small), 2);
        assert_eq!(small, [A, B]);
        assert_eq!(q.len(), 1);
        let mut rest = [A; 4];
        assert_eq!(q.poll(&mut rest), 1);
        assert_eq!(rest[0], C);
        assert_eq!(q.poll(&mut rest), 0);
    }

    #[test]
    fn zero_sized_queue_always_drops() {
        let mut q: EventQueue<0> = EventQueue::new();
        assert_eq!(q.push(A), Some(A));
        assert_eq!(q.dropped(), 1);
        assert!(q.is_empty());
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn clear_keeps_drop_counter() {
        let mut q: EventQueue<1> = EventQueue::new();
        q.push(A);
        q.push(B);
        assert_eq!(q.dropped(), 1);
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.dropped(), 1);
    }
}
