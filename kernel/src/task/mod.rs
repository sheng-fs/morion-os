//! 任务/进程管理
//!
//! 微内核调度器:
//!   - 抢占式调度 (APIC Timer 中断驱动)
//!   - 优先级队列 (类似 seL4 的 MCS / 固定优先级)
//!   - 上下文切换: 保存/恢复寄存器 + 地址空间切换 (CR3)

pub mod scheduler;

use crate::boot::BootInfo;

/// 创建初始 init 进程
pub fn spawn_init(boot_info: &BootInfo) {
    // 1. 创建新地址空间
    // 2. 映射 init 二进制 (从 initrd 加载)
    // 3. 授予初始能力 (文件服务端点、网络端点等)
    // 4. 设置入口点
    // 5. 放入调度队列
}
