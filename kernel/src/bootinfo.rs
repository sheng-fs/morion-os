//! Boot Info 结构 — 引导器通过物理地址 0x7000 传递给内核
//!
//! 与 boot/src/main.rs 中的 BootInfo 布局严格对应。

#[repr(C)]
pub struct BootInfo {
    pub magic: u32,           // 0x4D4F5249 = "MORI"
    pub version: u32,         // 2
    pub fb_addr: u64,         // 帧缓冲物理地址
    pub fb_width: u32,        // 宽度 (像素)
    pub fb_height: u32,       // 高度 (像素)
    pub fb_stride: u32,       // 行跨度 (像素)
    pub fb_bpp: u32,          // 每像素位数
    pub mmap_addr: u64,       // 内存图数据物理地址
    pub mmap_entry_count: u64,// 内存图条目数
    pub mmap_entry_size: u64, // 单个条目字节数
}

/// Boot Info 所在的物理地址
pub const BOOT_INFO_ADDR: usize = 0x7000;

/// 有效 Boot Info 的魔数 "MORI"
pub const BOOT_MAGIC: u32 = 0x4D4F5249;

/// UEFI 内存描述符 (EFI_MEMORY_DESCRIPTOR, 40 字节)
///
/// 与 uefi crate 的 MemoryDescriptor 布局一致:
///   ty(u32) + pad(u32) + phys_start(u64) + virt_start(u64) + page_count(u64) + att(u64)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemoryDescriptor {
    pub ty: u32,
    _pad: u32,
    pub phys_start: u64,
    pub virt_start: u64,
    pub page_count: u64, // 4 KiB 页数量
    pub att: u64,
}

// UEFI 内存类型 (MemoryType)
pub const MEMORY_RESERVED: u32 = 0;
pub const MEMORY_LOADER_CODE: u32 = 1;
pub const MEMORY_LOADER_DATA: u32 = 2;
pub const MEMORY_BOOT_SERVICES_CODE: u32 = 3;
pub const MEMORY_BOOT_SERVICES_DATA: u32 = 4;
pub const MEMORY_RUNTIME_SERVICES_CODE: u32 = 5;
pub const MEMORY_RUNTIME_SERVICES_DATA: u32 = 6;
pub const MEMORY_CONVENTIONAL: u32 = 7;
pub const MEMORY_UNUSABLE: u32 = 8;
pub const MEMORY_ACPI_RECLAIM: u32 = 9;

/// 读取并校验 Boot Info。魔数不合法时直接停机 (此时帧缓冲不可用, 无法打印)。
pub fn get() -> &'static BootInfo {
    let info = unsafe { &*(BOOT_INFO_ADDR as *const BootInfo) };
    if info.magic != BOOT_MAGIC {
        crate::halt();
    }
    info
}
