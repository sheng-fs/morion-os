//! 微内核调度器
//!
//! 抢占式、基于优先级的调度:
//!   - 每个 CPU 有一个运行队列
//!   - APIC Timer 中断驱动抢占
//!   - 调度类: Round-Robin (同优先级), FIFO (实时)
//!   - Idle 任务: 最低优先级, 忙等 HLT
//!
//! 未来: CFS 或 MCS (Mixed-Criticality Systems)

pub fn init() {
    // 1. 创建 idle 任务 (每个 CPU 一个)
    // 2. 初始化运行队列
    // 3. 配置 APIC Timer 为调度时钟
}

/// 进入调度循环 (永不返回)
pub fn run_loop() -> ! {
    loop {
        // 1. 从运行队列中选取下一个任务 (pick_next)
        // 2. 如果无任务可运行 → idle 任务
        // 3. 上下文切换 (switch_to)
        //    - 保存当前 CPU 状态
        //    - 切换地址空间 (CR3)
        //    - 恢复下一个任务的 CPU 状态
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)); }
    }
}

/// 让出 CPU (主动调用 syscall `schedule`)
pub fn yield_now() {
    // 触发调度器重新选择
}
