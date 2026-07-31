//! 虚拟内存管理 — x86_64 页表操作
//!
//! 页表结构: PML4 → PDPT → PD → PT → Page (4KB)
//! 大页支持: 2MB (PD 级) 和 1GB (PDPT 级)
//!
//! 内核映射:
//!   0xFFFF800000000000 ~ 0xFFFFFFFFFFFFFFFF: 内核空间 (-2GB)
//!   0x0000000000000000 ~ 0x00007FFFFFFFFFFF: 用户空间

use crate::boot::BootInfo;

pub fn init(boot_info: &BootInfo) {
    // 1. 从 BSP 继承初始页表 (UEFI 身份映射)
    // 2. 重建内核页表:
    //    a. 恒等映射低 1MB
    //    b. 映射内核 .text/.rodata (RWX 属性)
    //    c. 映射帧缓冲 (WC 缓存策略)
    //    d. 映射 APIC MMIO
    // 3. 切换 CR3 → 新内核页表
    // 4. 刷 TLB
}

/// 映射物理地址到指定虚拟地址
pub fn map(virt: u64, phys: u64, flags: PageFlags) -> Result<(), ()> {
    Ok(())
}

/// 解除虚拟地址映射
pub fn unmap(virt: u64) {
}

/// 页表项标志 (x86_64 PML4E/PDPTE/PDE/PTE 通用)
pub struct PageFlags(u64);

impl PageFlags {
    pub const PRESENT: u64       = 1 << 0;
    pub const WRITABLE: u64      = 1 << 1;
    pub const USER: u64          = 1 << 2;
    pub const WRITE_THROUGH: u64 = 1 << 3;
    pub const CACHE_DISABLE: u64 = 1 << 4;
    pub const HUGE_PAGE: u64     = 1 << 7;
    pub const NO_EXECUTE: u64    = 1 << 63; // 需要 EFER.NXE = 1
}
