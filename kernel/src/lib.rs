//! Morion OS 微内核 — 最小可信计算基 (TCB)
//!
//! 阶段一: CPU 初始化地基
//!   - arch  : GDT / TSS / IDT (x86_64)
//!   - video : GOP 线性帧缓冲 + 8x16 位图字体输出
//!   - bootinfo: 引导器传递的 Boot Info 结构
//! 阶段二: 物理内存管理
//!   - memory: 位图物理帧分配器 (基于 UEFI 内存图)

#![no_std]
#![feature(abi_x86_interrupt)]
#![allow(static_mut_refs)]

pub mod arch;
pub mod bootinfo;
pub mod memory;
pub mod video;

/// 停机 CPU (hlt 循环, 永不返回)。
pub fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) }
    }
}