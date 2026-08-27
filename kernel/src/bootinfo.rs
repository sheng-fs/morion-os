//! Boot Info 结构 — 引导器通过物理地址 0x7000 传递给内核
//!
//! 与 boot/src/main.rs 中的 BootInfo 布局严格对应。

#[repr(C)]
pub struct BootInfo {
    pub magic: u32,      // 0x4D4F5249 = "MORI"
    pub version: u32,    // 1
    pub fb_addr: u64,    // 帧缓冲物理地址
    pub fb_width: u32,   // 宽度 (像素)
    pub fb_height: u32,  // 高度 (像素)
    pub fb_stride: u32,  // 行跨度 (像素)
    pub fb_bpp: u32,     // 每像素位数
}

/// Boot Info 所在的物理地址
pub const BOOT_INFO_ADDR: usize = 0x7000;

/// 有效 Boot Info 的魔数 "MORI"
pub const BOOT_MAGIC: u32 = 0x4D4F5249;

/// 读取并校验 Boot Info。魔数不合法时直接停机 (此时帧缓冲不可用, 无法打印)。
pub fn get() -> &'static BootInfo {
    let info = unsafe { &*(BOOT_INFO_ADDR as *const BootInfo) };
    if info.magic != BOOT_MAGIC {
        crate::halt();
    }
    info
}