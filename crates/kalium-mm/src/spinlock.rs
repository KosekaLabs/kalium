//! A minimal spinlock.
//!
//! The kernel runs on a single core in M1 and never allocates from interrupt
//! context, so a plain test-and-set lock is enough. On aarch64 the atomics
//! require the MMU to be enabled (normal memory), which the HAL guarantees
//! before the allocator is initialised.
//!
//! This is the only `unsafe` outside [`crate::heap`]: the usual
//! `UnsafeCell` + `Sync` pattern of a mutex.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A spinlock protecting a `T`.
pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: the lock serialises all access to `data`, so sharing the lock
// between threads is sound whenever `T` could be sent to the thread that
// ends up holding it.
unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Creates an unlocked lock around `value`. `const` so it can live in a
    /// `static`.
    pub const fn new(value: T) -> Self {
        SpinLock { locked: AtomicBool::new(false), data: UnsafeCell::new(value) }
    }

    /// Acquires the lock, spinning until it is free.
    ///
    /// Not reentrant: locking twice on the same thread deadlocks. Do not call
    /// from interrupt context while the same lock may be held by the
    /// interrupted code.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            while self.locked.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
    }

    /// Acquires the lock if it is free, without spinning.
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            Some(SpinLockGuard { lock: self })
        } else {
            None
        }
    }

    /// Whether the lock is currently held (racy; for diagnostics only).
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    /// Returns a mutable reference to the data without locking, which is
    /// safe because the borrow proves exclusive access.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    /// Consumes the lock and returns the protected value.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

/// RAII guard: the lock is released when the guard is dropped.
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard exists only while the lock is held, which gives
        // exclusive access to `data` for the guard's lifetime.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, plus `&mut self` makes this the only
        // reference derived from the guard.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::vec::Vec;

    #[test]
    fn lock_unlock_roundtrip() {
        let lock = SpinLock::new(5u32);
        assert!(!lock.is_locked());
        {
            let mut g = lock.lock();
            *g += 1;
            assert!(lock.is_locked());
            assert!(lock.try_lock().is_none());
        }
        assert!(!lock.is_locked());
        assert_eq!(*lock.lock(), 6);
        assert_eq!(lock.into_inner(), 6);
    }

    #[test]
    fn get_mut_bypasses_lock() {
        let mut lock = SpinLock::new(1u8);
        *lock.get_mut() = 9;
        assert_eq!(*lock.lock(), 9);
    }

    #[test]
    fn counts_under_contention() {
        let lock = Arc::new(SpinLock::new(0u64));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let lock = Arc::clone(&lock);
                thread::spawn(move || {
                    for _ in 0..10_000 {
                        *lock.lock() += 1;
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*lock.lock(), 80_000);
    }
}
