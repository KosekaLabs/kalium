//! CPU primitives: interrupt masking, barriers, `wfi`, system registers.

use core::arch::asm;

/// Read a system register.
macro_rules! read_sysreg {
    ($reg:literal) => {{
        let v: u64;
        // SAFETY: reading a system register has no side effects on memory.
        unsafe { ::core::arch::asm!(concat!("mrs {0}, ", $reg), out(reg) v, options(nomem, nostack)) };
        v
    }};
}

/// Write a system register. The caller is responsible for the semantics.
macro_rules! write_sysreg {
    ($reg:literal, $val:expr) => {{
        let v: u64 = $val;
        // SAFETY: the caller chose the register and value; writes to system
        // registers do not touch Rust-visible memory.
        unsafe { ::core::arch::asm!(concat!("msr ", $reg, ", {0}"), in(reg) v, options(nomem, nostack)) };
    }};
}

pub(crate) use read_sysreg;
pub(crate) use write_sysreg;

/// Bit 7 of DAIF (the `I` flag): IRQs masked when set.
const DAIF_IRQ: u64 = 1 << 7;

/// Wait for an interrupt (`wfi`). Returns after any interrupt, even if it is
/// masked.
#[inline(always)]
pub fn wait_for_interrupt() {
    // SAFETY: `wfi` only halts the core until an event; no memory effects.
    unsafe { asm!("wfi", options(nomem, nostack)) };
}

/// Unmask IRQs at the current exception level.
#[inline(always)]
pub fn enable_irqs() {
    // SAFETY: changing DAIF has no memory effects. No `nomem` so it also acts
    // as a compiler barrier.
    unsafe { asm!("msr daifclr, #2", options(nostack)) };
}

/// Mask IRQs at the current exception level.
#[inline(always)]
pub fn disable_irqs() {
    // SAFETY: as in `enable_irqs`.
    unsafe { asm!("msr daifset, #2", options(nostack)) };
}

/// True if IRQs are currently unmasked.
#[inline(always)]
pub fn irqs_enabled() -> bool {
    read_sysreg!("daif") & DAIF_IRQ == 0
}

/// Mask IRQs and return the previous DAIF so it can be restored.
#[inline(always)]
pub fn save_and_disable_irqs() -> u64 {
    let daif = read_sysreg!("daif");
    disable_irqs();
    daif
}

/// Restore DAIF saved by [`save_and_disable_irqs`].
#[inline(always)]
pub fn restore_irqs(daif: u64) {
    // SAFETY: as in `enable_irqs`.
    unsafe { asm!("msr daif, {0}", in(reg) daif, options(nostack)) };
}

/// The current exception level (0..=3).
pub fn current_el() -> u8 {
    ((read_sysreg!("CurrentEL") >> 2) & 0b11) as u8
}

/// Instruction synchronization barrier.
#[inline(always)]
pub fn isb() {
    // SAFETY: barrier, no memory effects visible to Rust.
    unsafe { asm!("isb", options(nostack)) };
}

/// Full-system data synchronization barrier.
#[inline(always)]
pub fn dsb_sy() {
    // SAFETY: barrier.
    unsafe { asm!("dsb sy", options(nostack)) };
}

/// Spin hint.
#[inline(always)]
pub fn spin_hint() {
    core::hint::spin_loop();
}

/// Park the core forever with IRQs masked.
pub fn halt() -> ! {
    disable_irqs();
    loop {
        wait_for_interrupt();
    }
}
