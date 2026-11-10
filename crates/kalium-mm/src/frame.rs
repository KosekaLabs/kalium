//! Physical frame allocator: a bitmap over 4 KiB frames.
//!
//! One bit per frame, set when the frame is in use. `alloc_frames(n)` finds
//! the first run of `n` free frames (first fit) and marks it; `free_frames`
//! clears it. The bitmap lives in a `Vec<u64>` on the kernel heap, so the
//! kernel initialises [`crate::KernelHeap`] first and then builds the frame
//! allocator over the rest of RAM.
//!
//! The allocator never touches the frames themselves; it only tracks their
//! state. That is what makes it host-testable: a test can hand it a range of
//! offsets into a `Vec<u8>` arena and use the returned addresses as indices.
//!
//! No `unsafe` here.

#![forbid(unsafe_code)]

use alloc::vec::Vec;
use core::fmt;

/// Size of a physical frame in bytes.
pub const FRAME_SIZE: usize = 4096;

const BITS: usize = u64::BITS as usize;

/// A physical address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PhysAddr(pub usize);

impl PhysAddr {
    /// Wraps a raw address.
    pub const fn new(addr: usize) -> Self {
        PhysAddr(addr)
    }

    /// The raw address.
    pub const fn as_usize(self) -> usize {
        self.0
    }

    /// Whether the address is on a frame boundary.
    pub const fn is_frame_aligned(self) -> bool {
        self.0.is_multiple_of(FRAME_SIZE)
    }

    /// Rounds down to a frame boundary.
    pub const fn frame_floor(self) -> Self {
        PhysAddr(self.0 & !(FRAME_SIZE - 1))
    }

    /// Rounds up to a frame boundary; `None` on overflow.
    pub const fn frame_ceil(self) -> Option<Self> {
        match self.0.checked_add(FRAME_SIZE - 1) {
            Some(v) => Some(PhysAddr(v & !(FRAME_SIZE - 1))),
            None => None,
        }
    }

    /// `self + bytes`, or `None` on overflow.
    pub const fn checked_add(self, bytes: usize) -> Option<Self> {
        match self.0.checked_add(bytes) {
            Some(v) => Some(PhysAddr(v)),
            None => None,
        }
    }
}

impl fmt::Debug for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysAddr({:#x})", self.0)
    }
}

impl fmt::Display for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

/// Errors from [`FrameAllocator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The address is not on a frame boundary.
    Misaligned,
    /// The frame range is not inside the allocator's range (or overflows).
    OutOfRange,
    /// `free_frames` on a frame that is not allocated (double free).
    NotAllocated,
    /// `reserve` on a frame that is already allocated.
    AlreadyAllocated,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FrameError::Misaligned => "address is not frame aligned",
            FrameError::OutOfRange => "frame range outside the allocator",
            FrameError::NotAllocated => "frame is not allocated",
            FrameError::AlreadyAllocated => "frame is already allocated",
        };
        f.write_str(s)
    }
}

/// Counters describing the allocator's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameStats {
    /// Frames in the managed range.
    pub total_frames: usize,
    /// Frames currently free.
    pub free_frames: usize,
    /// Frames currently allocated or reserved.
    pub used_frames: usize,
    /// Successful `alloc_frames` calls.
    pub allocations: u64,
    /// Successful `free_frames` calls.
    pub frees: u64,
    /// `alloc_frames` calls that found no fitting run.
    pub failures: u64,
}

/// Bitmap allocator over the frames in `[start, end)`.
pub struct FrameAllocator {
    base: PhysAddr,
    frames: usize,
    /// Bit `i` set means frame `i` is in use. Bits past `frames` are set so
    /// the word scan never has to special-case the tail.
    bitmap: Vec<u64>,
    free: usize,
    /// Lowest frame index that might be free; a hint to skip the dense
    /// prefix of a mostly full bitmap.
    hint: usize,
    allocations: u64,
    frees: u64,
    failures: u64,
}

impl FrameAllocator {
    /// Manages the frames fully inside `[start, end)`: `start` is rounded up
    /// and `end` rounded down to frame boundaries. An empty or inverted
    /// range gives an allocator with no frames.
    pub fn new(start: PhysAddr, end: PhysAddr) -> Self {
        let base = start.frame_ceil().unwrap_or(PhysAddr(usize::MAX));
        let end = end.frame_floor();
        let frames = if end.0 > base.0 { (end.0 - base.0) / FRAME_SIZE } else { 0 };
        let words = frames.div_ceil(BITS);
        let mut bitmap = alloc::vec![0u64; words];
        // Mark the padding bits of the last word as used.
        let tail_bits = frames % BITS;
        if tail_bits != 0
            && let Some(last) = bitmap.last_mut()
        {
            *last = !0u64 << tail_bits;
        }
        FrameAllocator { base, frames, bitmap, free: frames, hint: 0, allocations: 0, frees: 0, failures: 0 }
    }

    /// First managed frame.
    pub fn base(&self) -> PhysAddr {
        self.base
    }

    /// One past the last managed frame.
    pub fn end(&self) -> PhysAddr {
        PhysAddr(self.base.0 + self.frames * FRAME_SIZE)
    }

    /// Number of frames managed.
    pub fn total_frames(&self) -> usize {
        self.frames
    }

    /// Number of frames currently free.
    pub fn free_frames_count(&self) -> usize {
        self.free
    }

    /// Number of frames currently in use.
    pub fn used_frames(&self) -> usize {
        self.frames - self.free
    }

    /// All counters.
    pub fn stats(&self) -> FrameStats {
        FrameStats {
            total_frames: self.frames,
            free_frames: self.free,
            used_frames: self.frames - self.free,
            allocations: self.allocations,
            frees: self.frees,
            failures: self.failures,
        }
    }

    #[inline]
    fn is_set(&self, idx: usize) -> bool {
        self.bitmap[idx / BITS] & (1u64 << (idx % BITS)) != 0
    }

    #[inline]
    fn set(&mut self, idx: usize) {
        self.bitmap[idx / BITS] |= 1u64 << (idx % BITS);
    }

    #[inline]
    fn clear(&mut self, idx: usize) {
        self.bitmap[idx / BITS] &= !(1u64 << (idx % BITS));
    }

    /// Converts a frame-aligned address range to frame indices, checking
    /// that it lies inside the managed range.
    fn range_to_indices(&self, addr: PhysAddr, n: usize) -> Result<(usize, usize), FrameError> {
        if !addr.is_frame_aligned() {
            return Err(FrameError::Misaligned);
        }
        if addr.0 < self.base.0 {
            return Err(FrameError::OutOfRange);
        }
        let first = (addr.0 - self.base.0) / FRAME_SIZE;
        let last = first.checked_add(n).ok_or(FrameError::OutOfRange)?;
        if last > self.frames {
            return Err(FrameError::OutOfRange);
        }
        Ok((first, last))
    }

    /// Whether the frame at `addr` is allocated. `None` if `addr` is
    /// misaligned or outside the range.
    pub fn is_allocated(&self, addr: PhysAddr) -> Option<bool> {
        let (first, _) = self.range_to_indices(addr, 1).ok()?;
        Some(self.is_set(first))
    }

    /// Finds the first run of `n` free frames starting at or after `from`.
    fn find_run(&self, from: usize, n: usize) -> Option<usize> {
        let mut idx = from;
        while idx < self.frames {
            // Skip whole words that are completely full.
            let word = self.bitmap[idx / BITS];
            if word == !0u64 {
                idx = (idx / BITS + 1) * BITS;
                continue;
            }
            if self.is_set(idx) {
                idx += 1;
                continue;
            }
            // `idx` is free; extend the run.
            let start = idx;
            let needed_end = start.checked_add(n)?;
            if needed_end > self.frames {
                return None;
            }
            let mut run_end = start + 1;
            while run_end < needed_end && !self.is_set(run_end) {
                run_end += 1;
            }
            if run_end == needed_end {
                return Some(start);
            }
            // The run was broken by `run_end`; resume after it.
            idx = run_end + 1;
        }
        None
    }

    /// Allocates `n` contiguous frames, first fit. Returns the address of the
    /// first one, or `None` if no run of `n` free frames exists (or `n == 0`).
    pub fn alloc_frames(&mut self, n: usize) -> Option<PhysAddr> {
        if n == 0 || n > self.free {
            self.failures += 1;
            return None;
        }
        match self.find_run(self.hint, n) {
            Some(start) => {
                for i in start..start + n {
                    self.set(i);
                }
                self.free -= n;
                self.allocations += 1;
                if start == self.hint {
                    self.hint = start + n;
                }
                Some(PhysAddr(self.base.0 + start * FRAME_SIZE))
            }
            None => {
                self.failures += 1;
                None
            }
        }
    }

    /// Allocates one frame.
    pub fn alloc_frame(&mut self) -> Option<PhysAddr> {
        self.alloc_frames(1)
    }

    /// Frees the `n` frames starting at `addr`. Every frame must be
    /// allocated; otherwise nothing is changed and an error is returned.
    /// `n == 0` is a no-op.
    pub fn free_frames(&mut self, addr: PhysAddr, n: usize) -> Result<(), FrameError> {
        let (first, last) = self.range_to_indices(addr, n)?;
        if (first..last).any(|i| !self.is_set(i)) {
            return Err(FrameError::NotAllocated);
        }
        for i in first..last {
            self.clear(i);
        }
        self.free += n;
        if n > 0 {
            self.frees += 1;
            self.hint = self.hint.min(first);
        }
        Ok(())
    }

    /// Frees one frame.
    pub fn free_frame(&mut self, addr: PhysAddr) -> Result<(), FrameError> {
        self.free_frames(addr, 1)
    }

    /// Marks the `n` frames starting at `addr` as in use without allocating
    /// them, for memory the kernel already occupies (image, stacks, boot
    /// data). Every frame must be free; otherwise nothing is changed.
    pub fn reserve(&mut self, addr: PhysAddr, n: usize) -> Result<(), FrameError> {
        let (first, last) = self.range_to_indices(addr, n)?;
        if (first..last).any(|i| self.is_set(i)) {
            return Err(FrameError::AlreadyAllocated);
        }
        for i in first..last {
            self.set(i);
        }
        self.free -= n;
        Ok(())
    }

    /// Reserves every frame that overlaps the byte range `[start, end)`,
    /// which need not be frame aligned, clipped to the managed range. Frames
    /// already in use are left as they are. Returns the number of frames
    /// newly reserved.
    pub fn reserve_bytes(&mut self, start: PhysAddr, end: PhysAddr) -> usize {
        let lo = start.frame_floor().0.max(self.base.0);
        let hi = end.frame_ceil().map(|a| a.0).unwrap_or(usize::MAX).min(self.end().0);
        if hi <= lo {
            return 0;
        }
        let first = (lo - self.base.0) / FRAME_SIZE;
        let last = (hi - self.base.0) / FRAME_SIZE;
        let mut newly = 0;
        for i in first..last {
            if !self.is_set(i) {
                self.set(i);
                newly += 1;
            }
        }
        self.free -= newly;
        newly
    }

    /// Recounts the bitmap and compares against the counters; for tests.
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        let used = (0..self.frames).filter(|&i| self.is_set(i)).count();
        if used + self.free != self.frames {
            return Err("free counter does not match bitmap");
        }
        let tail_bits = self.frames % BITS;
        if tail_bits != 0 {
            let last = self.bitmap[self.bitmap.len() - 1];
            if last >> tail_bits != !0u64 >> tail_bits {
                return Err("padding bits not all set");
            }
        }
        if self.hint > self.frames {
            return Err("hint past the end");
        }
        if (0..self.hint).any(|i| !self.is_set(i)) {
            return Err("free frame below the hint");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::Rng;

    const F: usize = FRAME_SIZE;

    fn pa(frame: usize) -> PhysAddr {
        PhysAddr(frame * F)
    }

    #[test]
    fn phys_addr_helpers() {
        assert!(pa(3).is_frame_aligned());
        assert!(!PhysAddr(F + 1).is_frame_aligned());
        assert_eq!(PhysAddr(F + 1).frame_floor(), pa(1));
        assert_eq!(PhysAddr(F + 1).frame_ceil(), Some(pa(2)));
        assert_eq!(PhysAddr(F).frame_ceil(), Some(pa(1)));
        assert_eq!(PhysAddr(usize::MAX).frame_ceil(), None);
        assert_eq!(PhysAddr(usize::MAX).checked_add(1), None);
        assert_eq!(std::format!("{:?}", pa(1)), "PhysAddr(0x1000)");
        assert_eq!(std::format!("{}", pa(1)), "0x1000");
    }

    #[test]
    fn rounds_range_inwards() {
        let a = FrameAllocator::new(PhysAddr(F + 1), PhysAddr(10 * F - 1));
        assert_eq!(a.base(), pa(2));
        assert_eq!(a.end(), pa(9));
        assert_eq!(a.total_frames(), 7);
        assert_eq!(a.free_frames_count(), 7);
        a.check_invariants().unwrap();

        let mut empty = FrameAllocator::new(pa(5), pa(5));
        assert_eq!(empty.total_frames(), 0);
        assert_eq!(empty.alloc_frame(), None);
        let inverted = FrameAllocator::new(pa(9), pa(2));
        assert_eq!(inverted.total_frames(), 0);
        inverted.check_invariants().unwrap();
    }

    #[test]
    fn sequential_allocation_and_free() {
        let mut a = FrameAllocator::new(pa(0), pa(8));
        assert_eq!(a.alloc_frame(), Some(pa(0)));
        assert_eq!(a.alloc_frames(3), Some(pa(1)));
        assert_eq!(a.alloc_frames(4), Some(pa(4)));
        assert_eq!(a.alloc_frame(), None);
        assert_eq!(a.free_frames_count(), 0);
        assert_eq!(a.used_frames(), 8);
        let s = a.stats();
        assert_eq!(s.allocations, 3);
        assert_eq!(s.failures, 1);
        a.check_invariants().unwrap();

        a.free_frames(pa(1), 3).unwrap();
        assert_eq!(a.free_frames_count(), 3);
        assert_eq!(a.is_allocated(pa(1)), Some(false));
        assert_eq!(a.is_allocated(pa(0)), Some(true));
        assert_eq!(a.is_allocated(pa(8)), None);
        assert_eq!(a.is_allocated(PhysAddr(1)), None);
        // First fit: the hole at 1..4 is reused before anything else.
        assert_eq!(a.alloc_frames(2), Some(pa(1)));
        assert_eq!(a.alloc_frames(2), None);
        assert_eq!(a.alloc_frames(1), Some(pa(3)));
        a.check_invariants().unwrap();
    }

    #[test]
    fn zero_and_oversize_requests() {
        let mut a = FrameAllocator::new(pa(0), pa(4));
        assert_eq!(a.alloc_frames(0), None);
        assert_eq!(a.alloc_frames(5), None);
        assert_eq!(a.alloc_frames(usize::MAX), None);
        assert_eq!(a.stats().failures, 3);
        assert_eq!(a.free_frames(pa(0), 0), Ok(()));
        assert_eq!(a.alloc_frames(4), Some(pa(0)));
    }

    #[test]
    fn free_errors_leave_state_unchanged() {
        let mut a = FrameAllocator::new(pa(2), pa(10));
        let p = a.alloc_frames(2).unwrap();
        let before = a.stats();
        assert_eq!(a.free_frames(PhysAddr(p.0 + 1), 1), Err(FrameError::Misaligned));
        assert_eq!(a.free_frames(pa(1), 1), Err(FrameError::OutOfRange));
        assert_eq!(a.free_frames(pa(10), 1), Err(FrameError::OutOfRange));
        assert_eq!(a.free_frames(pa(9), 2), Err(FrameError::OutOfRange));
        assert_eq!(a.free_frames(pa(2), usize::MAX), Err(FrameError::OutOfRange));
        // Partially free range: the second frame is already free.
        assert_eq!(a.free_frames(pa(3), 2), Err(FrameError::NotAllocated));
        assert_eq!(a.free_frames(pa(5), 1), Err(FrameError::NotAllocated));
        assert_eq!(a.stats(), before);
        a.check_invariants().unwrap();
        a.free_frames(p, 2).unwrap();
        assert_eq!(a.free_frames(p, 2), Err(FrameError::NotAllocated));
        assert_eq!(a.free_frames_count(), 8);
    }

    #[test]
    fn reserve_marks_frames_used() {
        let mut a = FrameAllocator::new(pa(0), pa(16));
        a.reserve(pa(0), 4).unwrap();
        assert_eq!(a.free_frames_count(), 12);
        assert_eq!(a.reserve(pa(3), 2), Err(FrameError::AlreadyAllocated));
        assert_eq!(a.reserve(pa(15), 2), Err(FrameError::OutOfRange));
        assert_eq!(a.free_frames_count(), 12);
        assert_eq!(a.alloc_frame(), Some(pa(4)));
        // Reserved frames can be released like allocated ones.
        a.free_frames(pa(0), 4).unwrap();
        assert_eq!(a.alloc_frames(4), Some(pa(0)));
        a.check_invariants().unwrap();
    }

    #[test]
    fn reserve_bytes_clips_and_rounds_outwards() {
        let mut a = FrameAllocator::new(pa(4), pa(12));
        // Covers frames 5 and 6 (partial on both ends).
        assert_eq!(a.reserve_bytes(PhysAddr(5 * F + 10), PhysAddr(6 * F + 1)), 2);
        assert_eq!(a.is_allocated(pa(5)), Some(true));
        assert_eq!(a.is_allocated(pa(6)), Some(true));
        assert_eq!(a.is_allocated(pa(7)), Some(false));
        // Overlapping an existing reservation and both range ends.
        assert_eq!(a.reserve_bytes(pa(0), pa(100)), 6);
        assert_eq!(a.free_frames_count(), 0);
        assert_eq!(a.reserve_bytes(pa(0), pa(100)), 0);
        assert_eq!(a.reserve_bytes(pa(12), pa(20)), 0);
        assert_eq!(a.reserve_bytes(pa(20), pa(1)), 0);
        a.check_invariants().unwrap();
        a.free_frames(pa(4), 8).unwrap();
        assert_eq!(a.free_frames_count(), 8);
    }

    #[test]
    fn word_boundaries_and_padding_bits() {
        // 130 frames: two full words plus two bits in a third.
        let mut a = FrameAllocator::new(pa(0), pa(130));
        a.check_invariants().unwrap();
        assert_eq!(a.alloc_frames(130), Some(pa(0)));
        assert_eq!(a.alloc_frame(), None);
        a.free_frames(pa(63), 2).unwrap(); // straddles words 0 and 1
        assert_eq!(a.alloc_frames(2), Some(pa(63)));
        a.free_frames(pa(127), 3).unwrap(); // straddles into the tail word
        assert_eq!(a.alloc_frames(4), None); // padding is not free space
        assert_eq!(a.alloc_frames(3), Some(pa(127)));
        a.check_invariants().unwrap();
        a.free_frames(pa(0), 130).unwrap();
        assert_eq!(a.free_frames_count(), 130);
        a.check_invariants().unwrap();
    }

    #[test]
    fn large_range_scans_quickly_past_full_words() {
        let mut a = FrameAllocator::new(pa(0), pa(1 << 16));
        for _ in 0..(1 << 16) - 1 {
            assert!(a.alloc_frame().is_some());
        }
        assert_eq!(a.alloc_frame(), Some(pa((1 << 16) - 1)));
        assert_eq!(a.alloc_frame(), None);
        a.free_frames(pa(40_000), 10).unwrap();
        assert_eq!(a.alloc_frames(10), Some(pa(40_000)));
        a.check_invariants().unwrap();
    }

    /// Randomized test over a `Vec<u8>` arena: offsets are addresses. Each
    /// allocation fills its frames with a tag; before freeing we check the
    /// tag survived, which fails if any two live allocations overlapped.
    fn randomized(seed: u64, frames: usize, ops: usize, max_run: u64) {
        let mut arena = std::vec![0u8; frames * F];
        let mut a = FrameAllocator::new(pa(0), pa(frames));
        let mut rng = Rng::new(seed);
        let mut live: std::vec::Vec<(PhysAddr, usize, u8)> = std::vec::Vec::new();
        let mut tag = 0u8;
        let mut successes = 0;

        for step in 0..ops {
            let do_alloc = live.is_empty() || rng.chance(if a.free_frames_count() > frames / 2 { 3 } else { 1 }, 4);
            if do_alloc {
                let n = rng.range(1, max_run) as usize;
                if let Some(p) = a.alloc_frames(n) {
                    assert!(p.is_frame_aligned());
                    assert!(p.0 + n * F <= arena.len());
                    tag = tag.wrapping_add(1);
                    // Every frame must be unused (all-zero: we re-zero on free).
                    assert!(arena[p.0..p.0 + n * F].iter().all(|&b| b == 0), "overlap at {p}");
                    arena[p.0..p.0 + n * F].fill(tag);
                    live.push((p, n, tag));
                    successes += 1;
                } else {
                    // Failure must mean no run of n free frames exists.
                    assert!(a.free_frames_count() < n || !has_run(&a, n), "spurious failure for {n}");
                }
            } else {
                let i = rng.below(live.len() as u64) as usize;
                let (p, n, t) = live.swap_remove(i);
                assert!(arena[p.0..p.0 + n * F].iter().all(|&b| b == t), "corrupted at {p}");
                arena[p.0..p.0 + n * F].fill(0);
                a.free_frames(p, n).unwrap();
            }
            if step % 101 == 0 {
                a.check_invariants().unwrap();
                assert_eq!(a.used_frames(), live.iter().map(|&(_, n, _)| n).sum::<usize>());
            }
        }
        assert!(successes > ops / 4);
        for (p, n, t) in live.drain(..) {
            assert!(arena[p.0..p.0 + n * F].iter().all(|&b| b == t));
            a.free_frames(p, n).unwrap();
        }
        assert_eq!(a.free_frames_count(), frames);
        assert_eq!(a.used_frames(), 0);
        a.check_invariants().unwrap();
        // Everything free again: the whole range is one run.
        assert_eq!(a.alloc_frames(frames), Some(pa(0)));
        let s = a.stats();
        assert_eq!(s.allocations, s.frees + 1);
    }

    /// Brute-force: is there a run of `n` free frames anywhere?
    fn has_run(a: &FrameAllocator, n: usize) -> bool {
        let mut run = 0;
        for i in 0..a.total_frames() {
            if a.is_allocated(pa(i)) == Some(false) {
                run += 1;
                if run >= n {
                    return true;
                }
            } else {
                run = 0;
            }
        }
        false
    }

    #[test]
    fn randomized_single_frames() {
        randomized(1, 1024, 20_000, 1);
    }

    #[test]
    fn randomized_mixed_runs() {
        randomized(2, 2048, 20_000, 16);
    }

    #[test]
    fn randomized_large_runs_small_arena() {
        randomized(3, 300, 10_000, 64);
    }

    #[test]
    fn randomized_several_seeds() {
        for seed in 20..26 {
            randomized(seed, 512, 4_000, 8);
        }
    }
}
