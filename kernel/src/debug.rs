//! 调试输出 — 极简串口日志
//!
//! 在用户态日志服务就绪前，内核通过 COM1 (0x3F8) 输出。
//! 之后通过能力将串口能力授予用户态日志服务。

pub fn early_println(args: core::fmt::Arguments) {
    use core::fmt::Write;
    // 使用 COM1 串口输出
    // 实际实现直接写 0x3F8 端口
}

/// 输出调用栈 (x86_64 frame pointer 回溯)
pub fn stack_trace() {
    // 遍历 RBP 链式帧指针
    // 读取每个帧的返回地址
    // 解析 DWARF 符号 (如果编译期保留)
}
