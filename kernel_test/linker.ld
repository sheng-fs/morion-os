/* 测试内核链接脚本
 * 加载到物理地址 0x100000 (1MB)
 * 引导器负责将各段复制到正确的物理地址
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

    /* 内核栈 */
    .stack (NOLOAD) : {
        . = ALIGN(16);
        _stack_start = .;
        . += 64K;
        _stack_end = .;
    }
}