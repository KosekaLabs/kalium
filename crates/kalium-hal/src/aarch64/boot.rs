//! Boot entry point and the kernel's memory layout.
//!
//! `_start` is the ELF entry. QEMU `virt -kernel` jumps here on core 0, at
//! EL1 by default or at EL2 when run with `-machine virtualization=on`. The
//! code below:
//!
//! 1. parks every core except core 0 (`MPIDR_EL1.Aff0 == 0`);
//! 2. if at EL2, configures EL1 as AArch64 with access to the physical timer
//!    and counter, then drops to EL1 with exceptions masked;
//! 3. enables FP/SIMD at EL1 so that compiler-generated code may use it;
//! 4. sets `SP_EL1` to the exception stack and `SP_EL0` to the boot stack,
//!    selects `SP_EL0` (EL1t) for the kernel, zeroes `.bss`, records the
//!    value of `x0` at entry and the boot EL, and calls `kmain`, which must
//!    not return.
//!
//! Running the kernel on `SP_EL0` means every exception taken at EL1 switches
//! to `SP_EL1`, a separate stack with its own guard: a kernel stack overflow
//! hits the unmapped guard below the boot stack and the fault is reported
//! from a stack that is still intact instead of double-faulting silently.
//!
//! # The device tree
//!
//! A Linux-style loader passes the device tree address in `x0`; QEMU's
//! `-kernel` with an ELF image does not run that boot stub, so `x0` holds its
//! reset value 0 and QEMU instead installs the device tree at the start of
//! RAM, provided the 1 MiB it reserves for it fits below the lowest loaded
//! segment. The linker script loads the image 2 MiB above RAM for exactly
//! that reason. [`dtb_address`] honours `x0` when it points at a blob and
//! otherwise looks at the start of RAM.
//!
//! The linker script (`kernel/link.ld`) provides the section symbols exposed
//! through the `*_range` functions here; every boundary is page aligned.

use core::arch::global_asm;
use core::ops::Range;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::fdt;

/// Value of `x0` at entry (the device tree address when a loader passes one).
/// Written by the boot code before `kmain`.
#[no_mangle]
static KALIUM_BOOT_X0: AtomicU64 = AtomicU64::new(0);

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

1:  // Keep x0 (the DTB pointer, if a loader passed one) and the entry EL.
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
    // CPACR_EL1.FPEN = 0b11: no FP/SIMD traps at EL1/EL0.
    mov     x1, #(3 << 20)
    msr     cpacr_el1, x1
    isb

    // Stacks. SP_EL1 (selected while SPSel = 1) is the exception stack;
    // SP_EL0 is the boot stack the kernel runs on. Both must be set before
    // touching any memory from Rust; the .bss clear loop below does not use
    // the stack, but kmain immediately will.
    msr     spsel, #1
    adrp    x1, __exc_stack_top
    add     x1, x1, :lo12:__exc_stack_top
    mov     sp, x1
    msr     spsel, #0
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
    adrp    x1, KALIUM_BOOT_X0
    add     x1, x1, :lo12:KALIUM_BOOT_X0
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
    static __kernel_start: u8;
    static __kernel_end: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __data_end: u8;
    static __stack_guard_start: u8;
    static __stack_guard_end: u8;
    static __stack_bottom: u8;
    static __stack_top: u8;
    static __exc_stack_guard_start: u8;
    static __exc_stack_guard_end: u8;
    static __exc_stack_bottom: u8;
    static __exc_stack_top: u8;
    static __heap_start: u8;
    static __heap_end: u8;
}

/// Start of RAM on QEMU `virt`.
pub const RAM_START: usize = 0x4000_0000;

/// End of the address range the identity map can cover (see `mmu.rs`): RAM
/// beyond it is not used.
pub const MAPPED_LIMIT: usize = 4 << 30;

/// The MMU's page size; [`ram_info`] reports the end of RAM at this
/// granularity so the identity map can end exactly there.
pub const PAGE_SIZE: usize = 4 << 10;

fn sym(s: &u8) -> usize {
    core::ptr::addr_of!(*s) as usize
}

macro_rules! range_fn {
    ($(#[$doc:meta])* $name:ident, $start:ident, $end:ident) => {
        $(#[$doc])*
        pub fn $name() -> Range<usize> {
            // SAFETY: linker-provided symbols; only their addresses are used.
            unsafe { sym(&$start)..sym(&$end) }
        }
    };
}

range_fn!(
    /// `[__kernel_start, __kernel_end)`: the whole image including `.bss`,
    /// the stacks and their guard pages.
    kernel_image_range, __kernel_start, __kernel_end
);
range_fn!(
    /// The code: `.text.boot`, the vector table and all functions. Mapped
    /// read-only and executable.
    text_range, __text_start, __text_end
);
range_fn!(
    /// Constants and the embedded boot image. Mapped read-only, never
    /// executable.
    rodata_range, __rodata_start, __rodata_end
);
range_fn!(
    /// `.data` and `.bss`. Mapped read-write, never executable.
    data_range, __data_start, __data_end
);
range_fn!(
    /// The unmapped pages directly below the boot stack.
    boot_stack_guard_range, __stack_guard_start, __stack_guard_end
);
range_fn!(
    /// The 1 MiB boot stack the kernel runs on (`SP_EL0`).
    boot_stack_range, __stack_bottom, __stack_top
);
range_fn!(
    /// The unmapped pages directly below the exception stack.
    exception_stack_guard_range, __exc_stack_guard_start, __exc_stack_guard_end
);
range_fn!(
    /// The stack every exception taken at EL1 runs on (`SP_EL1`).
    exception_stack_range, __exc_stack_bottom, __exc_stack_top
);

/// `[RAM_START, __kernel_start)`: where QEMU installs the device tree. Mapped
/// read-only, never executable.
pub fn dtb_area_range() -> Range<usize> {
    RAM_START..kernel_image_range().start
}

/// The value of `x0` at entry.
pub fn boot_x0() -> u64 {
    KALIUM_BOOT_X0.load(Ordering::Relaxed)
}

/// The exception level the kernel was entered at (1 or 2).
pub fn boot_el() -> u8 {
    KALIUM_BOOT_EL.load(Ordering::Relaxed) as u8
}

/// True if `addr` is inside RAM the identity map covers, so reading a word
/// from it cannot fault.
fn in_mapped_ram(addr: usize) -> bool {
    (RAM_START..MAPPED_LIMIT).contains(&addr) && addr.is_multiple_of(4)
}

/// Physical address of the device tree blob: `x0` when it points at one, or
/// the start of RAM when QEMU put it there, or 0 if there is none.
pub fn dtb_address() -> usize {
    let passed = boot_x0() as usize;
    for candidate in [passed, RAM_START] {
        if candidate != 0 && in_mapped_ram(candidate) {
            // SAFETY: `candidate` is an aligned address inside identity-mapped
            // RAM and only one word is read.
            let magic = unsafe { core::ptr::read_volatile(candidate as *const u32) };
            if fdt::is_fdt_magic(magic) {
                return candidate;
            }
        }
    }
    0
}

/// Where the size of RAM came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RamSource {
    /// The `/memory` node of the device tree at [`RamInfo::dtb`].
    DeviceTree,
    /// No usable device tree: the linker script's `RAM_SIZE` (128 MiB).
    LinkerDefault,
}

/// The RAM the kernel may use, as determined once by [`ram_info`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RamInfo {
    /// `[start, end)` of the RAM bank containing the kernel image, clamped
    /// to [`MAPPED_LIMIT`] and rounded down to [`PAGE_SIZE`] (a partial
    /// page cannot be mapped). The identity map ends exactly at `end`.
    pub start: usize,
    pub end: usize,
    pub source: RamSource,
    /// Address of the device tree, 0 if none was found.
    pub dtb: usize,
    /// `totalsize` of the device tree, 0 if none.
    pub dtb_size: usize,
}

impl RamInfo {
    pub fn size(&self) -> usize {
        self.end - self.start
    }
}

/// Cached end of RAM (0 = not determined yet) and how it was determined.
static RAM_END: AtomicUsize = AtomicUsize::new(0);
static RAM_FROM_DTB: AtomicUsize = AtomicUsize::new(0);
static DTB_SIZE: AtomicUsize = AtomicUsize::new(0);

/// How many bytes the device tree reader may touch from `dtb`: up to the
/// kernel image when the blob sits below it (QEMU's placement), otherwise
/// the reader's own limit.
fn dtb_limit(dtb: usize) -> usize {
    let image = kernel_image_range().start;
    if dtb < image {
        (image - dtb).min(fdt::MAX_SIZE)
    } else {
        fdt::MAX_SIZE
    }
}

/// Size RAM from the device tree, falling back to the linker default.
/// Evaluated once, before the MMU is on; later calls return the cached
/// answer.
pub fn ram_info() -> RamInfo {
    let dtb = dtb_address();
    let cached = RAM_END.load(Ordering::Relaxed);
    if cached != 0 {
        let source = if RAM_FROM_DTB.load(Ordering::Relaxed) != 0 {
            RamSource::DeviceTree
        } else {
            RamSource::LinkerDefault
        };
        return RamInfo {
            start: RAM_START,
            end: cached,
            source,
            dtb,
            dtb_size: DTB_SIZE.load(Ordering::Relaxed),
        };
    }

    let image = kernel_image_range();
    let page_floor = |end: usize| end & !(PAGE_SIZE - 1);
    // SAFETY: linker-provided symbol; only its address is used.
    let fallback_end = page_floor(unsafe { sym(&__heap_end) });
    let mut dtb_size = 0;
    let from_dtb = if dtb == 0 {
        None
    } else {
        // SAFETY: `dtb` is a non-zero address inside identity-mapped RAM
        // (either passed by the loader or found at RAM_START), nothing
        // writes the blob, and the reader never touches memory past
        // `dtb_limit`.
        unsafe { fdt::Fdt::from_raw(dtb as *const u8, dtb_limit(dtb)) }.and_then(|f| {
            dtb_size = f.total_size();
            f.memory_containing(image.start as u64)
        })
    };
    let (end, source) = match from_dtb {
        // The bank must start at RAM_START (where the identity map's normal
        // memory begins) and leave room after the image to be usable.
        Some(bank) if bank.start as usize == RAM_START => {
            let end = page_floor(bank.end.min(MAPPED_LIMIT as u64) as usize);
            if end > image.end {
                (end, RamSource::DeviceTree)
            } else {
                (fallback_end, RamSource::LinkerDefault)
            }
        }
        _ => (fallback_end, RamSource::LinkerDefault),
    };
    DTB_SIZE.store(dtb_size, Ordering::Relaxed);
    RAM_FROM_DTB.store(
        usize::from(source == RamSource::DeviceTree),
        Ordering::Relaxed,
    );
    RAM_END.store(end, Ordering::Relaxed);
    RamInfo {
        start: RAM_START,
        end,
        source,
        dtb,
        dtb_size,
    }
}

/// `[__heap_start, end of RAM)`: RAM after the kernel image, up to the size
/// the device tree reports (or the linker default without one).
pub fn heap_region() -> Range<usize> {
    // SAFETY: linker-provided symbol; only its address is used.
    let start = unsafe { sym(&__heap_start) };
    start..ram_info().end.max(start)
}
