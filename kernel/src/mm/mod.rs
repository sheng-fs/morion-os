//! 内存管理子系统
//!
//! 物理内存: 位图/Buddy 分配器
//! 虚拟内存: 页表管理 (4-level paging)
//! 分配器:   Slab/Buddy 内核堆

pub mod physical;
pub mod virtual_mem;
pub mod allocator;
