//! 引导信息 — UEFI 引导器传递给内核的系统描述

/// 内存类型 (UEFI 兼容)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum MemoryType {
    Reserved = 0,
    LoaderCode = 1,
    LoaderData = 2,
    BootServicesCode = 3,
    BootServicesData = 4,
    RuntimeServicesCode = 5,
    RuntimeServicesData = 6,
    Conventional = 7,
    Unusable = 8,
    ACPIReclaim = 9,
    ACPINVS = 10,
    MemoryMappedIO = 11,
    MemoryMappedIOPortSpace = 12,
    PalCode = 13,
    Persistent = 14,
}

/// 内存描述符 (UEFI 兼容)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct MemoryDescriptor {
    pub ty: MemoryType,
    pub physical_start: u64,
    pub virtual_start: u64,
    pub number_of_pages: u64,
    pub attribute: u64,
}

/// 帧缓冲信息 (GOP)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct FrameBufferInfo {
    pub base: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32, // 0=BGR, 1=RGB, 2=Bitmask
}

/// 引导信息 — 引导器在跳转前填充
#[derive(Debug, Clone)]
#[repr(C)]
pub struct BootInfo {
    /// 魔数 "MBRN" (0x4F52424D)
    pub magic: u32,
    /// 结构体版本
    pub version: u32,
    /// 内核命令行偏移量
    pub cmdline_offset: u64,
    /// 物理内存映射条目数
    pub memory_map_entries: u64,
    /// 物理内存映射描述符大小
    pub memory_map_desc_size: u64,
    /// 物理内存映射基地址
    pub memory_map_base: u64,
    /// 帧缓冲信息
    pub framebuffer: FrameBufferInfo,
    /// ACPI RSDP 表指针
    pub acpi_rsdp: u64,
    /// SMBIOS 表入口
    pub smbios_entry: u64,
    /// TPM EventLog 地址
    pub tpm_event_log: u64,
    /// TPM EventLog 大小
    pub tpm_event_log_size: u64,
    /// 当前 PCR 值 [0..7]
    pub pcr_values: [u8; 256],
}

impl BootInfo {
    /// 获取物理内存映射
    pub fn memory_map(&self) -> &[MemoryDescriptor] {
        let count = self.memory_map_entries as usize;
        let desc_size = self.memory_map_desc_size as usize;
        let base = self.memory_map_base as *const MemoryDescriptor;
        if base.is_null() || count == 0 {
            return &[];
        }
        // SAFETY: 引导器保证此内存映射在 ExitBootServices 后保持不变
        unsafe { core::slice::from_raw_parts(base, count) }
    }

    /// 计算可用物理内存总大小 (Conventional 类型)
    pub fn total_memory(&self) -> u64 {
        self.memory_map()
            .iter()
            .filter(|d| d.ty == MemoryType::Conventional)
            .map(|d| d.number_of_pages * 4096)
            .sum()
    }

    /// 验证魔数
    pub fn valid(&self) -> bool {
        self.magic == 0x4F52424D // "MBRM"
    }

    /// 获取内核命令行
    pub fn cmdline(&self) -> &str {
        if self.cmdline_offset == 0 {
            return "";
        }
        unsafe {
            let ptr = self.cmdline_offset as *const u8;
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr, len))
        }
    }
}
