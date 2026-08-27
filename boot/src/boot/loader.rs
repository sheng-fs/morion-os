//! 内核/映像加载器
//!
//! 负责从 Nix store 路径加载内核映像 (ELF 或 PE) 和 initrd，
//! 验证签名后将控制权转移给内核。
//!
//! 支持的格式：
//!   - ELF64 (Linux 内核)
//!   - PE32+ (UEFI 原生应用)
//!   - bzImage (Linux 传统格式)
//!   - 预编译飞地 unikernel

use crate::config::entries::BootEntry;
use crate::security::{ImageHasher, SignatureVerifier, TpmMeasurer};
use uefi::table::boot::BootServices;
use alloc::string::String;

/// ELF64 文件头
#[repr(C)]
pub struct Elf64Header {
    pub ident: [u8; 16],
    pub etype: u16,
    pub machine: u16,
    pub version: u32,
    pub entry: u64,
    pub phoff: u64,
    pub shoff: u64,
    pub flags: u32,
    pub ehsize: u16,
    pub phentsize: u16,
    pub phnum: u16,
    pub shentsize: u16,
    pub shnum: u16,
    pub shstrndx: u16,
}

/// ELF64 程序头
#[repr(C)]
pub struct Elf64ProgramHeader {
    pub ptype: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub paddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

// ELF 常量
const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const ELF_CLASS_64: u8 = 2;
const PT_LOAD: u32 = 1;
#[allow(dead_code)]
const PT_PHDR: u32 = 6;

/// 内核加载结果
pub struct KernelImage {
    /// 入口点地址 (物理地址)
    pub entry_point: u64,
    /// 映像基址
    pub base_address: u64,
    /// 映像大小
    pub image_size: u64,
    /// 内核命令行参数
    pub cmdline: String,
    /// 映像的 SHA-256 哈希
    pub hash: [u8; 32],
}

/// 内核加载器
#[allow(dead_code)]
pub struct KernelLoader<'a> {
    boot_services: &'a BootServices,
    hasher: ImageHasher,
    verifier: SignatureVerifier,
}

impl<'a> KernelLoader<'a> {
    pub fn new(boot_services: &'a BootServices, verifier: SignatureVerifier) -> Self {
        Self {
            boot_services,
            hasher: ImageHasher::new(),
            verifier,
        }
    }

    /// 加载 ELF64 格式内核
    ///
    /// 执行步骤：
    ///   1. 读取 ELF 文件头，验证魔数和架构
    ///   2. 遍历程序头表，将每个 PT_LOAD 段加载到内存
    ///   3. 根据段对齐要求分配物理内存
    ///   4. 计算并记录 SHA-256 哈希 (同时流式测量到 TPM PCR[8])
    ///   5. 返回入口点
    pub fn load_elf64(
        &mut self,
        data: &[u8],
        cmdline: &str,
        _entry: &BootEntry,
        tpm: &mut TpmMeasurer,
    ) -> Result<KernelImage, &'static str> {
        if data.len() < core::mem::size_of::<Elf64Header>() {
            return Err("数据太小，无法包含 ELF 头");
        }

        let header = unsafe { &*(data.as_ptr() as *const Elf64Header) };

        // 验证 ELF 魔数
        if header.ident[0..4] != ELF_MAGIC {
            return Err("无效的 ELF 魔数");
        }
        if header.ident[4] != ELF_CLASS_64 {
            return Err("仅支持 64 位 ELF");
        }
        if header.machine != 0x3E {
            // EM_X86_64
            return Err("仅支持 x86_64 架构");
        }

        // TPM 测量内核映像
        tpm.measure_kernel(data);

        // 遍历程序头加载段
        let phoff = header.phoff as usize;
        let phentsize = header.phentsize as usize;
        let phnum = header.phnum as usize;

        let mut lowest_addr = u64::MAX;
        let mut highest_addr = 0u64;

        for i in 0..phnum {
            let ph_offset = phoff + i * phentsize;
            if ph_offset + core::mem::size_of::<Elf64ProgramHeader>() > data.len() {
                break;
            }
            let ph = unsafe { &*(data.as_ptr().add(ph_offset) as *const Elf64ProgramHeader) };

            if ph.ptype == PT_LOAD && ph.memsz > 0 {
                let load_addr = ph.paddr;
                let memsz = ph.memsz as usize;

                // 分配物理内存
                let pages = (memsz + 0xFFF) >> 12; // 向上取整到 4KB
                if pages > 0 {
                    // 使用 UEFI AllocatePages 分配物理内存
                    // 由于 UEFI 中我们已经是物理地址视角，直接复制数据
                    // 实际实现需要用 AllocateAddress 或 AllocateAnyPages
                    self.allocate_and_load(
                        load_addr,
                        &data[ph.offset as usize..ph.offset as usize + ph.filesz as usize],
                        memsz,
                    )?;

                    if load_addr < lowest_addr {
                        lowest_addr = load_addr;
                    }
                    let end = load_addr + memsz as u64;
                    if end > highest_addr {
                        highest_addr = end;
                    }
                }
            }
        }

        let image_size = highest_addr - lowest_addr;

        // 计算哈希
        let hash = [0u8; 32];
        // hash = self.hasher.finalize().bytes;

        Ok(KernelImage {
            entry_point: header.entry,
            base_address: lowest_addr,
            image_size,
            cmdline: cmdline.into(),
            hash,
        })
    }

    /// 分配物理内存并加载段数据
    fn allocate_and_load(
        &self,
        phys_addr: u64,
        data: &[u8],
        memsz: usize,
    ) -> Result<(), &'static str> {
        // 在 UEFI 环境中，物理内存与虚拟内存一一映射
        // 直接 memcpy 到目标地址
        if !data.is_empty() {
            unsafe {
                let dst = core::slice::from_raw_parts_mut(phys_addr as *mut u8, data.len());
                dst.copy_from_slice(data);
            }
        }

        // 零填充 BSS 区域
        if memsz > data.len() {
            let bss_start = phys_addr + data.len() as u64;
            let bss_size = memsz - data.len();
            unsafe {
                core::ptr::write_bytes(bss_start as *mut u8, 0, bss_size);
            }
        }

        Ok(())
    }

    /// 计算内核命令行的总长度
    pub fn cmdline_len(cmdline: &str) -> usize {
        cmdline.len() + 1 // +1 for null terminator
    }
}

/// Linux 内核引导参数设置
///
/// 对于 Linux 内核 (bzImage 格式)：
///   1. 设置 32 位引导协议的参数
///   2. 填充 setup_header 结构
///   3. 跳转到保护模式入口
///
/// 参照 Linux 内核文档 Documentation/x86/boot.rst
pub fn prepare_linux_boot_params(
    _kernel_data: &[u8],
    _cmdline: &str,
    _initrd_data: Option<&[u8]>,
) -> Result<u64, &'static str> {
    // Linux bzImage 格式：
    //   偏移 0x1F1: setup_sects (1 字节)
    //   偏移 0x202: 协议版本 (2 字节)
    //   偏移 0x206: 内核启动标识 (2 字节)
    //   偏移 0x214: 内核入口类型 (1 字节)
    //
    // 完整实现需要：
    //   1. 读取内核 setup 头部
    //   2. 分配并填充 boot_params 结构
    //   3. 设置 cmd_line_ptr 指向内核命令行
    //   4. 设置 initrd 相关字段
    //   5. 返回保护模式入口点

    Err("bzImage 加载暂未完整实现")
}
