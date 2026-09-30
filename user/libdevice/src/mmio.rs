//! 易失 MMIO / DMA 读写原语。
//!
//! 设备寄存器与 DMA 环都映射为**非缓存**，必须用 `read_volatile` / `write_volatile`，
//! 否则编译器会把访问挪走或合并。宽度按设备要求显式指定（读 32 位寄存器就别用 64 位读）。

/// 读一个字节。
pub fn rd8(a: u64) -> u8 {
    unsafe { core::ptr::read_volatile(a as *const u8) }
}
/// 读 16 位（小端，x86 原生）。
pub fn rd16(a: u64) -> u16 {
    unsafe { core::ptr::read_volatile(a as *const u16) }
}
/// 读 32 位。
pub fn rd32(a: u64) -> u32 {
    unsafe { core::ptr::read_volatile(a as *const u32) }
}
/// 读 64 位。
pub fn rd64(a: u64) -> u64 {
    unsafe { core::ptr::read_volatile(a as *const u64) }
}

/// 写一个字节。
pub fn wr8(a: u64, v: u8) {
    unsafe { core::ptr::write_volatile(a as *mut u8, v) }
}
/// 写 16 位。
pub fn wr16(a: u64, v: u16) {
    unsafe { core::ptr::write_volatile(a as *mut u16, v) }
}
/// 写 32 位。
pub fn wr32(a: u64, v: u32) {
    unsafe { core::ptr::write_volatile(a as *mut u32, v) }
}
/// 写 64 位。
pub fn wr64(a: u64, v: u64) {
    unsafe { core::ptr::write_volatile(a as *mut u64, v) }
}

/// 提交内存写序。
///
/// 用于"写描述符 / avail 环 → 敲门铃"这类次序敏感的场合：x86 本就是 TSO（硬件不乱序），
/// 这里只为挡住**编译器**重排。
pub fn fence() {
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}
