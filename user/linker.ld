/* Morion OS 用户态程序链接脚本
 * 链接到用户空间基址 USER_SPACE_BASE (P4[1] = 0x8000_0000_0000)。
 * 入口 _start 必须位于镜像最前端 (offset 0), 内核据此加载。
 * 注意: 此地址须与 kernel/src/memory/paging.rs 的 USER_SPACE_BASE 保持一致。
 */

ENTRY(_start)

SECTIONS {
    . = 0x8000000000;

    .text : {
        *(.text._start)
        *(.text .text.*)
    }

    .rodata : {
        *(.rodata .rodata.*)
        /* code-model=large 下 LLVM 会生成 .lrodata (大模型只读数据), 须显式并入,
           否则会被作为 orphan 段放到 .text 之前, 破坏 _start 的 offset 0 约定。 */
        *(.lrodata .lrodata.*)
    }

    .data : {
        *(.data .data.*)
        *(.ldata .ldata.*)
    }

    .bss : {
        *(.bss .bss.*)
        *(.lbss .lbss.*)
    }
}
