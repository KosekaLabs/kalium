//! Memory objects: the kernel-side representation of `kalium:memory`
//! objects in M1.
//!
//! A [`MemObject`] is a fixed-size, zero-initialised byte array. Offsets are
//! `u64` because that is what the host ABI passes (`memobj_read(cap, offset:
//! i64, ...)`); every access is bounds-checked against the object's size and
//! never panics on a bad offset or length. Backing by physical frames with
//! MMU mapping is post-M1.
//!
//! No `unsafe` here.

#![forbid(unsafe_code)]

use alloc::vec::Vec;
use core::fmt;

/// Largest memory object that may be created, in bytes (16 MiB in M1).
///
/// This is the ABI's limit, so the kernel and the SDK agree by construction.
pub const MAX_MEMOBJ_BYTES: u64 = kalium_abi::limits::MAX_MEMOBJ_SIZE;

/// Errors from [`MemObject`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemError {
    /// Requested size is above [`MAX_MEMOBJ_BYTES`] (ABI `ETOOBIG`).
    TooBig,
    /// The backing allocation failed (ABI `ENOMEM`).
    OutOfMemory,
    /// `offset + len` is outside the object (ABI `EINVAL`).
    OutOfBounds,
}

impl fmt::Display for MemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            MemError::TooBig => "memory object too large",
            MemError::OutOfMemory => "out of memory",
            MemError::OutOfBounds => "access outside the memory object",
        };
        f.write_str(s)
    }
}

/// A zero-initialised, bounds-checked byte object.
#[derive(Clone, PartialEq, Eq)]
pub struct MemObject {
    bytes: Vec<u8>,
}

impl fmt::Debug for MemObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemObject")
            .field("size", &self.bytes.len())
            .finish()
    }
}

impl MemObject {
    /// Creates an object of `size` zero bytes.
    ///
    /// Fails with [`MemError::TooBig`] above [`MAX_MEMOBJ_BYTES`] and with
    /// [`MemError::OutOfMemory`] if the heap cannot provide the backing
    /// store. A zero-sized object is allowed.
    pub fn new(size: u64) -> Result<MemObject, MemError> {
        if size > MAX_MEMOBJ_BYTES {
            return Err(MemError::TooBig);
        }
        // MAX_MEMOBJ_BYTES fits in usize on every supported target (32-bit
        // and up); the conversion is checked anyway.
        let len = usize::try_from(size).map_err(|_| MemError::TooBig)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| MemError::OutOfMemory)?;
        bytes.resize(len, 0);
        Ok(MemObject { bytes })
    }

    /// Size in bytes.
    pub fn size(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// The whole contents.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Mutable access to the whole contents.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Resolves `[offset, offset + len)` to a `usize` range inside the
    /// object, or `OutOfBounds`.
    fn range(&self, offset: u64, len: usize) -> Result<core::ops::Range<usize>, MemError> {
        let start = usize::try_from(offset).map_err(|_| MemError::OutOfBounds)?;
        let end = start.checked_add(len).ok_or(MemError::OutOfBounds)?;
        if end > self.bytes.len() {
            return Err(MemError::OutOfBounds);
        }
        Ok(start..end)
    }

    /// Copies `buf.len()` bytes starting at `offset` into `buf`. Either the
    /// whole range is inside the object or nothing is copied.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        let r = self.range(offset, buf.len())?;
        buf.copy_from_slice(&self.bytes[r]);
        Ok(())
    }

    /// Copies `data` into the object at `offset`. Either the whole range is
    /// inside the object or nothing is written.
    pub fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), MemError> {
        let r = self.range(offset, data.len())?;
        self.bytes[r].copy_from_slice(data);
        Ok(())
    }

    /// Borrows `len` bytes at `offset`, for callers that want to avoid a
    /// copy (e.g. writing straight into linear memory).
    pub fn slice(&self, offset: u64, len: usize) -> Result<&[u8], MemError> {
        let r = self.range(offset, len)?;
        Ok(&self.bytes[r])
    }

    /// Mutably borrows `len` bytes at `offset`.
    pub fn slice_mut(&mut self, offset: u64, len: usize) -> Result<&mut [u8], MemError> {
        let r = self.range(offset, len)?;
        Ok(&mut self.bytes[r])
    }

    /// Sets `len` bytes at `offset` to zero.
    pub fn zero(&mut self, offset: u64, len: usize) -> Result<(), MemError> {
        self.slice_mut(offset, len)?.fill(0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::Rng;
    use std::vec;
    use std::vec::Vec;

    #[test]
    fn create_is_zeroed_and_sized() {
        let m = MemObject::new(1000).unwrap();
        assert_eq!(m.size(), 1000);
        assert!(m.as_slice().iter().all(|&b| b == 0));
        assert_eq!(std::format!("{m:?}"), "MemObject { size: 1000 }");
    }

    #[test]
    fn zero_sized_object() {
        let mut m = MemObject::new(0).unwrap();
        assert_eq!(m.size(), 0);
        assert_eq!(m.read(0, &mut []), Ok(()));
        assert_eq!(m.write(0, &[]), Ok(()));
        assert_eq!(m.read(1, &mut []), Err(MemError::OutOfBounds));
        assert_eq!(m.read(0, &mut [0]), Err(MemError::OutOfBounds));
    }

    #[test]
    fn size_limit() {
        assert_eq!(MAX_MEMOBJ_BYTES, 16 * 1024 * 1024);
        assert!(MemObject::new(MAX_MEMOBJ_BYTES).is_ok());
        assert_eq!(MemObject::new(MAX_MEMOBJ_BYTES + 1), Err(MemError::TooBig));
        assert_eq!(MemObject::new(u64::MAX), Err(MemError::TooBig));
        assert_eq!(MemObject::new(1 << 40), Err(MemError::TooBig));
    }

    #[test]
    fn read_write_roundtrip() {
        let mut m = MemObject::new(64).unwrap();
        let data: Vec<u8> = (0..16).collect();
        m.write(8, &data).unwrap();
        let mut out = [0u8; 16];
        m.read(8, &mut out).unwrap();
        assert_eq!(out, data.as_slice());
        // Untouched parts stay zero.
        let mut head = [0xFFu8; 8];
        m.read(0, &mut head).unwrap();
        assert_eq!(head, [0; 8]);
        // Writing at the very end is fine.
        m.write(60, &[1, 2, 3, 4]).unwrap();
        assert_eq!(m.slice(60, 4).unwrap(), &[1, 2, 3, 4]);
        m.zero(60, 2).unwrap();
        assert_eq!(m.slice(60, 4).unwrap(), &[0, 0, 3, 4]);
        m.as_mut_slice()[0] = 9;
        assert_eq!(m.as_slice()[0], 9);
    }

    #[test]
    fn bounds_are_checked_without_partial_effects() {
        let mut m = MemObject::new(64).unwrap();
        let snapshot = m.clone();
        let mut buf = [7u8; 8];
        assert_eq!(m.read(57, &mut buf), Err(MemError::OutOfBounds));
        assert_eq!(buf, [7; 8]);
        assert_eq!(m.read(64, &mut buf), Err(MemError::OutOfBounds));
        assert_eq!(m.read(u64::MAX, &mut buf), Err(MemError::OutOfBounds));
        assert_eq!(m.read(u64::MAX - 3, &mut buf), Err(MemError::OutOfBounds));
        assert_eq!(m.read(1 << 40, &mut [0]), Err(MemError::OutOfBounds));
        assert_eq!(m.write(60, &[0; 5]), Err(MemError::OutOfBounds));
        assert_eq!(m.write(u64::MAX, &[1]), Err(MemError::OutOfBounds));
        assert_eq!(m.write(u64::MAX, &[]), Err(MemError::OutOfBounds));
        assert_eq!(m.slice(0, 65), Err(MemError::OutOfBounds));
        assert_eq!(m.slice(1, usize::MAX), Err(MemError::OutOfBounds));
        assert_eq!(m.slice_mut(65, 0), Err(MemError::OutOfBounds));
        assert_eq!(m.zero(64, 1), Err(MemError::OutOfBounds));
        assert_eq!(m, snapshot);
        // Offset exactly at the end with zero length is allowed.
        assert_eq!(m.read(64, &mut []), Ok(()));
        assert_eq!(m.slice(64, 0), Ok(&[][..]));
    }

    #[test]
    fn randomized_against_a_model() {
        let size = 4096u64;
        let mut m = MemObject::new(size).unwrap();
        let mut model = vec![0u8; size as usize];
        let mut rng = Rng::new(42);
        for _ in 0..20_000 {
            let len = rng.below(300) as usize;
            // Mostly in range, sometimes near or past the end.
            let offset = if rng.chance(1, 10) {
                rng.range(size - 300, size + 300)
            } else {
                rng.below(size)
            };
            let in_bounds = offset.checked_add(len as u64).is_some_and(|e| e <= size);
            if rng.chance(1, 2) {
                let data: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
                let r = m.write(offset, &data);
                if in_bounds {
                    r.unwrap();
                    model[offset as usize..offset as usize + len].copy_from_slice(&data);
                } else {
                    assert_eq!(r, Err(MemError::OutOfBounds));
                }
            } else {
                let mut buf = vec![0xEEu8; len];
                let r = m.read(offset, &mut buf);
                if in_bounds {
                    r.unwrap();
                    assert_eq!(buf, &model[offset as usize..offset as usize + len]);
                } else {
                    assert_eq!(r, Err(MemError::OutOfBounds));
                    assert!(buf.iter().all(|&b| b == 0xEE));
                }
            }
        }
        assert_eq!(m.as_slice(), model.as_slice());
    }
}
