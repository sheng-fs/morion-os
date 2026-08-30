//! Morion OS 微内核入口点
//!
//! 由引导器 (UEFI) 在长模式下跳转到此, 物理地址 0x100000。

#![no_std]
#![no_main]

use morion_kernel::{arch, bootinfo, cap, domain, ipc, memory, pager, scheduler, syscall, video};

// ---------------------------------------------------------------------------
// Multiboot2 头 + 32→64 过渡代码 (用于 QEMU "-kernel" 直接启动, 避开 OVMF)
//
// 说明:
//   * 原本 UEFI 引导器将控制权交给 _start (64 位, 长模式已开, 栈已好, 0x7000 上有 BootInfo)。
//   * 现在 ENTRY() 改为 _mb2_entry_32。Multiboot2 引导器 (QEMU/GRUB) 在 32 位保护
//     模式下跳入; 本代码负责自建长模式早期页表、构造一份兼容 BootInfo、再跳 _start。
//   * UEFI 场景下 bootloader 依旧直接跳 _start (硬编码入口地址从 ELF header 读
//     entry 点为 _mb2_entry_32 — 为了兼容 UEFI bootloader, loader.rs 里的 entry
//     读取应该从 ELF header.entry 改为符号 _start, 但 boot/src/boot/loader.rs 目前用
//     `header.entry`, 所以这里必须保证 UEFI bootloader 仍能正确跳 64 位代码。
//     实际 UEFI bootloader (boot/) 已经是: 解析 ELF 段 → memcpy 到物理地址
//     → 把 entry_point (header.entry) 转为函数指针调用。它在长模式下调用,
//     所以 32 位入口会死。我们在 UEFI bootloader 硬编码使用符号 "_start" 的地址。
//     由于不想改 bootloader, 这里用一个小技巧:
//     在 main.rs 末尾提供同名 _mb2_entry_32 的 64 位"兼容桩", 仅当 CPU 已经在
//     长模式 (CS.L=1) 被调用时才走到。但 Multiboot2 调用者是 32 位, 会走真实 32 位代码。
//     我们在 global_asm 中让 .mb2entry 段里出现真正的 32 位标签
//     `_mb2_entry_32`; 同时在 Rust 侧再提供一个 pub extern "C" fn _mb2_entry_32()
//     作为 64 位桩 (UEFI 调用落这里, 直接跳 _start)。
//     这样链接器会:
//       - 若调用者在另一个共享对象里找同名符号: 优先选强符号 (global_asm .globl)
//       - 为避免冲突: 让 global_asm 标签使用 `.hidden` 或仅由链接脚本按段分配,
//         同时在 Rust 侧声明为 weak 不可能。
//     简化: 我们把 _mb2_entry_32 的 64 位桩重命名为 _mb2_entry_64,
//     并修改 boot/loader.rs (boot_kernel 处) — 它从 header.entry 取入口,
//     而 header.entry 是 ELF 的 e_entry 字段 = 链接脚本 ENTRY() = _mb2_entry_32
//     (32 位地址)。但 bootloader 在长模式下调用这个地址, 会在 0x66 前缀解码失败。
//     我们因此 *不改变* ENTRY(_start) 的必要性 — 只需保证 MB2 头在前 32768
//     字节里即可, 而让 ELF e_entry 仍为 _start (64 位)。Multiboot2 引导器不看
//     e_entry, 它只查 header 末尾的 address tag, 我们的 tag 为空 (用 ELF 默认)。
//  综合权衡: 保持 ENTRY(_start), 同时在 32 位入口提供 global 符号 `_mb2_entry_32`;
//  用 QEMU "-kernel" 时额外传 `-kernel` + `-initrd` 不用 + 在 command line
//  用不着; 我们直接用 QEMU "-append" 传不了入口。解决方案: 写 linker script
//  自定义 entry 为 _start (64 位), 但额外在 .multiboot2 段首 12 字节后放一个
//  "entry address tag" (type 3), 指定 entry = _mb2_entry_32。
// ---------------------------------------------------------------------------
#[cfg(target_os = "none")]
core::arch::global_asm!(
    // ------------- Multiboot2 header (放在 .text 开头, 保证在前 32768 字节) -------------
    r#"
    .section .text._mb2hdr, "ax"
    .code32
    .balign 8
    .globl __mb2_header
    .type __mb2_header, @object
__mb2_header:
    .set  MB2_MAGIC, 0xE85250D6
    .set  MB2_ARCH,  0
    .set  MB2_HLEN,  .Lhdr_end - __mb2_header
    .set  MB2_CKSUM, (0xFFFFFFFF - (MB2_MAGIC + MB2_ARCH + MB2_HLEN) + 1)
    .long MB2_MAGIC
    .long MB2_ARCH
    .long MB2_HLEN
    .long MB2_CKSUM

    /* entry address tag — 告诉 MB2 引导器跳到 32 位入口 */
    .short 3
    .short 0
    .long 12
    .long _mb2_entry_32

    /* framebuffer tag (偏好 1024x768x32, 尽力满足) */
    .short 5
    .short 0
    .long 20
    .long 1024
    .long 768
    .long 32

    /* terminator tag */
    .short 0
    .short 0
    .long 8
.Lhdr_end:
    .size __mb2_header, .Lhdr_end - __mb2_header
    .code64
    .previous
    "#,

    // ------------------------- 32→64 过渡代码 (放进 .text.* 合并区) -------------------------
    r#"
    .section .text._mb2entry, "ax"
    .code32
    .balign 4096
    .globl _mb2_entry_32
    .type _mb2_entry_32, @function
_mb2_entry_32:
    cli
    cld

    /* DEBUG MB2 ENTRY: 立即写串口 "MB2\r\n" (不依赖任何初始化). */
    call  .Ldbg_put_mb2

    /* 1) 早期长模式页表: PML4@0x2000, PDPT@0x3000, PD0..3@0x4000..0x6000
     *    注意: 0x7000 是 BootInfo 所在地址, 不能被清零/覆盖.
     *    所以只构造 2 个 PD (PD0@0x4000 覆盖 0..1GiB, PD1@0x5000 覆盖 1GiB..2GiB)
     *    够了: 物理 2GiB 内存, 内核在 1MiB~3MiB, 全部在 PD0/PD1 范围内. */
    lea   edi, [0x2000]
    mov   ecx, (0x6000 - 0x2000) / 4     /* 0x2000..0x6000: PML4 + PDPT + 2 PDs */
    xor   eax, eax
    cld
    rep   stosd

    /* PML4[0] 和 PML4[256] → PDPT@0x3000 */
    mov   DWORD PTR [0x2000],         0x3003
    mov   DWORD PTR [0x2000 + 256*8], 0x3003

    /* PDPT[0] → PD0@0x4000; PDPT[1] → PD1@0x5000; PDPT[2..3] → 暂不填 (>2GiB 不用) */
    mov   DWORD PTR [0x3000 + 0*8], 0x4003
    mov   DWORD PTR [0x3000 + 1*8], 0x5003

    /* 2 个 PD, 每 PD 512 项 × 2MiB huge page → 覆盖 0..2GiB */
    mov   ecx, 0
.Lpd:
    mov   edx, 0x4000
    mov   ebx, ecx
    shl   ebx, 12
    add   edx, ebx
    mov   esi, 0
.Lpd_entry:
    mov   eax, ecx
    shl   eax, 30
    mov   ebx, esi
    shl   ebx, 21
    or    eax, ebx
    or    eax, 0x83                     /* P | RW | PS(huge) */
    mov   DWORD PTR [edx + esi*8 + 0], eax
    mov   DWORD PTR [edx + esi*8 + 4], 0
    inc   esi
    cmp   esi, 512
    jb    .Lpd_entry
    inc   ecx
    cmp   ecx, 2
    jb    .Lpd

    push  'P'  /* P = Page tables built */
    call  .Ldbg_putc_16550

    /* 2) 构造 BootInfo 到 0x7000 (在页表清零之后, 避免被擦除) */
    mov   DWORD PTR [0x7000], 0x4D4F5249     /* magic "MORI" */
    mov   DWORD PTR [0x7004], 2              /* version */
    mov   DWORD PTR [0x7008], 0x000B8000     /* fb_addr = CGA text buffer 0xB8000 */
    mov   DWORD PTR [0x700C], 0
    mov   DWORD PTR [0x7010], 80             /* fb_width  (CGA 文本列数占位) */
    mov   DWORD PTR [0x7014], 25             /* fb_height */
    mov   DWORD PTR [0x7018], 80             /* stride */
    mov   DWORD PTR [0x701C], 32             /* bpp */
    mov   DWORD PTR [0x7020], 0x9000         /* mmap_addr = 0x9000 */
    mov   DWORD PTR [0x7024], 0
    mov   DWORD PTR [0x7028], 2              /* count = 2 */
    mov   DWORD PTR [0x702C], 0
    mov   DWORD PTR [0x7030], 40             /* entry_size */
    mov   DWORD PTR [0x7034], 0

    /* 3) 两条 EFI_MEMORY_DESCRIPTOR → 0x9000 (每 40 字节) */
    xor   eax, eax
    mov   DWORD PTR [0x9000], 7
    mov   DWORD PTR [0x9004], 0
    mov   DWORD PTR [0x9008], 0
    mov   DWORD PTR [0x900C], 0
    mov   DWORD PTR [0x9010], 0
    mov   DWORD PTR [0x9014], 0
    mov   DWORD PTR [0x9018], 0xA0           /* 640K/4K = 160 pages */
    mov   DWORD PTR [0x901C], 0
    mov   DWORD PTR [0x9020], 0xF
    mov   DWORD PTR [0x9024], 0x80000000

    mov   DWORD PTR [0x9028], 7
    mov   DWORD PTR [0x902C], 0
    mov   DWORD PTR [0x9030], 0x100000
    mov   DWORD PTR [0x9034], 0
    mov   DWORD PTR [0x9038], 0
    mov   DWORD PTR [0x903C], 0
    mov   DWORD PTR [0x9040], 0x7FF00        /* (2G-1M)/4K = 524032 */
    mov   DWORD PTR [0x9044], 0
    mov   DWORD PTR [0x9048], 0xF
    mov   DWORD PTR [0x904C], 0x80000000

    push  'B'  /* B = BootInfo written */
    call  .Ldbg_putc_16550

    /* 4) PAE | PGE */
    mov   eax, cr4
    or    eax, (1<<5) | (1<<7)
    mov   cr4, eax

    /* 5) CR3 = PML4 */
    mov   eax, 0x2000
    mov   cr3, eax

    /* 6) EFER.LME = 1 */
    mov   ecx, 0xC0000080
    rdmsr
    or    eax, (1<<8)
    wrmsr

    /* 7) CR0: PG | PE */
    mov   eax, cr0
    or    eax, (1<<31) | (1<<0)
    mov   cr0, eax

    push  'L'  /* L = Long mode enabled (CR0.PG set) */
    call  .Ldbg_putc_16550

    /* 8) LGDT + 远跳进入 64 位代码段 (直接写操作码避免解析器混淆) */
    lgdt  [.Lgdt_desc_mb2]
    .byte 0xEA
    .long .Lmb2_tramp64
    .word 0x08

    /* ---- 64 位 ---- */
    .code64
    .balign 16
.Lmb2_tramp64:
    mov   ax, 0x10
    mov   ds, ax
    mov   es, ax
    mov   ss, ax
    mov   fs, ax
    mov   gs, ax

    /* dbg '6': 进入 64 位 tramp */
    push  rax
    push  rdx
    push  rcx
    mov   edi, '6'
    call  .Ldbg_putc_64
    pop   rcx
    pop   rdx
    pop   rax

    /* rsp = _stack_end
     * 说明: LLVM 集成汇编 Intel 语法下, `mov eax, sym` == `mov eax, [sym]`(取内存!).
     * 要取符号地址(立即数), 必须写 `mov eax, OFFSET sym`. */
    mov   eax, OFFSET _stack_end
    mov   rsp, rax
    and   rsp, ~0xF

    /* dbg 'T': rsp set, about to call _start */
    push  rax
    push  rdx
    push  rcx
    mov   edi, 'T'
    call  .Ldbg_putc_64
    pop   rcx
    pop   rdx
    pop   rax

    mov   eax, OFFSET _start
    call  rax

.Lmb2_hang:
    hlt
    jmp   .Lmb2_hang

    /* ===== 32/64 共用调试串口函数 ===== */
    /* .code32 putc: cdecl, 1 arg on stack. 调用前关中断, 不保存寄存器. */
    .code32
.Ldbg_putc_16550:
    mov   edx, 0x3FD            /* LSR */
.Ldbg_wait32:
    in    al, dx
    test  al, 0x20              /* THRE */
    jz    .Ldbg_wait32
    mov   al, [esp + 4]         /* 取 char (栈参数) */
    mov   edx, 0x3F8            /* THR */
    out   dx, al
    ret   4
.Ldbg_put_mb2:
    push  0x0D
    call  .Ldbg_putc_16550
    push  0x0A
    call  .Ldbg_putc_16550
    push  '2'
    call  .Ldbg_putc_16550
    push  'B'
    call  .Ldbg_putc_16550
    push  'M'
    call  .Ldbg_putc_16550
    ret

    /* .code64 putc: System V AMD64 ABI: arg1 in edi. 不保存寄存器. */
    .code64
.Ldbg_putc_64:
    mov   edx, 0x3FD
.Ldbg_wait64:
    in    al, dx
    test  al, 0x20
    jz    .Ldbg_wait64
    mov   eax, edi
    mov   edx, 0x3F8
    out   dx, al
    ret

    .size _mb2_entry_32, . - _mb2_entry_32
    .code64
    .previous
    "#,

    // ------------------------- 临时 GDT (放入 .rodata.* 合并区) -------------------------
    r#"
    .section .rodata._mb2gdt, "a"
    .balign 16
    .globl _mb2_gdt
    .type _mb2_gdt, @object
_mb2_gdt:
    .quad 0x0000000000000000
    .quad 0x0020980000000000
    .quad 0x0000920000000000
.Lgdt_desc_mb2:
    .word .Lgdt_desc_mb2 - _mb2_gdt - 1
    .long _mb2_gdt
    .size _mb2_gdt, . - _mb2_gdt
    .code64
    .previous
    "#,

    // ------------------------- PVH ELF Note (让 QEMU "-kernel" 在 x86_64 也能直接引导)
    // type = XEN_ELFNOTE_PHYS32_ENTRY = 18 (0x12). data = 32 位物理入口 _mb2_entry_32.
    // 参考 Xen PVH 规范 / QEMU hw/i386/x86_memory_prepare.c:load_elfboot_kernel().
    r#"
    .pushsection .note.Xen, "a", @note
    .balign 4
    /* Elf64_Nhdr: n_namesz, n_descsz, n_type */
    .long 4f - 3f            /* namesz = sizeof("Xen\0") = 4 */
    .long 2f - 1f            /* descz  = 4 */
    .long 18                 /* type   = XEN_ELFNOTE_PHYS32_ENTRY */
3:  .asciz "Xen"
4:  .balign 4
1:  .long _mb2_entry_32     /* 32 位物理入口 */
2:  .balign 4
    .popsection
    .code64
    "#
);


extern crate alloc;
use alloc::boxed::Box;
use alloc::vec::Vec;

// 链接脚本 (.stack 段) 导出的内核栈顶
extern "C" {
    static _stack_end: u8;
    static _sbss: u8;
    static _ebss: u8;
}

/// 早期调试串口单字符输出 (16550 COM1 @ 0x3F8).
/// 在 video::println / serial_put 链路打通之前直接用.
/// 注意: 串口是 I/O 端口, 必须用 in/out 指令, 不能用 ptr read/write (访问内存 0x3F8 毫无意义).
#[inline]
unsafe fn dbg_char(ch: u8) {
    // 轮询 LSR (0x3FD) 的 THRE 位 (bit5)
    let mut lsr: u8;
    for _ in 0..10_000_000u32 {
        core::arch::asm!(
            "in al, dx",
            in("dx") 0x3FDu16,
            out("al") lsr,
            options(nomem, nostack, preserves_flags),
        );
        if lsr & 0x20 != 0 {
            break;
        }
    }
    // 写 THR (0x3F8)
    core::arch::asm!(
        "out dx, al",
        in("dx") 0x3F8u16,
        in("al") ch,
        options(nomem, nostack, preserves_flags),
    );
}

/// 早期调试字符串输出
#[allow(dead_code)]
unsafe fn dbg_str(s: &[u8]) {
    for &b in s {
        dbg_char(b);
    }
}

// ---------------------------------------------------------------------------
// 阶段十: 用户程序加载 (编译产物, 替代阶段九的手写机器码)
// ---------------------------------------------------------------------------
/// 用户程序基址 (P4[1] 用户空间基址), 与 user/linker.ld 的链接地址一致。
const USER_BASE: u64 = memory::paging::USER_SPACE_BASE;
/// 用户栈页虚拟地址。
///
/// 不能紧邻程序镜像 (程序已超 1 页, 会与镜像第二页重叠); 也不得占用
/// `USER_BASE + 0x3000` (用户态 sender/receiver 共享页演示) 与
/// `USER_BASE + 0x10000` 起的 NVMe 配置/MMIO/DMA 区域。故预留 0x8000 起。
const USER_STACK_ADDR: u64 = USER_BASE + 0x8000;
/// 用户栈顶虚拟地址 (栈向下增长)。
const USER_STACK_TOP: u64 = USER_STACK_ADDR + 0x1000;
/// 页大小。
const PAGE_SIZE: u64 = 4096;

/// 编译期嵌入的用户程序扁平二进制 (由 Makefile 先构建 user, 再 objcopy 产出)。
const USER_PROGRAM: &[u8] = include_bytes!("../../build/user/user.bin");

/// 把用户程序加载到指定域: 分配连续物理帧, 映射到 `USER_BASE` 起, 拷贝镜像,
/// 并映射一页用户栈。
fn load_user_program(domain_id: u64) {
    let bytes = USER_PROGRAM;
    let pages = (bytes.len() as u64 + PAGE_SIZE - 1) / PAGE_SIZE;

    for i in 0..pages {
        let frame =
            memory::frame_allocator::allocate_frame().expect("allocate user program frame");
        let vaddr = USER_BASE + i * PAGE_SIZE;
        memory::paging::map_user_page(domain_id, vaddr, frame);

        let start = (i * PAGE_SIZE) as usize;
        let end = core::cmp::min(start + PAGE_SIZE as usize, bytes.len());
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(start),
                frame as *mut u8,
                end - start,
            );
        }
    }

    // 用户栈页。
    let stack_frame =
        memory::frame_allocator::allocate_frame().expect("allocate user stack frame");
    memory::paging::map_user_page(domain_id, USER_STACK_ADDR, stack_frame);
}

/// 空闲任务: 当用户任务退出后兜底运行, 停机等待中断。
extern "C" fn task_idle() {
    // 首次进入时中断仍关闭 (run 未开启中断), 在此开启。
    x86_64::instructions::interrupts::enable();
    loop {
        x86_64::instructions::hlt();
    }
}

/// 内核入口 — 引导器通过 `jmp` 进入, Boot Info 指针在 rdi
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // EARLY DBG: 在任何 Rust 代码运行之前, 直接写串口 "KS\r\n" (进 _start).
    // 这段在栈重设之前, 用寄存器参数, 避免触碰任何未初始化数据.
    unsafe {
        core::arch::asm!(
            "mov dx, 0x3FD",
        "2:",
            "in al, dx",
            "test al, 0x20",
            "jz 2b",
            "mov al, 'K'",
            "mov dx, 0x3F8",
            "out dx, al",

            "mov dx, 0x3FD",
        "2:",
            "in al, dx",
            "test al, 0x20",
            "jz 2b",
            "mov al, 'S'",
            "mov dx, 0x3F8",
            "out dx, al",

            "mov dx, 0x3FD",
        "2:",
            "in al, dx",
            "test al, 0x20",
            "jz 2b",
            "mov al, 0x0D",
            "mov dx, 0x3F8",
            "out dx, al",

            "mov dx, 0x3FD",
        "2:",
            "in al, dx",
            "test al, 0x20",
            "jz 2b",
            "mov al, 0x0A",
            "mov dx, 0x3F8",
            "out dx, al",
        out("rax") _, out("rdx") _,
        options(nomem, nostack)
        );
    }

    // 设置内核栈 + 关中断 (IDT 就绪前不允许中断)
    unsafe {
        core::arch::asm!(
            "mov rsp, {0}",
            "cli",
            in(reg) &_stack_end as *const u8 as u64,
        );
    }

    // DBG: 'A' = stack set, before bss zero
    unsafe { dbg_char(b'A') };

    // 清零 .bss 段 (PVH/QEMU 的 ELF loader 只是按 PT_LOAD 分配内存, 不保证 NOBITS 为 0;
    // 未清零的 BSS 会导致 static mut / spin::Mutex 初值随机, 死锁或崩溃).
    //
    // 注: 避免用 ptr::offset_from (UB 因为不是同一对象内), 直接用 ASM 做地址相减 + rep stosb.
    unsafe {
        let start = &_sbss as *const u8 as u64;
        let end   = &_ebss as *const u8 as u64;
        core::arch::asm!(
            "mov rdi, {start}",
            "mov rcx, {end}",
            "sub rcx, rdi",       // rcx = ebss - sbss (bytes)
            "mov al, 0",
            "cld",
            "rep stosb",
            start = in(reg) start,
            end   = in(reg) end,
            out("rdi") _, out("rcx") _, out("rax") _,
            options(nostack),
        );
    }

    // DBG: 'Z' = Zeroed bss done
    unsafe { dbg_char(b'Z') };

    // 1. 读取并校验 Boot Info
    // DBG: '>' = about to get bootinfo
    unsafe { dbg_char(b'>') };
    let info = bootinfo::get();
    // DBG: '<' = bootinfo magic OK
    unsafe { dbg_char(b'<') };

    // 2. 初始化视频输出
    unsafe { dbg_char(b'V') };
    video::init(info);
    unsafe { dbg_char(b'v') };

    video::println("Morion OS Kernel");
    // DBG: 'M' (after Morion println)
    unsafe { dbg_char(b'm') };
    video::println("Stage 1: CPU initialization");
    video::println("");
    video::println("Boot Info verified (MORI)");

    // 3. GDT + TSS
    arch::gdt::init();
    video::println("[OK] GDT + TSS initialized");

    // 4. IDT
    arch::idt::init();
    video::println("[OK] IDT initialized (#BP / #DF / #PF)");

    // 5. 验证 IDT 工作 — 触发一次断点异常, 处理函数会打印后返回
    video::println("");
    video::println("Testing breakpoint exception...");
    unsafe { core::arch::asm!("int3") };
    video::println("[OK] Returned from #BP handler");

    // ============================================================
    //  阶段二: 物理内存管理
    // ============================================================
    video::println("");
    video::println("Stage 2: Physical memory management");
    memory::frame_allocator::init(info);
    memory::frame_allocator::print_stats();

    // 测试分配 / 释放
    video::println("");
    video::println("Testing frame allocation...");
    let f1 = memory::frame_allocator::allocate_frame();
    let f2 = memory::frame_allocator::allocate_frame();
    match (f1, f2) {
        (Some(a), Some(b)) => {
            video::print("[OK] Allocated frames at 0x");
            video::print_hex(a);
            video::print(" and 0x");
            video::print_hex(b);
            video::println("");
            memory::frame_allocator::free_frame(a);
            memory::frame_allocator::free_frame(b);
            video::println("[OK] Frames freed");
        }
        _ => {
            video::println("[FAIL] Frame allocation returned None");
        }
    }

    video::println("");
    video::println("Stage 2 complete.");

    // ============================================================
    //  阶段三: 虚拟内存与内核堆
    // ============================================================
    video::println("");
    video::println("Stage 3: Virtual memory & kernel heap");
    memory::paging::init();
    video::println("[OK] Paging initialized (identity + offset mapping)");

    // 测试内核堆 (Box / Vec)
    video::println("");
    video::println("Testing heap allocation...");
    let boxed = Box::new(0x2A);
    video::print("[OK] Box::new allocated, value = 0x");
    video::print_hex(*boxed as u64);
    video::println("");

    let mut vec = Vec::new();
    for i in 0..8 {
        vec.push(i);
    }
    video::print("[OK] Vec pushed ");
    video::print_u64(vec.len() as u64);
    video::println(" elements");

    video::println("");
    video::println("Stage 3 complete.");

    // ============================================================
    //  阶段四: 硬件中断框架
    // ============================================================
    video::println("");
    video::println("Stage 4: Hardware interrupts");
    arch::pic::init();
    arch::pit::init();
    arch::keyboard::init();
    video::println("[OK] PIC remapped + PIT timer started (100 Hz)");

    // ============================================================
    //  阶段 4.5: PCI 枚举 (文件系统阶段 0)
    // ============================================================
    video::println("");
    video::println("Stage 4.5: PCI enumeration");
    let pci_devices = arch::pci::enumerate();
    video::print("[OK] PCI devices found: ");
    video::print_u64(pci_devices.len() as u64);
    video::println("");
    for d in &pci_devices {
        video::print("  ");
        video::print_hex(((d.bus as u64) << 8) | ((d.dev as u64) << 3) | d.func as u64);
        video::print("  vend ");
        video::print_hex(d.vendor as u64);
        video::print("  dev ");
        video::print_hex(d.device as u64);
        video::print("  class ");
        video::print_hex(((d.class as u64) << 16) | ((d.subclass as u64) << 8) | d.progif as u64);
        video::println("");
    }

    // ============================================================
    //  阶段十: 用户态运行库 + 可加载用户程序
    // ============================================================
    video::println("");
    video::println("Stage 10: libuser + loadable user program");
    syscall::init();
    video::println("[OK] syscall/sysret enabled (EFER.SCE + STAR + LSTAR)");

    // ============================================================
    //  阶段十一: IPC + 能力系统
    // ============================================================
    scheduler::init();

    // 创建 6 个保护域:
    //   0 = sender    (持有 SendTo(1)+MapInto(1) 能力, 触发按需分页 + call 演示)
    //   1 = receiver  (接收消息)
    //   2 = pager     (分页器, 服务所有域的缺页)
    //   3 = echo      (同步 IPC 服务: recv → reply 回显)
    //   4 = kbd       (用户态键盘驱动, 注册接收 IRQ1)
    //   5 = disk      (用户态 IDE PIO 块设备驱动服务)
    let sender_domain = domain::create();
    let receiver_domain = domain::create();
    let pager_domain = domain::create();
    let echo_domain = domain::create();
    let kbd_domain = domain::create();
    let disk_domain = domain::create();

    // 初始化 IPC 邮箱、能力表与分页器映射 (数量 = 域数量)。
    ipc::init(6);
    cap::init(6);
    pager::init(6, pager_domain);

    // 授权: sender 可向 receiver 发送 + 共享内存。
    cap::grant(sender_domain, cap::Capability::SendTo(receiver_domain));
    cap::grant(sender_domain, cap::Capability::MapInto(receiver_domain));
    // 授权: sender 可向 echo 服务发起同步调用 (Stage 15)。
    cap::grant(sender_domain, cap::Capability::SendTo(echo_domain));
    // 授权: 分页器是全部域的分页器, 授予其向每个域映射匿名帧的能力 (按需分页)。
    for d in [
        sender_domain,
        receiver_domain,
        pager_domain,
        echo_domain,
        kbd_domain,
        disk_domain,
    ] {
        cap::grant(pager_domain, cap::Capability::MapInto(d));
    }
    // 授权: 键盘驱动域注册接收 IRQ1 (Stage 16)。
    cap::grant(kbd_domain, cap::Capability::Irq(1));
    // disk 域走 IDE PIO (固定 I/O 端口), 无需 DMA/MMIO/能力授权。
    video::println("[OK] IPC + capability + pager initialized (6 domains)");

    // 加载用户程序到六个域 (同一镜像, 经 domain_id 参数区分角色)。
    load_user_program(sender_domain);
    load_user_program(receiver_domain);
    load_user_program(pager_domain);
    load_user_program(echo_domain);
    load_user_program(kbd_domain);
    load_user_program(disk_domain);
    video::println("[OK] user program loaded into domains 0 & 1 & 2 & 3 & 4 & 5");

    // 域 0 / 1 / 2 / 3 / 4 / 5 各起一个用户任务。
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, sender_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, receiver_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, pager_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, echo_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, kbd_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, disk_domain);
    // 空闲任务兜底 (归属 sender 域)。
    scheduler::spawn(task_idle, sender_domain);
    video::println("[OK] sender + receiver + pager + echo + kbd + disk + idle tasks spawned");
    video::println("");
    video::println("Expected: sender shares memory with receiver, then");
    video::println("touches an unmapped page to trigger demand paging.");
    video::println("");

    // 交给调度器。首次切换在中断关闭下进行, 避免 enable 与首次调度之间
    // 的竞态 (否则定时器中断会在 run 完成前触发 schedule 抢走主执行流)。
    scheduler::run();
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    if video::ready() {
        video::clear(0x000033);
        video::set_cursor(20, 20);
        video::println("KERNEL PANIC");
        video::println("An unrecoverable error occurred.");
    }
    morion_kernel::halt();
}

// 仅用于 rust-analyzer 在 host 目标上检查时满足 [[bin]] 的 main 要求
// 实际内核编译时 (target_os = "none") 此函数被排除
#[cfg(not(target_os = "none"))]
fn main() {}
