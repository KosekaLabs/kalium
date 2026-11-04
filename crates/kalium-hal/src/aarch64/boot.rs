//! Boot entry point.
//!
//! `_start` is the ELF entry. QEMU `virt -kernel` jumps here on core 0 with
//! the device tree address in `x0`, at EL1 by default or at EL2 when run with
//! `-machine virtualization=on`. The code below:
//!
//! 1. parks every core except core 0 (`MPIDR_EL1.Aff0 == 0`);
//! 2. if at EL2, configures EL1 as AArch64 with access to the physical timer
//!    and counter, then drops to EL1h with exceptions masked;
//! 3. enables FP/SIMD at EL1 so that compiler-generated code may use it;
//! 4. sets the boot stack, zeroes `.bss`, records the DTB pointer and boot
//!    EL, and calls `kmain`, which must not return.
//!
//! The linker script (`kernel/link.ld`) provides `__stack_top`, `__bss_start`,
//! `__bss_end`, `__heap_start`, `__heap_end`, `__kernel_start` and
//! `__kernel_end`.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// Device tree address passed by the firmware/QEMU in `x0`. Written by the
/// boot code before `kmain`.
#[no_mangle]
static KALIUM_BOOT_DTB: AtomicU64 = AtomicU64::new(0);

/// Exception level the kernel was entered at (1 or 2). Written by boot code.
#[no_mangle]
static KALIUM_BOOT_EL: AtomicU64 = AtomicU64::new(0);

global_asm!(
    r#"
    .section .text.boot, "ax"
    .global _start
    .type _start, %function
_start:
    // Park every core but core 0.
    mrs     x1, mpidr_el1
    and     x1, x1, #0xff
    cbz     x1, 1f
0:  wfe
    b       0b

1:  // Keep the DTB pointer and the entry EL for kmain.
    mov     x19, x0
    mrs     x20, CurrentEL
    lsr     x20, x20, #2

    cmp     x20, #2
    b.ne    2f

    // ---- Entered at EL2: configure EL1 and drop into it. ----
    // HCR_EL2.RW = 1: EL1 runs AArch64.
    mov     x1, #(1 << 31)
    msr     hcr_el2, x1
    // Let EL1 use the physical counter and timer; zero the virtual offset.
    mrs     x1, cnthctl_el2
    orr     x1, x1, #3
    msr     cnthctl_el2, x1
    msr     cntvoff_el2, xzr
    // CPTR_EL2: RES1 bits set, TFP clear, so FP/SIMD is not trapped to EL2.
    mov     x1, #0x33ff
    msr     cptr_el2, x1
    // SCTLR_EL1: reset value (RES1 bits), MMU and caches off.
    mov     x1, #0x0800
    movk    x1, #0x30d0, lsl #16
    msr     sctlr_el1, x1
    // Return to EL1h with D, A, I, F masked.
    mov     x1, #0x3c5
    msr     spsr_el2, x1
    adr     x1, 2f
    msr     elr_el2, x1
    eret

2:  // ---- EL1 ----
    msr     spsel, #1
    // CPACR_EL1.FPEN = 0b11: no FP/SIMD traps at EL1/EL0.
    mov     x1, #(3 << 20)
    msr     cpacr_el1, x1
    isb

    // Boot stack. Must be set before touching any memory from Rust and
    // before the .bss clear loop (which does not use the stack, but kmain
    // immediately will).
    adrp    x1, __stack_top
    add     x1, x1, :lo12:__stack_top
    mov     sp, x1

    // Zero .bss (16-byte aligned by the linker script).
    adrp    x1, __bss_start
    add     x1, x1, :lo12:__bss_start
    adrp    x2, __bss_end
    add     x2, x2, :lo12:__bss_end
3:  cmp     x1, x2
    b.hs    4f
    stp     xzr, xzr, [x1], #16
    b       3b

4:  // Record boot information (after the .bss clear, which would wipe it).
    adrp    x1, KALIUM_BOOT_DTB
    add     x1, x1, :lo12:KALIUM_BOOT_DTB
    str     x19, [x1]
    adrp    x1, KALIUM_BOOT_EL
    add     x1, x1, :lo12:KALIUM_BOOT_EL
    str     x20, [x1]

    bl      kmain
5:  wfe
    b       5b
    .size _start, . - _start
"#
);

extern "C" {
    static __heap_start: u8;
    static __heap_end: u8;
    static __kernel_start: u8;
    static __kernel_end: u8;
}

/// Start of RAM on QEMU `virt`, where it places the device tree when it
/// boots a non-Linux ELF image without passing the address in `x0`.
const RAM_START: usize = 0x4000_0000;
/// Flattened device tree magic, big-endian `0xd00dfeed`.
const FDT_MAGIC: u32 = 0xd00d_feed;

/// Physical address of the device tree blob: the value passed in `x0`, or
/// the FDT found at the start of RAM when `x0` was zero (QEMU `-kernel` with
/// an ELF image), or 0 if there is none.
pub fn dtb_address() -> usize {
    let passed = KALIUM_BOOT_DTB.load(Ordering::Relaxed) as usize;
    if passed != 0 {
        return passed;
    }
    // SAFETY: RAM_START is valid identity-mapped RAM on QEMU virt and we
    // only read one aligned word from it.
    let magic = unsafe { core::ptr::read_volatile(RAM_START as *const u32) };
    if u32::from_be(magic) == FDT_MAGIC {
        RAM_START
    } else {
        0
    }
}

/// The exception level the kernel was entered at (1 or 2).
pub fn boot_el() -> u8 {
    KALIUM_BOOT_EL.load(Ordering::Relaxed) as u8
}

/// `[__heap_start, __heap_end)` from the linker script: RAM after the kernel
/// image and boot stack, up to the configured RAM size.
pub fn heap_region() -> core::ops::Range<usize> {
    core::ptr::addr_of!(__heap_start) as usize..core::ptr::addr_of!(__heap_end) as usize
}

/// `[__kernel_start, __kernel_end)`: the kernel image including bss and
/// the boot stack.
pub fn kernel_image_range() -> core::ops::Range<usize> {
    core::ptr::addr_of!(__kernel_start) as usize..core::ptr::addr_of!(__kernel_end) as usize
}
