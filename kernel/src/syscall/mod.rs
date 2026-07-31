//! 系统调用 — 最小化内核原语 (10~20 个)
//!
//! 所有系统调用均需携带能力，内核对每条路径强制校验。
//!
//! 调用约定 (x86_64 syscall):
//!   RAX = 系统调用号
//!   RDI, RSI, RDX, R10, R8, R9 = 参数
//!   返回: RAX = 返回值, RDX = 错误码

/// 系统调用号 (极简: 仅 10~20 个)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum SyscallNumber {
    /// IPC: 发送消息 (可附带能力)
    Send = 0x01,
    /// IPC: 接收消息 (阻塞直到消息到达)
    Receive = 0x02,
    /// IPC: 同步调用 (Send + Receive)
    Call = 0x03,
    /// IPC: 异步通知
    Notify = 0x04,

    /// 内存: 映射物理帧到地址空间
    Map = 0x10,
    /// 内存: 解除映射
    Unmap = 0x11,
    /// 内存: 分配物理帧
    AllocateFrame = 0x12,
    /// 内存: 释放物理帧
    FreeFrame = 0x13,

    /// 保护域: 创建新进程/线程
    CreateDomain = 0x20,
    /// 保护域: 销毁进程
    DestroyDomain = 0x21,

    /// 调度: 主动让出 CPU
    Schedule = 0x30,

    /// 中断: 注册中断处理
    RegisterInterrupt = 0x40,
    /// 中断: 应答中断
    AckInterrupt = 0x41,

    /// 飞地: 创建硬件隔离飞地
    CreateEnclave = 0x50,
}

/// 系统调用错误码
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum SyscallError {
    Success = 0,
    InvalidCap = 1,
    InvalidArgument = 2,
    NoMemory = 3,
    NotAuthorized = 4,
    WouldBlock = 5,
    Timeout = 6,
}
