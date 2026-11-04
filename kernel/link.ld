/*
 * Kalium kernel linker script for QEMU virt (aarch64).
 *
 * RAM starts at 0x4000_0000. The image is loaded 2 MiB above that, at
 * 0x4020_0000, and not at the conventional 0x4008_0000: QEMU `-kernel` with a
 * non-Linux ELF image installs the device tree at the start of RAM only if
 * its 1 MiB FDT buffer fits below the lowest loaded segment, so an image at
 * 0x4008_0000 (512 KiB above RAM) gets no device tree at all. The kernel is
 * identity mapped, so virtual == physical everywhere.
 *
 * Every section boundary below is 4 KiB aligned so that the HAL can map the
 * image with precise page attributes (W^X): .text read-only and executable,
 * .rodata read-only and never executable, .data/.bss/stacks read-write and
 * never executable, the stack guards unmapped.
 *
 * Symbols exported for the HAL and the memory manager:
 *   __kernel_start / __kernel_end             the whole image (2 MiB aligned start)
 *   __text_start / __text_end                 boot code, vectors, all functions
 *   __rodata_start / __rodata_end             constants and the embedded boot image
 *   __data_start / __data_end                 .data and .bss
 *   __bss_start / __bss_end                   zeroed by the boot code (16-byte aligned)
 *   __stack_guard_start / __stack_guard_end   unmapped pages below the boot stack
 *   __stack_bottom / __stack_top              the 1 MiB boot stack (SP_EL0)
 *   __exc_stack_guard_start / __exc_stack_guard_end   unmapped pages below the
 *                                             exception stack
 *   __exc_stack_bottom / __exc_stack_top      the exception stack (SP_EL1)
 *   __heap_start / __heap_end                 free RAM after the image, up to
 *                                             RAM_SIZE; the HAL replaces __heap_end
 *                                             with the device tree's memory size
 *                                             when a device tree is present
 *
 * RAM_SIZE defaults to 128 MiB; override with `--defsym=RAM_SIZE=...`. It is
 * only the fallback for a boot without a device tree.
 *
 * MAPPED_IMAGE_LIMIT bounds the region the HAL maps with 4 KiB pages; it must
 * match `IMAGE_L3_TABLES * 2 MiB` in crates/kalium-hal/src/aarch64/mmu.rs.
 */

ENTRY(_start)

RAM_START = 0x40000000;
RAM_SIZE = DEFINED(RAM_SIZE) ? RAM_SIZE : 128M;
LOAD_ADDRESS = 0x40200000;
BOOT_STACK_SIZE = 1M;
EXC_STACK_SIZE = 64K;
GUARD_SIZE = 16K;
MAPPED_IMAGE_LIMIT = LOAD_ADDRESS + 8M;

SECTIONS
{
    . = LOAD_ADDRESS;
    __kernel_start = .;

    .text : ALIGN(4K)
    {
        __text_start = .;
        KEEP(*(.text.boot))
        . = ALIGN(2K);
        KEEP(*(.text.vectors))
        *(.text .text.*)
        . = ALIGN(4K);
        __text_end = .;
    }

    .rodata : ALIGN(4K)
    {
        __rodata_start = .;
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
        . = ALIGN(4K);
        __rodata_end = .;
    }

    .data : ALIGN(4K)
    {
        __data_start = .;
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
        . = ALIGN(4K);
        __data_end = .;
    }

    .stack (NOLOAD) : ALIGN(4K)
    {
        __stack_guard_start = .;
        . += GUARD_SIZE;
        __stack_guard_end = .;
        __stack_bottom = .;
        . += BOOT_STACK_SIZE;
        __stack_top = .;
        __exc_stack_guard_start = .;
        . += GUARD_SIZE;
        __exc_stack_guard_end = .;
        __exc_stack_bottom = .;
        . += EXC_STACK_SIZE;
        __exc_stack_top = .;
    }

    . = ALIGN(4K);
    __kernel_end = .;

    __heap_start = __kernel_end;
    __heap_end = RAM_START + RAM_SIZE;

    /DISCARD/ :
    {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
        *(.ARM.exidx .ARM.exidx.*)
    }
}

ASSERT(__kernel_start % 2M == 0, "kernel image must start on a 2 MiB boundary")
ASSERT(__kernel_end <= __heap_end, "kernel image does not fit in RAM_SIZE")
ASSERT(__heap_start < __heap_end, "no heap left after the kernel image")
ASSERT(__kernel_end <= MAPPED_IMAGE_LIMIT, "kernel image exceeds the 4 KiB-mapped region; raise IMAGE_L3_TABLES in mmu.rs and MAPPED_IMAGE_LIMIT")
ASSERT(__stack_top % 16 == 0, "boot stack top must be 16-byte aligned")
ASSERT(__exc_stack_top % 16 == 0, "exception stack top must be 16-byte aligned")
