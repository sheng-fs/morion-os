// ============================================================
// Morion OS 微内核入口 — _kernel_start
//
// 由引导器 (UEFI) 加载后直接跳转至此。
// 入参:
//   RDI = *BootInfo 结构体指针 (引导器传递的系统信息)
//   RSI = kernel_cmdline 指针 (指向内核命令行参数字符串)
//
// 此函数不返回: 内核启动后永久运行调度循环。
// ============================================================

#![no_std]
#![no_main]
#![feature(asm_experimental_arch)]
#![allow(incomplete_features)]

extern crate morion_kernel;

use core::panic::PanicInfo;
use core::arch::asm;

/// 内核入口点 (由链接脚本导出)
///
/// 引导器 (morion-boot.efi) 加载 ELF 后跳转到此地址。
/// 链接脚本 ENTRY(_kernel_start) 确保此为第一条指令。
#[no_mangle]
pub extern "C" fn _kernel_start(boot_info: u64, cmdline: u64) -> ! {
    // ============================================================
    // 阶段 0: 临时 GDT/IDT (引导早期最小化环境)
    // ============================================================
    // 在正式初始化前, 先用极简的 GDT/IDT 防止三重故障

    // 加载临时 GDT (空 + 代码段 + 数据段)
    unsafe {
        load_temp_gdt();
        load_temp_idt();
    }

    // ============================================================
    // 阶段 1: 早期串口输出 (调试)
    // ============================================================
    // 内核尚未有内存分配器, 使用固定地址的串口缓冲
    early_println!("Morion OS 微内核启动...");
    early_println!("  引导信息 @ 0x{:016X}", boot_info);
    early_println!("  内核命令行 @ 0x{:016X}", cmdline);
    early_println!("  架构: x86_64");

    // ============================================================
    // 阶段 2: 解析引导信息
    // ============================================================
    // BootInfo 包含:
    //   - 物理内存映射 (UEFI MemoryMap)
    //   - 帧缓冲地址 (GOP)
    //   - ACPI RSDP 指针
    //   - SMBIOS 入口
    //   - TPM EventLog
    let boot_info = unsafe {
        &*(boot_info as *const morion_kernel::boot::BootInfo)
    };

    early_println!("  物理内存: {}MB 可用", boot_info.total_memory() >> 20);

    // ============================================================
    // 阶段 3: 物理内存管理初始化
    // ============================================================
    // 构建位图/Buddy 分配器, 标记已用/可用/预留页帧
    morion_kernel::mm::physical::init(boot_info.memory_map());
    early_println!("  物理内存管理器已初始化");

    // ============================================================
    // 阶段 4: 虚拟内存初始化
    // ============================================================
    // 建立内核页表:
    //   - 恒等映射低 1MB (BIOS/VGA)
    //   - 映射内核到 -2GB 虚拟地址
    //   - 映射帧缓冲到内核虚拟地址空间
    morion_kernel::mm::virtual_mem::init(boot_info);
    early_println!("  虚拟内存管理器已初始化");

    // ============================================================
    // 阶段 5: 中断系统初始化
    // ============================================================
    // 安装正式 IDT:
    //   - 异常处理 (缺页 #PF, GPF #GP, 除零 #DE)
    //   - 中断处理 (PIT/APIC Timer, 键盘, 串口)
    morion_kernel::arch::x86_64::interrupts::init();
    early_println!("  中断系统已初始化");

    // ============================================================
    // 阶段 6: 设备初始化
    // ============================================================
    // 初始化 APIC (Local APIC + I/O APIC)
    morion_kernel::arch::x86_64::apic::init(boot_info);
    early_println!("  APIC 已初始化");

    // ============================================================
    // 阶段 7: 全局分配器
    // ============================================================
    // 初始化内核堆分配器 (Slab / Buddy)
    morion_kernel::mm::allocator::init();
    early_println!("  内核分配器已初始化");

    // ============================================================
    // 阶段 8: 调度器初始化
    // ============================================================
    // 创建初始进程/任务:
    //   1. init 进程 — 用户态的初始服务管理器
    //   2. idle 任务 — 空闲时运行的占位任务
    morion_kernel::task::scheduler::init();
    early_println!("  调度器已初始化");

    // ============================================================
    // 阶段 9: IPC 基础设施
    // ============================================================
    // 初始化能力空间和端点
    morion_kernel::ipc::init();
    early_println!("  IPC 子系统已初始化");

    // ============================================================
    // 阶段 10: 启动第一个用户态进程
    // ============================================================
    // 依据 boot_info 中的 initrd 路径创建 init 进程
    morion_kernel::task::spawn_init(boot_info);
    early_println!("  init 进程已创建");

    // ============================================================
    // 阶段 11: 进入调度循环 (永不返回)
    // ============================================================
    early_println!("  进入调度循环...");
    unsafe {
        // 开启中断, 调度器接管
        asm!("sti", options(nomem, nostack));
    }

    morion_kernel::task::scheduler::run_loop();
}

// ============================================================
// 临时 GDT/IDT (引导早期)
// ============================================================

unsafe fn load_temp_gdt() {
    // 空 GDT 描述符 (仅用于防止 #GP 故障)
    #[repr(C, align(16))]
    struct TempGdt {
        null: u64,
        code: u64,
        data: u64,
    }

    static TEMP_GDT: TempGdt = TempGdt {
        null: 0,
        code: 0x00AF9A000000FFFF, // 64-bit 代码段
        data: 0x00AF92000000FFFF, // 64-bit 数据段
    };

    #[repr(C, packed)]
    struct GdtDescriptor {
        limit: u16,
        base: u64,
    }

    let gdt_desc = GdtDescriptor {
        limit: (core::mem::size_of::<TempGdt>() - 1) as u16,
        base: &TEMP_GDT as *const _ as u64,
    };

    unsafe {
        asm!(
            "lgdt [{}]",
            in(reg) &gdt_desc,
            options(nostack, preserves_flags)
        );
        // 刷新段选择器
        asm!(
            "mov ax, 0x10",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            out("ax") _,
            options(nostack)
        );
    }
}

unsafe fn load_temp_idt() {
    // 临时 IDT: 所有向量指向一个通用异常处理
    #[repr(C, align(16))]
    struct TempIdtEntry {
        offset_low: u16,
        selector: u16,
        ist: u8,
        flags: u8,
        offset_mid: u16,
        offset_high: u32,
        _reserved: u32,
    }

    static mut TEMP_IDT: [TempIdtEntry; 256] = [TempIdtEntry {
        offset_low: 0,
        selector: 0,
        ist: 0,
        flags: 0,
        offset_mid: 0,
        offset_high: 0,
        _reserved: 0,
    }; 256];

    #[repr(C, packed)]
    struct IdtDescriptor {
        limit: u16,
        base: u64,
    }

    // 安装通用处理函数
    extern "C" fn temp_exception_handler() {
        unsafe {
            asm!(
                "cli",
                "hlt",
                options(nomem, nostack)
            );
        }
    }

    let handler_addr = temp_exception_handler as u64;
    for entry in unsafe { TEMP_IDT.iter_mut() } {
        entry.offset_low = handler_addr as u16;
        entry.offset_mid = (handler_addr >> 16) as u16;
        entry.offset_high = (handler_addr >> 32) as u32;
        entry.selector = 0x08; // 代码段选择器
        entry.flags = 0x8E;    // Present, Ring0, Interrupt Gate
    }

    let idt_desc = IdtDescriptor {
        limit: (core::mem::size_of::<[TempIdtEntry; 256]>() - 1) as u16,
        base: unsafe { TEMP_IDT.as_ptr() as u64 },
    };

    unsafe {
        asm!("lidt [{}]", in(reg) &idt_desc, options(nostack, preserves_flags));
    }
}

// ============================================================
// Panic 处理
// ============================================================

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    early_println!("\n!!! 内核 Panic !!!");
    early_println!("{}", info);

    // 尝试输出调用栈 (如果可用)
    // morion_kernel::debug::stack_trace();

    // 停机: 关中断 + HLT
    unsafe {
        asm!("cli; hlt", options(nomem, nostack));
    }

    loop {
        unsafe {
            asm!("hlt", options(nomem, nostack));
        }
    }
}
