//! Kalium memory management: physical frame allocator, kernel heap and
//! memory objects.
//!
//! This crate is one of the three places in the kernel allowed to contain
//! `unsafe` (see `docs/prototype-plan.md`). It is kept to the heap module and
//! the spinlock; the frame allocator and memory objects are `forbid(unsafe_code)`.
//!
//! * [`FrameAllocator`]: a bitmap allocator over 4 KiB physical frames in a
//!   given range, with contiguous first-fit allocation.
//! * [`KernelHeap`]: a first-fit free-list allocator with coalescing that
//!   implements [`core::alloc::GlobalAlloc`] behind a spinlock, intended to be
//!   the kernel's `#[global_allocator]`.
//! * [`MemObject`]: a bounds-checked, zero-initialised byte object backed by a
//!   `Vec<u8>` (the M1 implementation of `kalium:memory` objects).
//!
//! Everything here builds and is tested on the host; nothing depends on the
//! target architecture beyond the pointer width.

#![no_std]
#![deny(missing_docs)]
#![warn(clippy::all)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod frame;
pub mod heap;
pub mod memobj;
pub mod spinlock;

pub use frame::{FrameAllocator, FrameError, FrameStats, PhysAddr, FRAME_SIZE};
pub use heap::{Heap, HeapError, HeapStats, KernelHeap};
pub use memobj::{MemError, MemObject, MAX_MEMOBJ_BYTES};
pub use spinlock::{SpinLock, SpinLockGuard};

#[cfg(test)]
pub(crate) mod test_util {
    //! Shared helpers for the host tests.

    /// A tiny deterministic PRNG (xorshift64*) so the randomized tests are
    /// reproducible without pulling in a dependency.
    pub struct Rng(u64);

    impl Rng {
        pub fn new(seed: u64) -> Self {
            // The state must never be zero.
            Rng(seed | 1)
        }

        pub fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// Uniform-ish value in `0..n` (`n > 0`).
        pub fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }

        /// Uniform-ish value in `lo..=hi`.
        pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + self.below(hi - lo + 1)
        }

        pub fn chance(&mut self, num: u64, den: u64) -> bool {
            self.below(den) < num
        }
    }
}
