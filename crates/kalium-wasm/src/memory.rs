//! Linear memory.
//!
//! A minimal but complete implementation: a `Vec<u8>` grown in 64 KiB pages
//! with every access bounds-checked. The kernel's host functions use it
//! through [`crate::HostCtx`].
//!
//! A memory is shared between the instances that import it through a
//! [`MemoryRef`] (`Rc<RefCell<Memory>>`): growing it through any importer
//! is visible to all of them, because there is only one object. See
//! [`crate::instance`] for why `Rc`/`RefCell` are the right tools in this
//! kernel.

use crate::budget::BudgetRef;
use crate::error::Trap;
use crate::types::{MemType, ResizableLimits, MAX_PAGES, PAGE_SIZE};
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

/// A shared handle to a linear memory: what an instance holds for each
/// memory in its index space, what it exports and what it imports. The
/// interpreter borrows it mutably for the duration of an interpreter loop
/// and releases it around every host call, so a host function may borrow
/// the memory itself (through [`crate::HostCtx`]) and may run another
/// instance that shares it.
pub type MemoryRef = Rc<RefCell<Memory>>;

/// A bounds-checked linear memory.
///
/// A memory may be charged to a [`Budget`](crate::Budget)
/// ([`set_budget`](Memory::set_budget)): its size is then counted against
/// the budget, a `grow` the budget refuses fails like one the allocator
/// refuses, and the charge is released when the memory is dropped.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Memory {
    bytes: Vec<u8>,
    /// Upper bound in pages (the smaller of the type's max and the instance
    /// limit).
    max_pages: u32,
    /// The `max` of the memory's declared type, which import matching sees
    /// (`None` for a memory declared without one, however small the
    /// runtime cap `max_pages` is).
    declared_max: Option<u32>,
    /// The budget the memory is charged to, and how many bytes it holds
    /// of it (its size, from `set_budget` on).
    budget: Option<BudgetRef>,
    charged: usize,
}

impl Clone for Memory {
    /// A copy of the contents and limits that is charged to no budget
    /// (the original keeps its charge).
    fn clone(&self) -> Memory {
        Memory {
            bytes: self.bytes.clone(),
            max_pages: self.max_pages,
            declared_max: self.declared_max,
            budget: None,
            charged: 0,
        }
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        if let Some(b) = &self.budget {
            b.release(self.charged);
        }
    }
}

impl Memory {
    /// Charges the memory's current size to `budget`, and every later
    /// growth to it, until the memory is dropped (which releases the
    /// charge) or `set_budget` is called again (which moves it). Returns
    /// `false`, leaving the memory charged to its previous budget if any,
    /// when the current size does not fit `budget`.
    pub fn set_budget(&mut self, budget: &BudgetRef) -> bool {
        if !budget.reserve(self.bytes.len()) {
            return false;
        }
        if let Some(old) = self.budget.take() {
            old.release(self.charged);
        }
        self.charged = self.bytes.len();
        self.budget = Some(budget.clone());
        true
    }

    /// The budget the memory is charged to, if any.
    pub fn budget(&self) -> Option<&BudgetRef> {
        self.budget.as_ref()
    }
    /// Create a memory of `min_pages` pages that may grow to `max_pages`:
    /// a memory of type `(memory min_pages max_pages)`. Returns `None` if
    /// `min_pages > max_pages` or the allocation would be larger than
    /// 4 GiB.
    pub fn new(min_pages: u32, max_pages: u32) -> Option<Memory> {
        Memory::with_type(
            MemType {
                limits: ResizableLimits {
                    min: min_pages,
                    max: Some(max_pages),
                },
            },
            max_pages,
        )
    }

    /// Create a memory of type `ty` that may grow to the smaller of the
    /// type's max and `limit` pages (the instance's
    /// [`crate::Limits::max_memory_pages`]). Returns `None` if the type's
    /// minimum exceeds that bound or the allocation fails.
    pub fn with_type(ty: MemType, limit: u32) -> Option<Memory> {
        let max_pages = ty.limits.max.map_or(limit, |m| m.min(limit)).min(MAX_PAGES);
        if ty.limits.min > max_pages {
            return None;
        }
        let len = (ty.limits.min as usize).checked_mul(PAGE_SIZE)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len).ok()?;
        bytes.resize(len, 0);
        Some(Memory {
            bytes,
            max_pages,
            declared_max: ty.limits.max,
            budget: None,
            charged: 0,
        })
    }

    /// Create a memory of type `ty` (capped like [`Memory::with_type`])
    /// whose contents are `bytes`, which must be a whole number of pages
    /// within the type's limits and `limit`. Returns `None` otherwise. The
    /// bytes are taken over, not copied.
    pub fn with_contents(ty: MemType, bytes: Vec<u8>, limit: u32) -> Option<Memory> {
        let max_pages = ty.limits.max.map_or(limit, |m| m.min(limit)).min(MAX_PAGES);
        if !bytes.len().is_multiple_of(PAGE_SIZE) {
            return None;
        }
        let pages = u32::try_from(bytes.len() / PAGE_SIZE).ok()?;
        if pages < ty.limits.min || pages > max_pages {
            return None;
        }
        Some(Memory {
            bytes,
            max_pages,
            declared_max: ty.limits.max,
            budget: None,
            charged: 0,
        })
    }

    /// Wrap the memory in a shared handle.
    pub fn into_shared(self) -> MemoryRef {
        Rc::new(RefCell::new(self))
    }

    /// The memory's current type: its size in pages as the minimum and the
    /// declared maximum. This is what an import of the memory is matched
    /// against (`import.min <= size`, and `import.max`, if given, requires
    /// a declared maximum that is no larger).
    pub fn ty(&self) -> MemType {
        MemType {
            limits: ResizableLimits {
                min: self.size_pages(),
                max: self.declared_max,
            },
        }
    }

    /// Whether this memory can be imported as `import` (import matching
    /// per the spec, limits subtyping).
    pub fn matches(&self, import: &MemType) -> bool {
        limits_match(&self.ty().limits, &import.limits)
    }

    /// Current size in bytes.
    pub fn size_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// Current size in pages.
    pub fn size_pages(&self) -> u32 {
        (self.bytes.len() / PAGE_SIZE) as u32
    }

    /// Maximum size in pages the memory can grow to (the runtime cap, not
    /// necessarily the declared maximum).
    pub fn max_pages(&self) -> u32 {
        self.max_pages
    }

    /// Grow by `delta` pages. Returns the old size in pages, or `None` if the
    /// limit would be exceeded, the budget refuses the growth or the
    /// allocation fails (`memory.grow` semantics: the caller turns `None`
    /// into `-1`).
    pub fn grow(&mut self, delta: u32) -> Option<u32> {
        let old = self.size_pages();
        let new = old.checked_add(delta)?;
        if new > self.max_pages {
            return None;
        }
        let new_len = (new as usize).checked_mul(PAGE_SIZE)?;
        let added = new_len - self.bytes.len();
        if self.budget.as_ref().is_some_and(|b| !b.reserve(added)) {
            return None;
        }
        if self.bytes.try_reserve_exact(added).is_err() {
            if let Some(b) = &self.budget {
                b.release(added);
            }
            return None;
        }
        self.bytes.resize(new_len, 0);
        if self.budget.is_some() {
            self.charged += added;
        }
        Some(old)
    }

    /// The byte range `[off, off + len)` if it lies within memory.
    fn range(&self, off: u32, len: usize) -> Result<core::ops::Range<usize>, Trap> {
        let start = off as usize;
        let end = start.checked_add(len).ok_or(Trap::OutOfBounds)?;
        if end > self.bytes.len() {
            return Err(Trap::OutOfBounds);
        }
        Ok(start..end)
    }

    /// The interpreter's load: the `N` bytes at `addr + offset` (the
    /// effective address of a Wasm memory instruction, which may exceed
    /// `u32::MAX` and then always traps). Inlined so that the constant `N`
    /// folds into a single bounds check.
    #[inline(always)]
    pub(crate) fn load<const N: usize>(&self, addr: u32, offset: u32) -> Result<[u8; N], Trap> {
        // Both operands are < 2^32, so the sum cannot overflow a u64.
        let start = addr as u64 + offset as u64;
        let end = start + N as u64;
        if end > self.bytes.len() as u64 {
            return Err(Trap::OutOfBounds);
        }
        // `end <= len <= usize::MAX`, so the casts are lossless.
        let src = self
            .bytes
            .get(start as usize..end as usize)
            .ok_or(Trap::OutOfBounds)?;
        let mut out = [0u8; N];
        out.copy_from_slice(src);
        Ok(out)
    }

    /// The interpreter's store: write `data` at `addr + offset`. See
    /// [`Memory::load`].
    #[inline(always)]
    pub(crate) fn store(&mut self, addr: u32, offset: u32, data: &[u8]) -> Result<(), Trap> {
        let start = addr as u64 + offset as u64;
        let end = start + data.len() as u64;
        if end > self.bytes.len() as u64 {
            return Err(Trap::OutOfBounds);
        }
        let dst = self
            .bytes
            .get_mut(start as usize..end as usize)
            .ok_or(Trap::OutOfBounds)?;
        dst.copy_from_slice(data);
        Ok(())
    }

    /// Copy `buf.len()` bytes starting at `off` into `buf`.
    pub fn read(&self, off: u32, buf: &mut [u8]) -> Result<(), Trap> {
        let r = self.range(off, buf.len())?;
        buf.copy_from_slice(&self.bytes[r]);
        Ok(())
    }

    /// Copy `data` into memory starting at `off`.
    pub fn write(&mut self, off: u32, data: &[u8]) -> Result<(), Trap> {
        let r = self.range(off, data.len())?;
        self.bytes[r].copy_from_slice(data);
        Ok(())
    }

    /// Borrow `len` bytes starting at `off`.
    pub fn slice(&self, off: u32, len: usize) -> Result<&[u8], Trap> {
        let r = self.range(off, len)?;
        Ok(&self.bytes[r])
    }

    /// Mutably borrow `len` bytes starting at `off`.
    pub fn slice_mut(&mut self, off: u32, len: usize) -> Result<&mut [u8], Trap> {
        let r = self.range(off, len)?;
        Ok(&mut self.bytes[r])
    }

    /// Fill `len` bytes starting at `off` with `value` (`memory.fill`).
    pub fn fill(&mut self, off: u32, value: u8, len: u32) -> Result<(), Trap> {
        let r = self.range(off, len as usize)?;
        self.bytes[r].fill(value);
        Ok(())
    }

    /// Copy `len` bytes from `src` to `dst` within this memory, overlapping
    /// allowed (`memory.copy`). Both ranges are checked before anything is
    /// written.
    pub fn copy_within(&mut self, dst: u32, src: u32, len: u32) -> Result<(), Trap> {
        let s = self.range(src, len as usize)?;
        let _ = self.range(dst, len as usize)?;
        self.bytes.copy_within(s, dst as usize);
        Ok(())
    }

    /// The whole memory as a byte slice.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}

/// Import matching for limits (`limits <= limits'` in the spec): the
/// actual minimum is at least the imported one, and if the import has a
/// maximum, the actual type has one that is no larger.
pub(crate) fn limits_match(actual: &ResizableLimits, import: &ResizableLimits) -> bool {
    actual.min >= import.min
        && match import.max {
            None => true,
            Some(m) => actual.max.is_some_and(|a| a <= m),
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_and_import_matching() {
        let lim = |min, max| ResizableLimits { min, max };
        let m = Memory::new(1, 3).unwrap();
        assert_eq!(m.ty().limits, lim(1, Some(3)));
        // Declared without a maximum: the runtime cap does not show up in
        // the type, so an import that wants a maximum does not match.
        let mut m = Memory::with_type(
            MemType {
                limits: lim(2, None),
            },
            4,
        )
        .unwrap();
        assert_eq!(m.max_pages(), 4);
        assert_eq!(m.ty().limits, lim(2, None));
        assert!(m.matches(&MemType {
            limits: lim(0, None)
        }));
        assert!(m.matches(&MemType {
            limits: lim(2, None)
        }));
        assert!(!m.matches(&MemType {
            limits: lim(3, None)
        }));
        assert!(!m.matches(&MemType {
            limits: lim(0, Some(4))
        }));
        // Growth is reflected in the type's minimum.
        assert_eq!(m.grow(1), Some(2));
        assert!(m.matches(&MemType {
            limits: lim(3, None)
        }));
        let m = Memory::with_type(
            MemType {
                limits: lim(1, Some(2)),
            },
            65536,
        )
        .unwrap();
        assert!(m.matches(&MemType {
            limits: lim(1, Some(2))
        }));
        assert!(m.matches(&MemType {
            limits: lim(1, Some(3))
        }));
        assert!(!m.matches(&MemType {
            limits: lim(1, Some(1))
        }));
        assert!(Memory::with_type(
            MemType {
                limits: lim(3, Some(2))
            },
            65536
        )
        .is_none());
        // Shared handles see each other's growth.
        let a = Memory::new(1, 3).unwrap().into_shared();
        let b = a.clone();
        assert_eq!(a.borrow_mut().grow(1), Some(1));
        assert_eq!(b.borrow().size_pages(), 2);
    }

    #[test]
    fn new_and_grow() {
        let mut m = Memory::new(1, 3).unwrap();
        assert_eq!(m.size_bytes(), PAGE_SIZE);
        assert_eq!(m.size_pages(), 1);
        assert_eq!(m.grow(1), Some(1));
        assert_eq!(m.size_pages(), 2);
        assert_eq!(m.grow(2), None);
        assert_eq!(m.grow(1), Some(2));
        assert_eq!(m.grow(0), Some(3));
        assert_eq!(m.grow(u32::MAX), None);
        assert!(Memory::new(2, 1).is_none());
        assert_eq!(Memory::new(0, 0).unwrap().size_bytes(), 0);
    }

    #[test]
    fn bounds_checked_access() {
        let mut m = Memory::new(1, 1).unwrap();
        m.write(0, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        m.read(0, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
        assert_eq!(
            m.write(PAGE_SIZE as u32 - 3, &[0; 4]),
            Err(Trap::OutOfBounds)
        );
        m.write(PAGE_SIZE as u32 - 4, &[9; 4]).unwrap();
        assert_eq!(m.read(u32::MAX, &mut buf), Err(Trap::OutOfBounds));
        assert_eq!(m.read(u32::MAX, &mut []), Err(Trap::OutOfBounds));
        assert!(m.read(PAGE_SIZE as u32, &mut []).is_ok());
        assert_eq!(m.fill(10, 0xAB, 6), Ok(()));
        assert_eq!(m.slice(10, 6).unwrap(), &[0xAB; 6]);
        assert_eq!(m.fill(PAGE_SIZE as u32, 0, 1), Err(Trap::OutOfBounds));
        m.copy_within(12, 10, 4).unwrap();
        m.copy_within(0, 10, 8).unwrap();
        assert_eq!(
            m.slice(0, 8).unwrap(),
            &[0xAB, 0xAB, 0xAB, 0xAB, 0xAB, 0xAB, 0, 0]
        );
        assert_eq!(
            m.copy_within(PAGE_SIZE as u32 - 2, 0, 4),
            Err(Trap::OutOfBounds)
        );
        assert_eq!(
            m.copy_within(0, PAGE_SIZE as u32 - 2, 4),
            Err(Trap::OutOfBounds)
        );
    }

    #[test]
    fn interpreter_load_store() {
        let mut m = Memory::new(1, 1).unwrap();
        m.store(4, 4, &[1, 2, 3, 4]).unwrap();
        assert_eq!(m.load::<4>(0, 8), Ok([1, 2, 3, 4]));
        assert_eq!(m.load::<2>(9, 0), Ok([2, 3]));
        assert_eq!(m.load::<1>(PAGE_SIZE as u32 - 1, 0), Ok([0]));
        assert_eq!(m.load::<1>(PAGE_SIZE as u32, 0), Err(Trap::OutOfBounds));
        assert_eq!(m.load::<4>(PAGE_SIZE as u32 - 3, 0), Err(Trap::OutOfBounds));
        // The effective address does not wrap around.
        assert_eq!(m.load::<4>(u32::MAX, 1), Err(Trap::OutOfBounds));
        assert_eq!(m.load::<4>(1, u32::MAX), Err(Trap::OutOfBounds));
        assert_eq!(m.store(u32::MAX, u32::MAX, &[0]), Err(Trap::OutOfBounds));
        assert_eq!(
            m.store(PAGE_SIZE as u32 - 1, 0, &[7, 8]),
            Err(Trap::OutOfBounds)
        );
        m.store(PAGE_SIZE as u32 - 1, 0, &[7]).unwrap();
        assert_eq!(m.load::<1>(PAGE_SIZE as u32 - 1, 0), Ok([7]));
        // Zero-length accesses at the end are fine, one past it is not.
        assert_eq!(m.load::<0>(PAGE_SIZE as u32, 0), Ok([]));
        assert_eq!(m.load::<0>(PAGE_SIZE as u32 + 1, 0), Err(Trap::OutOfBounds));
    }
}
