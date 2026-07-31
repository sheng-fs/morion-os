//! 架构特定模块 (x86_64)
//!
//! 实现 x86_64 特有的硬件交互:
//!   - GDT/IDT/TSS 初始化
//!   - 页表操作 (4-level / 5-level paging)
//!   - APIC 中断控制器
//!   - MSR 寄存器操作
//!   - CPU 特性检测

pub mod x86_64;
