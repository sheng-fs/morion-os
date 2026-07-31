//! APIC 中断控制器 — x86_64 Local APIC + I/O APIC
//!
//! Local APIC:  每个 CPU 核心的内建中断控制器, 处理本地中断源
//! I/O APIC:    系统芯片组中的外部中断路由器, 将硬件 IRQ 分发到各核心
//!
//! 当前状态: 占位模块, 待实现 MMIO 访问和 APIC 配置逻辑

use crate::boot::BootInfo;

/// 初始化 APIC 子系统
///
/// TODO: 实现以下步骤:
///   1. 检测 APIC 支持 (CPUID)
///   2. 映射 Local APIC MMIO 基地址 (从 ACPI MADT 表获取)
///   3. 配置 Local APIC: 启用 + 设置 Spurious Interrupt Vector
///   4. 解析 MADT 表, 配置 I/O APIC 中断重定向表
///   5. 配置 APIC Timer (用于抢占式调度)
pub fn init(boot_info: &BootInfo) {
    // 占位: 后续通过 ACPI MADT 表获取 APIC 基地址并初始化
    let _ = boot_info;
}
