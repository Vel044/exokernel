/* libOS 链接脚本 — 入口地址固定为 0x4000_0000 (与 EL2 mmu::map 一致) */

ENTRY(_start)

SECTIONS {
    . = 0x40000000;

    .text : {
        *(.text.entry)    /* _start 必须最先 */
        *(.text*)
    }

    . = ALIGN(0x1000);
    .rodata : {
        *(.rodata*)
    }

    . = ALIGN(0x1000);
    .data : {
        *(.data*)
    }

    . = ALIGN(0x1000);
    .bss : {
        *(.bss*)
    }

    /DISCARD/ : {
        *(.comment*)
        *(.note*)
        *(.eh_frame*)
    }
}
