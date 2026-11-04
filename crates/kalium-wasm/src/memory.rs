//! Linear memory (owned by the interpreter step, F).
//!
//! A minimal but complete implementation: a `Vec<u8>` grown in 64 KiB pages
//! with every access bounds-checked. The interpreter may replace or extend
//! it; the public methods below are the ones the plan names, and the kernel's
//! host functions use them through `HostCtx`.

use crate::error::Trap;
use crate::types::{MAX_PAGES, PAGE_SIZE};
use alloc::vec::Vec;

/// A bounds-checked linear memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    bytes: Vec<u8>,
    /// Upper bound in pages (the smaller of the type's max and the instance
    /// limit).
    max_pages: u32,
}

impl Memory {
    /// Create a memory of `min_pages` pages that may grow to `max_pages`.
    /// Returns `None` if `min_pages > max_pages` or the allocation would be
    /// larger than 4 GiB.
    pub fn new(min_pages: u32, max_pages: u32) -> Option<Memory> {
        let max_pages = max_pages.min(MAX_PAGES);
        if min_pages > max_pages {
            return None;
        }
        let len = (min_pages as usize).checked_mul(PAGE_SIZE)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len).ok()?;
        bytes.resize(len, 0);
        Some(Memory { bytes, max_pages })
    }

    /// Current size in bytes.
    pub fn size_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// Current size in pages.
    pub fn size_pages(&self) -> u32 {
        (self.bytes.len() / PAGE_SIZE) as u32
    }

    /// Maximum size in pages.
    pub fn max_pages(&self) -> u32 {
        self.max_pages
    }

    /// Grow by `delta` pages. Returns the old size in pages, or `None` if the
    /// limit would be exceeded or the allocation fails (`memory.grow`
    /// semantics: the caller turns `None` into `-1`).
    pub fn grow(&mut self, delta: u32) -> Option<u32> {
        let old = self.size_pages();
        let new = old.checked_add(delta)?;
        if new > self.max_pages {
            return None;
        }
        let new_len = (new as usize).checked_mul(PAGE_SIZE)?;
        self.bytes
            .try_reserve_exact(new_len - self.bytes.len())
            .ok()?;
        self.bytes.resize(new_len, 0);
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
