//! x86_64 架构实现
//!
//! 子模块:
//!   - gdt:       全局描述符表 (64-bit 代码/数据段 + TSS)
//!   - idt:       中断描述符表 (异常 + 硬件中断)
//!   - interrupts: 中断处理函数注册与分发
//!   - paging:    页表操作 (CR3 切换, 页表遍历)
//!   - apic:      Local APIC + I/O APIC 配置

pub mod interrupts;
pub mod apic;

/// 启用中断 (STI)
pub fn enable_interrupts() {
    unsafe { core::arch::asm!("sti", options(nomem, nostack)); }
}

/// 禁用中断 (CLI)
pub fn disable_interrupts() {
    unsafe { core::arch::asm!("cli", options(nomem, nostack)); }
}

/// 暂停 CPU 直到下次中断 (HLT)
pub fn halt() {
    unsafe { core::arch::asm!("hlt", options(nomem, nostack)); }
}
