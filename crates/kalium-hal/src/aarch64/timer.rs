//! ARM generic timer: the EL1 physical timer (`CNTP_*_EL0`) and counter
//! (`CNTPCT_EL0`), interrupt PPI 30 on QEMU `virt`.
//!
//! The timer is one-shot: [`set_timer`] arms a deadline, the interrupt masks
//! the timer again and calls the handler registered with
//! [`set_tick_handler`]. A periodic tick is obtained by re-arming from the
//! handler.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::cpu::{read_sysreg, write_sysreg};
use super::{cpu, gic};

/// Interrupt ID of the EL1 physical timer (PPI 14).
pub const TIMER_IRQ: u32 = 30;

const NS_PER_S: u128 = 1_000_000_000;

// CNTP_CTL_EL0 bits.
const CTL_ENABLE: u64 = 1 << 0;
const CTL_IMASK: u64 = 1 << 1;
const CTL_ISTATUS: u64 = 1 << 2;

/// The registered tick handler, stored as a `usize` (0 = none).
static TICK_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Counter frequency, cached at init.
static FREQ_HZ: AtomicU64 = AtomicU64::new(0);

/// Counter ticks per second (`CNTFRQ_EL0`; 62.5 MHz on QEMU virt).
pub fn frequency_hz() -> u64 {
    let cached = FREQ_HZ.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let f = read_sysreg!("cntfrq_el0") & 0xffff_ffff;
    // A zero frequency means firmware did not program CNTFRQ; refuse to
    // divide by it.
    let f = if f == 0 { 1 } else { f };
    FREQ_HZ.store(f, Ordering::Relaxed);
    f
}

/// Raw counter value.
#[inline]
pub fn counter() -> u64 {
    // The counter read is ordered after preceding instructions by ISB so a
    // timestamp cannot be taken speculatively early.
    cpu::isb();
    read_sysreg!("cntpct_el0")
}

/// Convert counter ticks to nanoseconds.
pub fn ticks_to_ns(ticks: u64) -> u64 {
    (u128::from(ticks) * NS_PER_S / u128::from(frequency_hz())) as u64
}

/// Convert nanoseconds to counter ticks, rounding up so a timer never fires
/// early.
pub fn ns_to_ticks(ns: u64) -> u64 {
    let f = u128::from(frequency_hz());
    ((u128::from(ns) * f).div_ceil(NS_PER_S)) as u64
}

/// Monotonic nanoseconds since the counter started (effectively since boot).
pub fn now_ns() -> u64 {
    ticks_to_ns(counter())
}

/// Initialize the timer: stopped, with its interrupt routed through the GIC.
/// Call after [`gic::init`].
pub fn init() {
    let _ = frequency_hz();
    stop_timer();
    gic::set_irq_handler(TIMER_IRQ, irq_handler);
    gic::enable_irq(TIMER_IRQ);
}

/// Arm a one-shot interrupt `ns_from_now` nanoseconds in the future (at least
/// one counter tick). Replaces any pending deadline.
pub fn set_timer(ns_from_now: u64) {
    let ticks = ns_to_ticks(ns_from_now).max(1);
    let deadline = counter().saturating_add(ticks);
    arm(deadline);
}

/// Arm a one-shot interrupt at absolute time `deadline_ns` (in [`now_ns`]
/// units). A deadline in the past fires immediately.
pub fn set_timer_at(deadline_ns: u64) {
    arm(ns_to_ticks(deadline_ns));
}

fn arm(deadline_ticks: u64) {
    // Mask while reprogramming so a stale compare cannot raise the line.
    write_sysreg!("cntp_ctl_el0", CTL_IMASK);
    write_sysreg!("cntp_cval_el0", deadline_ticks);
    write_sysreg!("cntp_ctl_el0", CTL_ENABLE);
    cpu::isb();
}

/// Disable the timer and mask its interrupt.
pub fn stop_timer() {
    write_sysreg!("cntp_ctl_el0", CTL_IMASK);
    cpu::isb();
}

/// True while an armed deadline has been reached and the interrupt is
/// pending or being delivered.
pub fn is_pending() -> bool {
    let ctl = read_sysreg!("cntp_ctl_el0");
    ctl & (CTL_ENABLE | CTL_ISTATUS) == (CTL_ENABLE | CTL_ISTATUS)
}

/// Register the function called from interrupt context on every timer
/// interrupt. The timer is already stopped when it runs; re-arm it with
/// [`set_timer`] for a periodic tick.
pub fn set_tick_handler(handler: fn()) {
    TICK_HANDLER.store(handler as usize, Ordering::Release);
}

/// Remove the tick handler.
pub fn clear_tick_handler() {
    TICK_HANDLER.store(0, Ordering::Release);
}

fn tick_handler() -> Option<fn()> {
    let raw = TICK_HANDLER.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY: the only non-zero values ever stored come from
        // `set_tick_handler`, which stores a valid `fn()`.
        Some(unsafe { core::mem::transmute::<usize, fn()>(raw) })
    }
}

fn irq_handler() {
    // Level-triggered: stop the timer first so the line drops before EOI.
    stop_timer();
    if let Some(f) = tick_handler() {
        f();
    }
}
