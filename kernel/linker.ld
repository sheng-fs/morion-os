/* Morion OS 微内核链接脚本 (阶段一)
 * 加载到物理地址 0x100000 (1MB)
 * 引导器负责将各段复制到正确的物理地址 (paddr == vaddr)
 *
 * 为支持 QEMU "-kernel" 直接启动 (Multiboot2), 保证
 * Multiboot2 header 位于 ELF 前 32768 字节内 (因此放在最开头)。
 */

ENTRY(_start)

SECTIONS {
    . = 0x100000;

    /* PVH / Multiboot2 ELF notes — 必须保留, QEMU "-kernel" 需要扫描 .note.Xen */
    .note : ALIGN(4) {
        KEEP(*(.note .note.* .note.Xen .note.gnu.build-id))
    }

    /* Multiboot2 header: 放在 .text 最开头, 保证在前 32768 字节内 */
    .text : {
        KEEP(*(.text._mb2hdr .text._mb2hdr.*))
        *(.mb2hdr .mb2hdr.*)
        *(.text .text.*)
        *(.mb2entry .mb2entry.*)
    }

    .rodata : {
        *(.mb2data .mb2data.*)
        *(.rodata .rodata.*)
    }

    .data : {
        *(.data .data.*)
    }

    .bss : {
        _sbss = .;
        *(.bss .bss.*)
        . = ALIGN(16);
        _ebss = .;
    }

    /* 内核初始栈 (引导早期使用) */
    .stack (NOLOAD) : {
        . = ALIGN(16);
        _stack_start = .;
        . += 64K;
        _stack_end = .;
    }

    /* 内核镜像结束 (含栈), 内存分配器保留其下所有帧 */
    _kernel_end = .;
}
