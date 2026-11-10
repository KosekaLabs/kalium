//! aarch64 implementation of the HAL (QEMU `virt`).

pub mod boot;
pub mod console;
pub mod cpu;
pub mod exceptions;
pub mod exit;
pub mod gic;
pub mod mmu;
mod panic;
#[cfg(feature = "hal-selftest")]
pub mod selftest;
pub mod timer;

/// A minimal IRQ-safe cell for single-core kernel state that is written from
/// both thread context and interrupt handlers.
///
/// Access runs with IRQs masked, which on a single core is enough for mutual
/// exclusion. When SMP arrives this must become a real spinlock.
pub(crate) struct IrqSafe<T> {
    inner: core::cell::UnsafeCell<T>,
}

// SAFETY: every access goes through `with`, which masks IRQs for the duration,
// and M1 runs on a single core.
unsafe impl<T> Sync for IrqSafe<T> {}

impl<T> IrqSafe<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self { inner: core::cell::UnsafeCell::new(value) }
    }

    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let state = cpu::save_and_disable_irqs();
        // SAFETY: IRQs are masked and there is one core, so no other access
        // to the cell can be live while `f` runs.
        let r = f(unsafe { &mut *self.inner.get() });
        cpu::restore_irqs(state);
        r
    }
}
