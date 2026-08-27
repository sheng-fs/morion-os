/* Morion OS 微内核链接脚本 (阶段一)
 * 加载到物理地址 0x100000 (1MB)
 * 引导器负责将各段复制到正确的物理地址 (paddr == vaddr)
 */

ENTRY(_start)

SECTIONS {
    . = 0x100000;

    .text : {
        *(.text .text.*)
    }

    .rodata : {
        *(.rodata .rodata.*)
    }

    .data : {
        *(.data .data.*)
    }

    .bss : {
        *(.bss .bss.*)
    }

    /* 内核初始栈 (引导早期使用) */
    .stack (NOLOAD) : {
        . = ALIGN(16);
        _stack_start = .;
        . += 64K;
        _stack_end = .;
    }
}