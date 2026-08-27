//! kexec 热启动机制 (桩实现)
//!
//! 跳过 UEFI 固件重初始化，直接跳转到新内核。
//! 当前版本提供接口桩，完整实现需要内核配合。

use crate::boot::loader::KernelImage;
use uefi::table::boot::BootServices;
use uefi::table::runtime::RuntimeServices;

/// kexec 引导上下文
#[repr(C)]
pub struct KexecContext {
    pub entry: u64,
    pub kernel_base: u64,
    pub kernel_size: u64,
    pub cmdline_ptr: u64,
    pub initrd_base: u64,
    pub initrd_size: u64,
    pub memory_map_ptr: u64,
    pub memory_map_entries: u64,
    pub memory_map_desc_size: u64,
    pub memory_map_key: u64,
    pub runtime_services: u64,
    pub acpi_rsdp: u64,
    pub smbios_entry: u64,
}

/// kexec 引导器
pub struct KexecBoot {
    available: bool,
}

impl KexecBoot {
    pub fn new() -> Self {
        Self { available: true }
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    /// 准备 kexec 跳转上下文 (桩)
    pub fn prepare_context(
        &self,
        _boot_services: &BootServices,
        runtime_services: &RuntimeServices,
        kernel: &KernelImage,
        _initrd_data: Option<&[u8]>,
    ) -> Result<KexecContext, &'static str> {
        // 桩实现: 返回最小化上下文
        Ok(KexecContext {
            entry: kernel.entry_point,
            kernel_base: kernel.base_address,
            kernel_size: kernel.image_size,
            cmdline_ptr: 0,
            initrd_base: 0,
            initrd_size: 0,
            memory_map_ptr: 0,
            memory_map_entries: 0,
            memory_map_desc_size: 0,
            memory_map_key: 0,
            runtime_services: runtime_services as *const _ as u64,
            acpi_rsdp: 0,
            smbios_entry: 0,
        })
    }

    /// 执行 kexec 跳转 (永不返回)
    pub unsafe fn jump_to_kernel(
        entry_point: u64,
        boot_params_ptr: u64,
    ) -> ! {
        core::arch::asm!(
            "cli",
            "xor eax, eax",
            "xor ebx, ebx",
            "xor ecx, ecx",
            "xor edx, edx",
            "xor esi, esi",
            "xor ebp, ebp",
            "jmp r8",
            in("r8") entry_point,
            in("rdi") boot_params_ptr,
            options(noreturn),
        );
        // 显式永不返回终止点 — asm(options=noreturn) 对 rust-analyzer 在部分 target 下不可见,
        // 追加 loop{} 作为 rust-analyzer 可见的 -> ! 保证 (永远不会被实际执行)
        #[allow(unreachable_code)]
        loop { core::hint::spin_loop(); }
    }
}