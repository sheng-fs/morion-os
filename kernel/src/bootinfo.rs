//! Boot Info 结构 — 引导器通过物理地址 0x7000 传递给内核
//!
//! 与 boot/src/main.rs 中的 BootInfo 布局严格对应。

/// 引导期服务模块表项（E3b）—— 与 boot/src/main.rs 的 `ServiceModule` 布局严格对应。
///
/// 引导器从自己所在的 ESP 读入服务 ELF（`\EFI\morion\services\<name>.elf`），
/// 用 `LOADER_DATA` 页装下并把这张表交过来；内核据此把每个服务载入它的固定域。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ServiceModule {
    /// 目标固定域号（域号是 ABI，见 kernel/src/main.rs）。
    pub domain: u64,
    /// ELF 镜像的物理地址（页对齐，位于前 4 GiB 的恒等映射内）。
    pub addr: u64,
    /// ELF 镜像字节数。
    pub len: u64,
}

#[repr(C)]
pub struct BootInfo {
    pub magic: u32,            // 0x4D4F5249 = "MORI"
    pub version: u32,          // 3
    pub fb_addr: u64,          // 帧缓冲物理地址
    pub fb_width: u32,         // 宽度 (像素)
    pub fb_height: u32,        // 高度 (像素)
    pub fb_stride: u32,        // 行跨度 (像素)
    pub fb_bpp: u32,           // 每像素位数
    pub mmap_addr: u64,        // 内存图数据物理地址
    pub mmap_entry_count: u64, // 内存图条目数
    pub mmap_entry_size: u64,  // 单个条目字节数
    pub svc_addr: u64,         // 服务模块表物理地址 (0 = 无)
    pub svc_count: u64,        // 服务模块条目数
    pub svc_entry_size: u64,   // 单个模块条目字节数
    /// **ACPI RSDP** 的物理地址（E1a）。0 = 固件未提供 / 引导器版本过旧。
    pub rsdp_addr: u64,
}

impl BootInfo {
    /// **ACPI RSDP** 的物理地址（E1a）。
    ///
    /// 引导器版本低于 [`BOOT_VERSION`] 时返回 0（那个版本还没有这个字段，按新布局读旧内存
    /// 会读到 `svc_entry_size` 之后的垃圾）—— 调用方据此打"无 DMAR"并优雅降级。
    pub fn rsdp_addr(&self) -> u64 {
        if self.version >= BOOT_VERSION {
            self.rsdp_addr
        } else {
            0
        }
    }

    /// 引导期服务模块表（E3b）。
    ///
    /// 引导器未提供（旧引导器 / 构建没有把服务放进 ESP）时返回 `None` —— 调用方据此
    /// 明确报错，而不是静默地"一个服务都没起"。
    ///
    /// `svc_entry_size` 一并校验：内核与引导器是两个独立编译的产物，布局若不一致
    /// （两边 `ServiceModule` 字段变了却没同步重建），宁可判定为"不可用"。
    pub fn service_modules(&self) -> Option<&'static [ServiceModule]> {
        if self.svc_addr == 0 || self.svc_count == 0 {
            return None;
        }
        if self.svc_entry_size != core::mem::size_of::<ServiceModule>() as u64 {
            return None;
        }
        // SAFETY: 表由引导器在 `LOADER_DATA` 页里放好并经 `svc_addr` 交过来；那些帧不在
        // 内核帧分配器的空闲池里（见 `frame_allocator::init` 只放行 CONVENTIONAL），
        // 故这块内存在本函数返回的 `'static` 生命周期内不会被复用。
        Some(unsafe {
            core::slice::from_raw_parts(
                self.svc_addr as *const ServiceModule,
                self.svc_count as usize,
            )
        })
    }
}

/// Boot Info 所在的物理地址
pub const BOOT_INFO_ADDR: usize = 0x7000;

/// 有效 Boot Info 的魔数 "MORI"
pub const BOOT_MAGIC: u32 = 0x4D4F5249;

/// 当前 `BootInfo` 布局版本（与 boot/src/main.rs 写入的 `version` 同步）。
///
/// 布局一变就 +1：内核与引导器是**两个独立编译**的产物，版本不一致时对新增字段按"不可用"
/// 处理（见 [`BootInfo::rsdp_addr`]），而不是按新布局去读旧内存。历史：3 = 加服务模块表，
/// 4 = 加 RSDP。
pub const BOOT_VERSION: u32 = 4;

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
