//! 计算器状态机 —— 纯逻辑、不依赖窗口 / syscall，便于单独做单元测试。
//!
//! 采用「累加器 + 当前输入 + 待执行运算符」的经典四则运算模型:
//! `2 + 3 ×` 会先把 `2 + 3` 算成 `5`，再挂起 `×`（从左到右求值，不做优先级）。

/// 四则运算符。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

/// 显示缓冲区长度（`-9223372036854775808` 共 20 字节，留一点余量）。
pub const DISPLAY_MAX: usize = 24;

/// 计算器状态。
#[derive(Clone, Copy, Debug)]
pub struct Calc {
    /// 已确认的累加值。
    acc: i64,
    /// 当前显示 / 正在输入的数。
    cur: i64,
    /// 待执行的运算符。
    op: Option<Op>,
    /// 本次数字输入是否已开始（用于区分 `0` 与「刚按完运算符」）。
    entering: bool,
    /// 下一个数字键是否应重新开始一个数。
    fresh: bool,
    /// 是否处于错误态（除零 / 溢出）。
    error: bool,
}

impl Default for Calc {
    fn default() -> Calc {
        Calc::new()
    }
}

impl Calc {
    /// 初始状态：显示 0。
    pub const fn new() -> Calc {
        Calc {
            acc: 0,
            cur: 0,
            op: None,
            entering: false,
            fresh: true,
            error: false,
        }
    }

    /// 输入一个数字位（`0..=9`）。
    pub fn digit(&mut self, d: u8) {
        if d > 9 {
            return;
        }
        if self.error {
            *self = Calc::new();
        }
        if self.fresh {
            self.cur = d as i64;
            self.fresh = false;
            self.entering = true;
            return;
        }
        // 题目只要求整数四则运算；溢出直接进错误态，不静默回绕。
        match self
            .cur
            .checked_mul(10)
            .and_then(|v| v.checked_add(d as i64))
        {
            Some(v) => self.cur = v,
            None => self.error = true,
        }
    }

    /// 按下一个运算符（重复按运算符只替换，不重复求值）。
    pub fn operator(&mut self, op: Op) {
        if self.error {
            return;
        }
        if self.entering {
            if let Some(prev) = self.op {
                match apply(self.acc, prev, self.cur) {
                    Some(v) => self.acc = v,
                    None => {
                        self.error = true;
                        return;
                    }
                }
            } else {
                self.acc = self.cur;
            }
            // 显示中间结果（如 `2 + 3 +` 显示 5），与常见计算器一致。
            self.cur = self.acc;
        } else if self.op.is_none() {
            self.acc = self.cur;
        }
        self.op = Some(op);
        self.fresh = true;
        self.entering = false;
    }

    /// 执行 `=`。
    pub fn equals(&mut self) {
        if self.error {
            return;
        }
        if let Some(op) = self.op {
            match apply(self.acc, op, self.cur) {
                Some(v) => {
                    self.acc = v;
                    self.cur = v;
                }
                None => {
                    self.error = true;
                    return;
                }
            }
            self.op = None;
        } else {
            self.acc = self.cur;
        }
        self.fresh = true;
        self.entering = false;
    }

    /// `C`：清空全部状态。
    pub fn clear(&mut self) {
        *self = Calc::new();
    }

    /// 是否处于错误态（界面显示 `ERROR`）。
    pub fn is_error(&self) -> bool {
        self.error
    }

    /// 当前应显示的值（仅测试断言用）。
    #[cfg(test)]
    pub fn value(&self) -> i64 {
        self.cur
    }

    /// 把显示文本写进 `buf` 并返回字符串视图（错误态返回 `ERROR`）。
    pub fn display<'a>(&self, buf: &'a mut [u8; DISPLAY_MAX]) -> &'a str {
        let n = if self.error {
            buf[..5].copy_from_slice(b"ERROR");
            5
        } else {
            format_i64(self.cur, buf)
        };
        core::str::from_utf8(&buf[..n]).unwrap_or("?")
    }
}

/// 纯函数：`a op b`；除零 / 溢出返回 `None`。
pub fn apply(a: i64, op: Op, b: i64) -> Option<i64> {
    match op {
        Op::Add => a.checked_add(b),
        Op::Sub => a.checked_sub(b),
        Op::Mul => a.checked_mul(b),
        Op::Div => a.checked_div(b),
    }
}

/// 把 `v` 以十进制写进 `buf`（含负号），返回字节数。
fn format_i64(v: i64, buf: &mut [u8; DISPLAY_MAX]) -> usize {
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    let neg = v < 0;
    let mut n = v.unsigned_abs();
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let digits = tmp.len() - i;
    let mut out = 0usize;
    if neg {
        buf[out] = b'-';
        out += 1;
    }
    buf[out..out + digits].copy_from_slice(&tmp[i..]);
    out + digits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_digits(c: &mut Calc, digits: &[u8]) {
        for &d in digits {
            c.digit(d - b'0');
        }
    }

    fn shown(c: &Calc) -> [u8; DISPLAY_MAX] {
        let mut buf = [0u8; DISPLAY_MAX];
        c.display(&mut buf);
        buf
    }

    #[test]
    fn starts_at_zero() {
        let c = Calc::new();
        assert_eq!(c.value(), 0);
        assert_eq!(&shown(&c)[..1], b"0".as_slice());
    }

    #[test]
    fn addition() {
        let mut c = Calc::new();
        type_digits(&mut c, b"12");
        c.operator(Op::Add);
        type_digits(&mut c, b"3");
        c.equals();
        assert_eq!(c.value(), 15);
    }

    #[test]
    fn subtraction_multiplication_division() {
        let mut c = Calc::new();
        type_digits(&mut c, b"9");
        c.operator(Op::Sub);
        type_digits(&mut c, b"4");
        c.equals();
        assert_eq!(c.value(), 5);

        c.clear();
        type_digits(&mut c, b"7");
        c.operator(Op::Mul);
        type_digits(&mut c, b"8");
        c.equals();
        assert_eq!(c.value(), 56);

        c.clear();
        type_digits(&mut c, b"8");
        c.operator(Op::Div);
        type_digits(&mut c, b"2");
        c.equals();
        assert_eq!(c.value(), 4);
    }

    #[test]
    fn division_by_zero_is_error() {
        let mut c = Calc::new();
        type_digits(&mut c, b"8");
        c.operator(Op::Div);
        type_digits(&mut c, b"0");
        c.equals();
        assert!(c.is_error());
        assert_eq!(&shown(&c)[..5], b"ERROR".as_slice());
    }

    #[test]
    fn chained_operators_evaluate_left_to_right() {
        let mut c = Calc::new();
        type_digits(&mut c, b"2");
        c.operator(Op::Add);
        type_digits(&mut c, b"3");
        c.operator(Op::Add);
        assert_eq!(c.value(), 5);
        type_digits(&mut c, b"4");
        c.equals();
        assert_eq!(c.value(), 9);
    }

    #[test]
    fn clear_recovers_from_error() {
        let mut c = Calc::new();
        c.operator(Op::Div);
        c.digit(0);
        c.equals();
        assert!(c.is_error());
        c.clear();
        assert!(!c.is_error());
        assert_eq!(c.value(), 0);
        type_digits(&mut c, b"42");
        assert_eq!(c.value(), 42);
    }

    #[test]
    fn digit_after_equals_starts_new_number() {
        let mut c = Calc::new();
        type_digits(&mut c, b"6");
        c.operator(Op::Mul);
        type_digits(&mut c, b"7");
        c.equals();
        assert_eq!(c.value(), 42);
        type_digits(&mut c, b"5");
        assert_eq!(c.value(), 5);
        c.equals();
        assert_eq!(c.value(), 5);
    }

    #[test]
    fn negative_result_and_min_value_format() {
        let mut c = Calc::new();
        type_digits(&mut c, b"3");
        c.operator(Op::Sub);
        type_digits(&mut c, b"10");
        c.equals();
        assert_eq!(c.value(), -7);
        assert_eq!(&shown(&c)[..2], b"-7".as_slice());

        let mut buf = [0u8; DISPLAY_MAX];
        let n = format_i64(i64::MIN, &mut buf);
        assert_eq!(&buf[..n], b"-9223372036854775808".as_slice());
    }
}
