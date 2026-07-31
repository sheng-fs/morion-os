/* ============================================================
 * Morion OS 微内核 — x86_64 链接脚本
 *
 * 布局设计 (参照 Linux 内核 + 微内核需求):
 *   1MB 起始 — 预留 BIOS/引导器区域
 *   2MB 起始 — 内核代码 (.text)
 *   之后       — 内核数据 (.rodata, .data, .bss)
 *   -2MB 区域 — 临时页表 (引导早期)
 *
 * 兼容 UEFI 64-bit PE 入口约定
 * ============================================================ */

ENTRY(_kernel_start)

/* 从 UEFI 引导的 x86_64 内核: 加载到高端物理地址 */
KERNEL_PHYS_BASE = 0xFFFFFFFF80000000;
KERNEL_LOAD_OFFSET = 0x200000;  /* 2MB */

SECTIONS
{
    /* 内核虚拟地址偏移 */
    . = KERNEL_PHYS_BASE + KERNEL_LOAD_OFFSET;

    /* =========================================================
     * .text — 只读可执行代码段
     * ========================================================= */
    .text ALIGN(4K) : AT(ADDR(.text) - KERNEL_PHYS_BASE)
    {
        _text_start = .;
        *(.text .text.*)
        *(.text._*)
        _text_end = .;
    }

    /* =========================================================
     * .rodata — 只读数据 (常量, 字符串表, 虚表)
     * ========================================================= */
    .rodata ALIGN(4K) : AT(ADDR(.rodata) - KERNEL_PHYS_BASE)
    {
        _rodata_start = .;
        *(.rodata .rodata.*)
        *(.rodata._*)

        /* Rust panic 信息 */
        *(.rust_main_panic.*)

        /* 异常表 (内核展开信息) */
        __start___ex_table = .;
        KEEP(*(__ex_table))
        __stop___ex_table = .;

        _rodata_end = .;
    }

    /* =========================================================
     * .data — 已初始化的读写数据
     * ========================================================= */
    .data ALIGN(4K) : AT(ADDR(.data) - KERNEL_PHYS_BASE)
    {
        _data_start = .;
        *(.data .data.*)
        *(.data._*)

        /* 内核初始化调用表 */
        __initcall_start = .;
        KEEP(*(.initcall.text))
        __initcall_end = .;

        _data_end = .;
    }

    /* =========================================================
     * .bss — 未初始化数据 (启动时清零)
     * ========================================================= */
    .bss ALIGN(4K) : AT(ADDR(.bss) - KERNEL_PHYS_BASE)
    {
        _bss_start = .;
        *(.bss .bss.*)
        *(.bss._*)
        *(COMMON)
        _bss_end = .;
    }

    /* =========================================================
     * 内核初始栈 (引导早期使用)
     * ========================================================= */
    .stack ALIGN(16) : AT(ADDR(.stack) - KERNEL_PHYS_BASE)
    {
        _stack_start = .;
        . += 64K;               /* 64KB 初始内核栈 */
        _stack_top = .;
    }

    /* =========================================================
     * 丢弃不需要的段
     * ========================================================= */
    /DISCARD/ :
    {
        *(.comment)
        *(.note)
        *(.note.*)
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.debug_*)
    }

    /* =========================================================
     * 内核映像结束标记
     * ========================================================= */
    _kernel_end = .;
    _kernel_physical_end = . - KERNEL_PHYS_BASE;
    _kernel_size = _kernel_end - _text_start;

    /* =========================================================
     * 导出符号 (供引导器使用)
     * ========================================================= */
    _kernel_load_addr = KERNEL_PHYS_BASE + KERNEL_LOAD_OFFSET;
    _kernel_phys_entry = _kernel_start - KERNEL_PHYS_BASE;
}
