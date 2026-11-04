/*
 * Kalium kernel linker script for QEMU virt (aarch64).
 *
 * QEMU loads an ELF `-kernel` at its own addresses; RAM starts at
 * 0x4000_0000 and the conventional image load address is 0x4008_0000.
 * The kernel is identity mapped, so virtual == physical everywhere.
 *
 * Symbols exported for the HAL and the memory manager:
 *   __kernel_start / __kernel_end   the image, bss and boot stack
 *   __bss_start / __bss_end         zeroed by the boot code (16-byte aligned)
 *   __stack_bottom / __stack_top    the 1 MiB boot stack
 *   __heap_start / __heap_end       free RAM after the image, up to RAM_SIZE
 *
 * RAM_SIZE defaults to 128 MiB; override with `--defsym=RAM_SIZE=...`.
 */

ENTRY(_start)

RAM_START = 0x40000000;
RAM_SIZE = DEFINED(RAM_SIZE) ? RAM_SIZE : 128M;
LOAD_ADDRESS = 0x40080000;
BOOT_STACK_SIZE = 1M;

SECTIONS
{
    . = LOAD_ADDRESS;
    __kernel_start = .;

    .text : ALIGN(4K)
    {
        KEEP(*(.text.boot))
        . = ALIGN(2K);
        KEEP(*(.text.vectors))
        *(.text .text.*)
    }

    .rodata : ALIGN(4K)
    {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    .data : ALIGN(4K)
    {
        *(.data .data.*)
        *(.sdata .sdata.*)
        *(.got .got.*)
    }

    .bss (NOLOAD) : ALIGN(16)
    {
        __bss_start = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    .stack (NOLOAD) : ALIGN(4K)
    {
        __stack_bottom = .;
        . += BOOT_STACK_SIZE;
        . = ALIGN(16);
        __stack_top = .;
    }

    . = ALIGN(4K);
    __kernel_end = .;

    __heap_start = ALIGN(2M);
    __heap_end = RAM_START + RAM_SIZE;

    /DISCARD/ :
    {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
        *(.ARM.exidx .ARM.exidx.*)
    }
}

ASSERT(__kernel_end <= __heap_end, "kernel image does not fit in RAM_SIZE")
ASSERT(__heap_start < __heap_end, "no heap left after the kernel image")
