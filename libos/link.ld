/* libOS 链接脚本 — 入口地址固定为 0x4000_0000 (与 EL2 mmu::map 一致) */

ENTRY(_start)

SECTIONS {
    . = 0x40000000;

    .text : {
        *(.text.entry)    /* _start 必须最先 */
        *(.text*)
    }

    .rodata : {
        *(.rodata*)
    }

    .data : {
        *(.data*)
    }

    .bss : {
        *(.bss*)
    }

    /DISCARD/ : {
        *(.comment*)
        *(.note*)
        *(.eh_frame*)
    }
}
