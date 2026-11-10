//! The kernel heap: a first-fit free-list allocator with coalescing.
//!
//! # Design
//!
//! [`Heap`] manages one contiguous region. Free space is kept as an
//! address-ordered singly linked list whose nodes (`FreeBlock`) live inside
//! the free memory itself, so the allocator needs no metadata storage of its
//! own. Allocated blocks carry **no header**: every block boundary is a
//! multiple of [`MIN_BLOCK`] bytes and every request is rounded up to it, so
//! the `Layout` handed back at deallocation is enough to recover the block.
//! That makes small allocations cheap (a 1-byte allocation costs 16 bytes, not
//! 32) and keeps the invariants simple:
//!
//! 1. Every free block starts at a `MIN_BLOCK`-aligned address and has a size
//!    that is a non-zero multiple of `MIN_BLOCK` (so a node always fits).
//! 2. The free list is sorted by address and no two free blocks are adjacent
//!    (freeing coalesces eagerly with both neighbours).
//! 3. Every allocated block is also `MIN_BLOCK`-aligned with a size that is a
//!    multiple of `MIN_BLOCK`, which is why the leftover on either side of an
//!    aligned allocation is either zero or a valid free block.
//!
//! Allocation walks the list and takes the first block where the request fits
//! after alignment padding (first fit). The padding, if any, stays in the list
//! as a smaller free block, and the tail remainder, if any, becomes a new one.
//!
//! [`KernelHeap`] wraps a `Heap` in a [`SpinLock`] and implements
//! [`GlobalAlloc`], so the kernel can declare
//! `#[global_allocator] static HEAP: KernelHeap = KernelHeap::new();` and call
//! [`KernelHeap::init`] once the HAL has set up memory.
//!
//! # Unsafe
//!
//! All `unsafe` in this crate's allocator lives in this module, in `Heap`'s
//! raw-pointer list manipulation. The soundness argument is that the heap owns
//! its region exclusively (promised by the caller of `init`), that free-list
//! nodes are only ever written into memory that is free, and that the
//! `GlobalAlloc` contract guarantees `dealloc` is called with a pointer and
//! layout that came from `alloc`. Violations of the latter (double free, wrong
//! layout) are detected where cheap and reported as [`HeapError::InvalidFree`]
//! instead of corrupting the list.

use core::alloc::{GlobalAlloc, Layout};
use core::mem::{align_of, size_of};
use core::ptr::{self, NonNull};

use crate::spinlock::SpinLock;

/// Allocation granule: all block addresses and sizes are multiples of this.
pub const MIN_BLOCK: usize = 16;

const _: () = assert!(size_of::<FreeBlock>() <= MIN_BLOCK);
const _: () = assert!(align_of::<FreeBlock>() <= MIN_BLOCK);
const _: () = assert!(MIN_BLOCK.is_power_of_two());

/// A node of the free list, stored at the start of the free block it
/// describes.
#[repr(C)]
struct FreeBlock {
    /// Size of the block in bytes, including this header.
    size: usize,
    /// The next free block, by increasing address, or null.
    next: *mut FreeBlock,
}

/// Errors reported by the heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeapError {
    /// `init` was called on a heap that already has a region.
    AlreadyInitialized,
    /// The region is too small to hold even one block after alignment.
    RegionTooSmall,
    /// `dealloc` was given a pointer/layout that does not describe a live
    /// allocation of this heap (double free, foreign pointer or wrong layout).
    InvalidFree,
}

/// Counters describing the heap's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeapStats {
    /// Bytes in the managed region (after alignment of its ends).
    pub total_bytes: usize,
    /// Bytes currently in free blocks.
    pub free_bytes: usize,
    /// Bytes currently handed out (rounded to `MIN_BLOCK`).
    pub allocated_bytes: usize,
    /// Number of blocks on the free list.
    pub free_blocks: usize,
    /// Size of the largest free block.
    pub largest_free: usize,
    /// Successful allocations since `init`.
    pub allocations: u64,
    /// Successful deallocations since `init`.
    pub frees: u64,
    /// Allocations that failed for lack of space.
    pub failures: u64,
}

/// The unlocked allocator. Use [`KernelHeap`] unless you provide your own
/// synchronisation.
pub struct Heap {
    head: *mut FreeBlock,
    start: usize,
    end: usize,
    free_bytes: usize,
    free_blocks: usize,
    allocations: u64,
    frees: u64,
    failures: u64,
}

// SAFETY: the raw pointers refer only to memory the heap owns exclusively;
// moving the heap to another thread moves that ownership with it.
unsafe impl Send for Heap {}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn round_up(x: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    x.checked_add(align - 1).map(|v| v & !(align - 1))
}

#[inline]
fn round_down(x: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    x & !(align - 1)
}

impl Heap {
    /// An empty heap with no region. Every allocation fails until
    /// [`Heap::init`] is called.
    pub const fn new() -> Self {
        Heap { head: ptr::null_mut(), start: 0, end: 0, free_bytes: 0, free_blocks: 0, allocations: 0, frees: 0, failures: 0 }
    }

    /// Whether `init` has succeeded.
    pub fn is_initialized(&self) -> bool {
        self.end != 0
    }

    /// Gives the heap the region `[start, start + size)`.
    ///
    /// The ends are rounded inwards to `MIN_BLOCK`; what remains must be at
    /// least `MIN_BLOCK` bytes.
    ///
    /// # Safety
    ///
    /// The region must be valid, writable memory that nothing else uses for
    /// as long as the heap exists, and `start + size` must not overflow.
    pub unsafe fn init(&mut self, start: *mut u8, size: usize) -> Result<(), HeapError> {
        if self.is_initialized() {
            return Err(HeapError::AlreadyInitialized);
        }
        let raw_start = start as usize;
        let raw_end = raw_start.checked_add(size).ok_or(HeapError::RegionTooSmall)?;
        let begin = round_up(raw_start, MIN_BLOCK).ok_or(HeapError::RegionTooSmall)?;
        let end = round_down(raw_end, MIN_BLOCK);
        if end <= begin || end - begin < MIN_BLOCK {
            return Err(HeapError::RegionTooSmall);
        }
        let block = begin as *mut FreeBlock;
        // SAFETY: `begin..end` lies inside the caller-provided region, is
        // `MIN_BLOCK`-aligned and at least `MIN_BLOCK` bytes, so a node fits.
        unsafe {
            block.write(FreeBlock { size: end - begin, next: ptr::null_mut() });
        }
        self.head = block;
        self.start = begin;
        self.end = end;
        self.free_bytes = end - begin;
        self.free_blocks = 1;
        Ok(())
    }

    /// The effective request size for `layout`: at least one granule and a
    /// multiple of `MIN_BLOCK`. `None` on overflow.
    #[inline]
    fn block_size(layout: Layout) -> Option<usize> {
        round_up(layout.size().max(1), MIN_BLOCK)
    }

    /// Allocates memory for `layout`, or `None` if no free block fits.
    pub fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let size = match Self::block_size(layout) {
            Some(s) => s,
            None => {
                self.failures += 1;
                return None;
            }
        };
        let align = layout.align().max(MIN_BLOCK);

        // `link` is the pointer slot that currently refers to `cur`: either
        // `self.head` or the `next` field of the previous block. Keeping the
        // slot rather than the previous block lets us unlink without a
        // special case for the head.
        let mut link: *mut *mut FreeBlock = &mut self.head;
        let mut free_blocks_delta: isize = 0;
        let result = loop {
            // SAFETY: `link` points either at `self.head` or at the `next`
            // field of a live free block; both are valid for reads.
            let cur = unsafe { *link };
            if cur.is_null() {
                break None;
            }
            let block_start = cur as usize;
            // SAFETY: `cur` is a live free block, so its header is readable.
            let (block_size, next) = unsafe { ((*cur).size, (*cur).next) };
            let block_end = block_start + block_size;

            let fits = round_up(block_start, align)
                .and_then(|aligned| aligned.checked_add(size).map(|end| (aligned, end)))
                .filter(|&(_, alloc_end)| alloc_end <= block_end);
            let (aligned, alloc_end) = match fits {
                Some(f) => f,
                None => {
                    // SAFETY: `cur` is live; taking the address of its `next`
                    // field is fine.
                    link = unsafe { ptr::addr_of_mut!((*cur).next) };
                    continue;
                }
            };

            // Padding in front and remainder behind are multiples of
            // MIN_BLOCK because every operand is, so each is either zero or
            // big enough to be a free block of its own.
            let pad = aligned - block_start;
            let tail = block_end - alloc_end;
            debug_assert!(pad == 0 || pad >= MIN_BLOCK);
            debug_assert!(tail == 0 || tail >= MIN_BLOCK);

            // SAFETY: all writes go to headers of free blocks: `cur`'s header
            // (which stays in free memory when `pad > 0`, since the
            // allocation starts at `aligned >= block_start + MIN_BLOCK`) and
            // the first `MIN_BLOCK` bytes of the tail remainder, which is free.
            // The slot `link` is valid as described above.
            unsafe {
                let mut slot = link;
                if pad > 0 {
                    (*cur).size = pad;
                    slot = ptr::addr_of_mut!((*cur).next);
                } else {
                    free_blocks_delta -= 1;
                }
                if tail > 0 {
                    let t = alloc_end as *mut FreeBlock;
                    t.write(FreeBlock { size: tail, next });
                    *slot = t;
                    free_blocks_delta += 1;
                } else {
                    *slot = next;
                }
            }
            break NonNull::new(aligned as *mut u8).map(|p| (p, size));
        };

        match result {
            Some((p, size)) => {
                self.free_bytes -= size;
                self.free_blocks = (self.free_blocks as isize + free_blocks_delta) as usize;
                self.allocations += 1;
                Some(p)
            }
            None => {
                self.failures += 1;
                None
            }
        }
    }

    /// Returns the block at `ptr` (allocated with `layout`) to the heap,
    /// coalescing it with free neighbours.
    ///
    /// Returns [`HeapError::InvalidFree`] and leaves the heap untouched when
    /// the block is outside the region, misaligned, or overlaps a free block
    /// (a double free or wrong layout). This is a defence for kernel bugs,
    /// not a guarantee: a wrong layout whose rounded size matches is not
    /// detectable.
    ///
    /// # Safety
    ///
    /// `ptr` must have been returned by `alloc` on this heap with a layout
    /// that rounds to the same block size, and must not be used afterwards.
    pub unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) -> Result<(), HeapError> {
        let size = Self::block_size(layout).ok_or(HeapError::InvalidFree)?;
        let addr = ptr as usize;
        let end = addr.checked_add(size).ok_or(HeapError::InvalidFree)?;
        if !addr.is_multiple_of(MIN_BLOCK) || addr < self.start || end > self.end {
            return Err(HeapError::InvalidFree);
        }

        // Find the insertion point: `link` is the slot that will refer to the
        // freed block, `prev` the free block before it (or null).
        let mut link: *mut *mut FreeBlock = &mut self.head;
        let mut prev: *mut FreeBlock = ptr::null_mut();
        // SAFETY: as in `alloc`, `link` always points at a valid slot and the
        // blocks it reaches are live free blocks.
        let next = unsafe {
            loop {
                let cur = *link;
                if cur.is_null() || cur as usize >= addr {
                    break cur;
                }
                prev = cur;
                link = ptr::addr_of_mut!((*cur).next);
            }
        };

        // SAFETY: `prev` and `next` are live free blocks or null.
        let (prev_end, prev_touches) = unsafe {
            if prev.is_null() {
                (0, false)
            } else {
                let e = prev as usize + (*prev).size;
                (e, e == addr)
            }
        };
        if !prev.is_null() && prev_end > addr {
            return Err(HeapError::InvalidFree);
        }
        let next_touches = !next.is_null() && next as usize == end;
        if !next.is_null() && (next as usize) < end {
            return Err(HeapError::InvalidFree);
        }

        // SAFETY: the block `[addr, end)` is a live allocation being returned,
        // so writing a header into it is fine; `prev`/`next` headers are in
        // free memory.
        unsafe {
            let mut merged_size = size;
            let mut merged_next = next;
            if next_touches {
                merged_size += (*next).size;
                merged_next = (*next).next;
                self.free_blocks -= 1;
            }
            if prev_touches {
                (*prev).size += merged_size;
                (*prev).next = merged_next;
            } else {
                let b = addr as *mut FreeBlock;
                b.write(FreeBlock { size: merged_size, next: merged_next });
                *link = b;
                self.free_blocks += 1;
            }
        }
        self.free_bytes += size;
        self.frees += 1;
        Ok(())
    }

    /// Current counters.
    pub fn stats(&self) -> HeapStats {
        let mut largest = 0;
        let mut cur = self.head;
        while !cur.is_null() {
            // SAFETY: walking the live free list.
            let (size, next) = unsafe { ((*cur).size, (*cur).next) };
            largest = largest.max(size);
            cur = next;
        }
        HeapStats {
            total_bytes: self.end - self.start,
            free_bytes: self.free_bytes,
            allocated_bytes: (self.end - self.start) - self.free_bytes,
            free_blocks: self.free_blocks,
            largest_free: largest,
            allocations: self.allocations,
            frees: self.frees,
            failures: self.failures,
        }
    }

    /// Checks the free-list invariants (sorted, inside the region, aligned,
    /// coalesced, counters consistent). Returns a description of the first
    /// violation. Meant for tests and debug builds; walks the whole list.
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        let mut cur = self.head;
        let mut last_end = 0usize;
        let mut count = 0usize;
        let mut bytes = 0usize;
        while !cur.is_null() {
            let addr = cur as usize;
            if !addr.is_multiple_of(MIN_BLOCK) {
                return Err("free block misaligned");
            }
            if addr < self.start {
                return Err("free block before region");
            }
            // SAFETY: walking the live free list.
            let (size, next) = unsafe { ((*cur).size, (*cur).next) };
            if size == 0 || size % MIN_BLOCK != 0 {
                return Err("free block size not a multiple of MIN_BLOCK");
            }
            let end = match addr.checked_add(size) {
                Some(e) => e,
                None => return Err("free block size overflows"),
            };
            if end > self.end {
                return Err("free block past region");
            }
            if count > 0 && addr <= last_end {
                if addr == last_end {
                    return Err("adjacent free blocks not coalesced");
                }
                return Err("free list not sorted or overlapping");
            }
            last_end = end;
            count += 1;
            bytes += size;
            cur = next;
        }
        if count != self.free_blocks {
            return Err("free block counter mismatch");
        }
        if bytes != self.free_bytes {
            return Err("free byte counter mismatch");
        }
        Ok(())
    }
}

/// The kernel's global allocator: a [`Heap`] behind a [`SpinLock`].
///
/// ```ignore
/// #[global_allocator]
/// static HEAP: KernelHeap = KernelHeap::new();
/// // in kmain, after the MMU is up:
/// unsafe { HEAP.init(heap_start, heap_size) }.expect("heap");
/// ```
pub struct KernelHeap {
    inner: SpinLock<Heap>,
}

impl Default for KernelHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl KernelHeap {
    /// An uninitialised heap; usable in a `static`.
    pub const fn new() -> Self {
        KernelHeap { inner: SpinLock::new(Heap::new()) }
    }

    /// Hands the region `[start, start + size)` to the heap. Call exactly
    /// once, before the first allocation.
    ///
    /// # Safety
    ///
    /// See [`Heap::init`]: the region must be valid, writable, and reserved
    /// for the heap for the rest of the kernel's life.
    pub unsafe fn init(&self, start: usize, size: usize) -> Result<(), HeapError> {
        // SAFETY: forwarded to the caller's obligation.
        unsafe { self.inner.lock().init(start as *mut u8, size) }
    }

    /// Whether `init` has succeeded.
    pub fn is_initialized(&self) -> bool {
        self.inner.lock().is_initialized()
    }

    /// Current counters (takes the lock briefly).
    pub fn stats(&self) -> HeapStats {
        self.inner.lock().stats()
    }

    /// Runs the free-list invariant check under the lock.
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        self.inner.lock().check_invariants()
    }
}

// SAFETY: `Heap` upholds the `GlobalAlloc` contract: distinct live
// allocations never overlap, memory is never moved, and the lock makes the
// methods safe to call from any thread.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match self.inner.lock().alloc(layout) {
            Some(p) => p.as_ptr(),
            None => ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // An invalid free is a kernel bug. Leaking the block is the least bad
        // response from inside `dealloc`, which cannot report errors; the
        // `failures`-style accounting is visible through `stats()` only via
        // the `frees` counter not advancing.
        // SAFETY: the `GlobalAlloc` contract gives us a pointer from `alloc`.
        let _ = unsafe { self.inner.lock().dealloc(ptr, layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::Rng;
    use std::vec::Vec;

    /// A page-aligned arena allocated from the std allocator for the heap
    /// under test to manage.
    struct Arena {
        ptr: *mut u8,
        layout: Layout,
    }

    impl Arena {
        fn new(size: usize) -> Self {
            let layout = Layout::from_size_align(size, 4096).unwrap();
            // SAFETY: non-zero size, valid layout.
            let ptr = unsafe { std::alloc::alloc(layout) };
            assert!(!ptr.is_null());
            Arena { ptr, layout }
        }

        fn heap(&self) -> Heap {
            let mut h = Heap::new();
            // SAFETY: the arena is exclusively ours and outlives the heap in
            // every test.
            unsafe { h.init(self.ptr, self.layout.size()) }.unwrap();
            h
        }

        fn start(&self) -> usize {
            self.ptr as usize
        }
    }

    impl Drop for Arena {
        fn drop(&mut self) {
            // SAFETY: allocated in `new` with this layout.
            unsafe { std::alloc::dealloc(self.ptr, self.layout) }
        }
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    #[test]
    fn init_rejects_bad_regions() {
        let arena = Arena::new(4096);
        let mut h = Heap::new();
        assert!(!h.is_initialized());
        assert!(h.alloc(layout(1, 1)).is_none());
        // SAFETY: arena memory.
        unsafe {
            assert_eq!(h.init(arena.ptr, 8), Err(HeapError::RegionTooSmall));
            assert_eq!(h.init(arena.ptr.add(3), 17), Err(HeapError::RegionTooSmall));
            assert_eq!(h.init(arena.ptr.add(3), 29), Ok(()));
            assert_eq!(h.init(arena.ptr, 4096), Err(HeapError::AlreadyInitialized));
        }
        // 3..32 rounds inward to 16..32: one granule.
        assert_eq!(h.stats().total_bytes, 16);
        assert!(h.alloc(layout(1, 1)).is_some());
        assert!(h.alloc(layout(1, 1)).is_none());
        // The pre-init attempt above and this one both count.
        assert_eq!(h.stats().failures, 2);
    }

    #[test]
    fn alloc_free_single_block_restores_heap() {
        let arena = Arena::new(4096);
        let mut h = arena.heap();
        let before = h.stats();
        assert_eq!(before.free_blocks, 1);
        assert_eq!(before.free_bytes, 4096);

        let l = layout(100, 8);
        let p = h.alloc(l).unwrap();
        assert_eq!(p.as_ptr() as usize, arena.start());
        let s = h.stats();
        assert_eq!(s.free_bytes, 4096 - 112);
        assert_eq!(s.allocated_bytes, 112);
        assert_eq!(s.free_blocks, 1);
        h.check_invariants().unwrap();

        // SAFETY: p came from alloc with l.
        unsafe { h.dealloc(p.as_ptr(), l) }.unwrap();
        let after = h.stats();
        assert_eq!(after.free_bytes, 4096);
        assert_eq!(after.free_blocks, 1);
        assert_eq!(after.largest_free, 4096);
        assert_eq!(after.allocations, 1);
        assert_eq!(after.frees, 1);
        h.check_invariants().unwrap();
    }

    #[test]
    fn alignment_is_honoured_and_padding_stays_free() {
        let arena = Arena::new(16 * 4096);
        let mut h = arena.heap();
        // Take 16 bytes so the next aligned slot needs padding.
        let small = h.alloc(layout(1, 1)).unwrap();
        for &align in &[32usize, 64, 256, 1024, 4096] {
            let l = layout(24, align);
            let p = h.alloc(l).unwrap();
            assert_eq!(p.as_ptr() as usize % align, 0, "align {align}");
            h.check_invariants().unwrap();
            // SAFETY: p from alloc with l.
            unsafe { h.dealloc(p.as_ptr(), l) }.unwrap();
            h.check_invariants().unwrap();
        }
        // SAFETY: from alloc above.
        unsafe { h.dealloc(small.as_ptr(), layout(1, 1)) }.unwrap();
        let s = h.stats();
        assert_eq!(s.free_blocks, 1);
        assert_eq!(s.free_bytes, 16 * 4096);
    }

    #[test]
    fn exact_fit_consumes_block_without_leaving_a_zero_tail() {
        let arena = Arena::new(4096);
        let mut h = arena.heap();
        let l = layout(4096, 16);
        let p = h.alloc(l).unwrap();
        let s = h.stats();
        assert_eq!(s.free_blocks, 0);
        assert_eq!(s.free_bytes, 0);
        assert!(h.alloc(layout(1, 1)).is_none());
        // SAFETY: from alloc.
        unsafe { h.dealloc(p.as_ptr(), l) }.unwrap();
        assert_eq!(h.stats().free_blocks, 1);
        h.check_invariants().unwrap();
    }

    #[test]
    fn coalesces_in_both_directions() {
        let arena = Arena::new(4096);
        let mut h = arena.heap();
        let l = layout(256, 16);
        let a = h.alloc(l).unwrap();
        let b = h.alloc(l).unwrap();
        let c = h.alloc(l).unwrap();
        assert_eq!(h.stats().free_blocks, 1);
        // SAFETY: all from alloc with l.
        unsafe {
            h.dealloc(a.as_ptr(), l).unwrap(); // free, [b], [c], free
            assert_eq!(h.stats().free_blocks, 2);
            h.dealloc(c.as_ptr(), l).unwrap(); // merges with the tail block
            assert_eq!(h.stats().free_blocks, 2);
            h.dealloc(b.as_ptr(), l).unwrap(); // merges with both
        }
        let s = h.stats();
        assert_eq!(s.free_blocks, 1);
        assert_eq!(s.free_bytes, 4096);
        h.check_invariants().unwrap();
    }

    #[test]
    fn first_fit_reuses_the_lowest_hole() {
        let arena = Arena::new(4096);
        let mut h = arena.heap();
        let l = layout(512, 16);
        let a = h.alloc(l).unwrap();
        let _b = h.alloc(l).unwrap();
        let c = h.alloc(l).unwrap();
        let _d = h.alloc(l).unwrap();
        // SAFETY: from alloc with l.
        unsafe {
            h.dealloc(c.as_ptr(), l).unwrap();
            h.dealloc(a.as_ptr(), l).unwrap();
        }
        // The lowest hole is a's; a 256-byte request lands there and splits it.
        let e = h.alloc(layout(256, 16)).unwrap();
        assert_eq!(e.as_ptr(), a.as_ptr());
        h.check_invariants().unwrap();
    }

    #[test]
    fn invalid_frees_are_rejected_without_damage() {
        let arena = Arena::new(4096);
        let mut h = arena.heap();
        let l = layout(64, 16);
        let p = h.alloc(l).unwrap();
        let q = h.alloc(l).unwrap();
        let before = h.stats();
        // SAFETY: the pointers are inside the arena; the calls are expected
        // to fail without touching memory.
        unsafe {
            // Misaligned.
            assert_eq!(h.dealloc(p.as_ptr().add(1), l), Err(HeapError::InvalidFree));
            // Outside the region.
            assert_eq!(h.dealloc(arena.ptr.sub(16), l), Err(HeapError::InvalidFree));
            assert_eq!(h.dealloc(arena.ptr.add(4096 - 16), l), Err(HeapError::InvalidFree));
            // A block overlapping free space (q with a size reaching into the
            // free tail).
            assert_eq!(h.dealloc(q.as_ptr(), layout(128, 16)), Err(HeapError::InvalidFree));
        }
        assert_eq!(h.stats(), before);
        h.check_invariants().unwrap();
        // SAFETY: legitimate frees.
        unsafe {
            h.dealloc(p.as_ptr(), l).unwrap();
            // Double free: p now sits in a free block.
            assert_eq!(h.dealloc(p.as_ptr(), l), Err(HeapError::InvalidFree));
            h.dealloc(q.as_ptr(), l).unwrap();
            assert_eq!(h.dealloc(q.as_ptr(), l), Err(HeapError::InvalidFree));
        }
        assert_eq!(h.stats().free_blocks, 1);
        h.check_invariants().unwrap();
    }

    #[test]
    fn huge_requests_fail_cleanly() {
        let arena = Arena::new(4096);
        let mut h = arena.heap();
        assert!(h.alloc(layout(4097, 1)).is_none());
        assert!(h.alloc(layout(isize::MAX as usize - 100, 1)).is_none());
        assert!(h.alloc(layout(16, 1 << 20)).is_none());
        assert_eq!(h.stats().failures, 3);
        h.check_invariants().unwrap();
        assert!(h.alloc(layout(4096, 1)).is_some());
    }

    /// A live allocation in the randomized tests: where it is, what layout
    /// it was made with and the byte pattern it was filled with.
    struct Live {
        ptr: *mut u8,
        layout: Layout,
        tag: u8,
    }

    fn fill(l: &Live) {
        // SAFETY: the block is ours and at least `layout.size()` bytes.
        unsafe { ptr::write_bytes(l.ptr, l.tag, l.layout.size()) }
    }

    fn verify(l: &Live) {
        // SAFETY: as in `fill`.
        let bytes = unsafe { core::slice::from_raw_parts(l.ptr, l.layout.size()) };
        assert!(bytes.iter().all(|&b| b == l.tag), "allocation at {:p} ({:?}) was overwritten", l.ptr, l.layout);
    }

    fn assert_disjoint(live: &[Live], arena: &Arena) {
        let mut ranges: Vec<(usize, usize)> =
            live.iter().map(|l| (l.ptr as usize, l.ptr as usize + l.layout.size().max(1))).collect();
        ranges.sort_unstable();
        for w in ranges.windows(2) {
            assert!(w[0].1 <= w[1].0, "overlap between {:?} and {:?}", w[0], w[1]);
        }
        for &(s, e) in &ranges {
            assert!(s >= arena.start() && e <= arena.start() + arena.layout.size());
        }
    }

    fn randomized(seed: u64, arena_size: usize, ops: usize, max_size: u64, max_align_log2: u32) {
        let arena = Arena::new(arena_size);
        let mut h = arena.heap();
        let mut rng = Rng::new(seed);
        let mut live: Vec<Live> = Vec::new();
        let mut tag = 0u8;
        let mut successes = 0usize;
        let mut failures = 0usize;

        for step in 0..ops {
            // Bias towards allocation while the heap is mostly free, then
            // towards freeing, so the live set swings up and down.
            let do_alloc = live.is_empty() || rng.chance(if live.len() < 200 { 3 } else { 1 }, 4);
            if do_alloc {
                let size = rng.range(1, max_size) as usize;
                let align = 1usize << rng.below(max_align_log2 as u64 + 1);
                let l = layout(size, align);
                match h.alloc(l) {
                    Some(p) => {
                        assert_eq!(p.as_ptr() as usize % align, 0);
                        tag = tag.wrapping_add(1);
                        let entry = Live { ptr: p.as_ptr(), layout: l, tag };
                        fill(&entry);
                        live.push(entry);
                        successes += 1;
                    }
                    None => failures += 1,
                }
            } else {
                let i = rng.below(live.len() as u64) as usize;
                let entry = live.swap_remove(i);
                verify(&entry);
                // SAFETY: entry came from alloc with its layout.
                unsafe { h.dealloc(entry.ptr, entry.layout) }.unwrap();
            }
            if step % 97 == 0 {
                h.check_invariants().unwrap();
                assert_disjoint(&live, &arena);
            }
        }
        assert!(successes > ops / 4, "too few successful allocations ({successes})");
        // Allocation failures are expected when the arena is full; the test
        // only cares that they do not corrupt anything.
        let _ = failures;

        h.check_invariants().unwrap();
        assert_disjoint(&live, &arena);
        for l in &live {
            verify(l);
        }
        // Free everything in random order; the heap must coalesce back into
        // one block covering the whole arena.
        while !live.is_empty() {
            let i = rng.below(live.len() as u64) as usize;
            let entry = live.swap_remove(i);
            // SAFETY: as above.
            unsafe { h.dealloc(entry.ptr, entry.layout) }.unwrap();
        }
        let s = h.stats();
        assert_eq!(s.free_blocks, 1, "not coalesced: {s:?}");
        assert_eq!(s.free_bytes, arena_size);
        assert_eq!(s.largest_free, arena_size);
        assert_eq!(s.allocated_bytes, 0);
        assert_eq!(s.allocations, s.frees);
        h.check_invariants().unwrap();
    }

    #[test]
    fn randomized_small_sizes_mixed_alignments() {
        randomized(1, 256 * 1024, 20_000, 128, 7);
    }

    #[test]
    fn randomized_large_sizes_page_alignment() {
        randomized(2, 2 * 1024 * 1024, 10_000, 8192, 12);
    }

    #[test]
    fn randomized_tight_arena_many_failures() {
        randomized(3, 64 * 1024, 20_000, 1024, 6);
    }

    #[test]
    fn randomized_several_seeds() {
        for seed in 10..18 {
            randomized(seed, 128 * 1024, 5_000, 512, 9);
        }
    }

    #[test]
    fn kernel_heap_global_alloc_interface() {
        let arena = Arena::new(64 * 1024);
        let heap = KernelHeap::new();
        assert!(!heap.is_initialized());
        // SAFETY: before init, alloc must return null, not crash.
        assert!(unsafe { heap.alloc(layout(8, 8)) }.is_null());
        // SAFETY: arena memory, exclusive.
        unsafe { heap.init(arena.start(), 64 * 1024) }.unwrap();
        assert!(heap.is_initialized());
        // SAFETY: arena memory.
        assert_eq!(unsafe { heap.init(arena.start(), 64 * 1024) }, Err(HeapError::AlreadyInitialized));

        let mut ptrs = Vec::new();
        for i in 0..100usize {
            let l = layout(i * 7 + 1, 1 << (i % 6));
            // SAFETY: GlobalAlloc contract with a valid layout.
            let p = unsafe { heap.alloc(l) };
            assert!(!p.is_null());
            ptrs.push((p, l));
        }
        // Zeroed allocation through the default method.
        let lz = layout(300, 16);
        // SAFETY: valid layout.
        let z = unsafe { heap.alloc_zeroed(lz) };
        assert!(!z.is_null());
        // SAFETY: 300 bytes were allocated.
        assert!(unsafe { core::slice::from_raw_parts(z, 300) }.iter().all(|&b| b == 0));
        // Realloc through the default method keeps the contents.
        // SAFETY: valid pointer/layout pair.
        unsafe {
            ptr::write_bytes(z, 0xAB, 300);
            let r = heap.realloc(z, lz, 5000);
            assert!(!r.is_null());
            assert!(core::slice::from_raw_parts(r, 300).iter().all(|&b| b == 0xAB));
            heap.dealloc(r, layout(5000, 16));
        }
        heap.check_invariants().unwrap();
        for (p, l) in ptrs {
            // SAFETY: from alloc above.
            unsafe { heap.dealloc(p, l) };
        }
        let s = heap.stats();
        assert_eq!(s.free_blocks, 1);
        assert_eq!(s.free_bytes, 64 * 1024);
        heap.check_invariants().unwrap();

        // An invalid free through GlobalAlloc is ignored, not fatal.
        // SAFETY: the pointer is inside the arena; the call must not touch it.
        unsafe { heap.dealloc(arena.ptr.add(32), layout(16, 16)) };
        assert_eq!(heap.stats(), s);
    }

    #[test]
    fn kernel_heap_is_sync_and_usable_from_threads() {
        use std::sync::Arc;
        use std::thread;

        let arena = Arena::new(1024 * 1024);
        let heap = Arc::new(KernelHeap::new());
        // SAFETY: arena outlives the threads (joined below).
        unsafe { heap.init(arena.start(), 1024 * 1024) }.unwrap();

        let handles: Vec<_> = (0..4u64)
            .map(|t| {
                let heap = Arc::clone(&heap);
                thread::spawn(move || {
                    let mut rng = Rng::new(100 + t);
                    let mut live: Vec<(*mut u8, Layout)> = Vec::new();
                    for _ in 0..2000 {
                        if live.len() < 50 && rng.chance(2, 3) {
                            let l = layout(rng.range(1, 256) as usize, 16);
                            // SAFETY: valid layout.
                            let p = unsafe { heap.alloc(l) };
                            if !p.is_null() {
                                // SAFETY: block is ours.
                                unsafe { ptr::write_bytes(p, t as u8, l.size()) };
                                live.push((p, l));
                            }
                        } else if let Some((p, l)) = live.pop() {
                            // SAFETY: block is ours.
                            let bytes = unsafe { core::slice::from_raw_parts(p, l.size()) };
                            assert!(bytes.iter().all(|&b| b == t as u8));
                            // SAFETY: from alloc.
                            unsafe { heap.dealloc(p, l) };
                        }
                    }
                    for (p, l) in live {
                        // SAFETY: from alloc.
                        unsafe { heap.dealloc(p, l) };
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let s = heap.stats();
        assert_eq!(s.free_blocks, 1);
        assert_eq!(s.free_bytes, 1024 * 1024);
        heap.check_invariants().unwrap();
    }
}
