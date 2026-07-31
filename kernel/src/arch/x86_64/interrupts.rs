//! 中断处理 — x86_64 IDT 初始化与异常/IRQ 分发
//!
//! 中断向量分配:
//!   0-31   : CPU 异常 (除零, GPF, 缺页等)
//!   32-47  : 硬件中断 (IRQ0-IRQ15 → 重映射到 32-47)
//!   48-79  : 微内核保留
//!   80     : 系统调用 (syscall 指令, MSR_LSTAR)
//!   81-255 : 用户态 IPC 快速路径 (可选)

/// 初始化中断系统
pub fn init() {
    // 1. 安装 IDT (256 个向量)
    // 2. 重映射 PIC (如果需要) 或配置 I/O APIC
    // 3. 设置 syscall MSR (syscall/sysret 使用)
    // 4. 启用中断 (STI)
}

/// 注册中断处理函数
pub fn register_handler(vector: u8, handler: unsafe extern "C" fn()) {
    // 将 handler 安装到 IDT[vector]
}
