//! 内存管理
//!
//! 阶段二: 物理帧分配 (frame_allocator)
//! 阶段三: 虚拟内存 / 页表 / 内核堆 (paging)
//!
//! 后续阶段将补充: 地址空间管理、页表回收、按需映射等。

pub mod frame_allocator;
pub mod paging;
