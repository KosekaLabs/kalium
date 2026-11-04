//! Memory objects: kernel-held byte arrays accessed by copying.

use crate::{abi_len, raw, Cap, Errno};

/// Largest memory object the kernel creates.
pub const MAX_SIZE: u64 = kalium_abi::limits::MAX_MEMOBJ_SIZE;

/// A `MemObj` capability.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemObj(Cap);

impl MemObj {
    /// Creates a zeroed memory object of `size` bytes (`memobj_create`);
    /// the capability has `READ|WRITE|DERIVE|TRANSFER`.
    pub fn create(size: u64) -> Result<MemObj, Errno> {
        let size = i64::try_from(size).map_err(|_| Errno::TooBig)?;
        let mut out: i32 = Cap::NONE.raw();
        // SAFETY: `out` is a live, writable `i32` the kernel writes.
        let r = unsafe { raw::memobj_create(size, raw::addr_mut(&mut out)) };
        Errno::check(r)?;
        if out < 0 {
            return Err(Errno::Other(r));
        }
        Ok(MemObj(Cap(out)))
    }

    /// Wraps a capability that is (expected to be) a memory object.
    pub const fn from_cap(cap: Cap) -> MemObj {
        MemObj(cap)
    }

    /// The underlying capability.
    pub const fn cap(self) -> Cap {
        self.0
    }

    /// The object's size in bytes (`memobj_size`).
    pub fn size(self) -> Result<u64, Errno> {
        let mut out: i64 = 0;
        // SAFETY: `out` is a live, writable `i64` the kernel writes.
        let r = unsafe { raw::memobj_size(self.0.raw(), raw::addr_mut(&mut out)) };
        Errno::check(r)?;
        u64::try_from(out).map_err(|_| Errno::Other(r))
    }

    /// Copies `buf.len()` bytes starting at `offset` into `buf`
    /// (`memobj_read`); needs `READ`. Out-of-range reads fail with
    /// [`Errno::Inval`] from the kernel, never a partial copy.
    pub fn read(self, offset: u64, buf: &mut [u8]) -> Result<(), Errno> {
        let len = abi_len(buf.len())?;
        let offset = i64::try_from(offset).map_err(|_| Errno::Inval)?;
        // SAFETY: `buf` is a live, writable slice of `len` bytes; the kernel
        // writes at most `len` bytes at its offset.
        let r =
            unsafe { raw::memobj_read(self.0.raw(), offset, raw::addr_mut(buf.as_mut_ptr()), len) };
        Errno::check(r)
    }

    /// Copies `data` into the object at `offset` (`memobj_write`); needs
    /// `WRITE`.
    pub fn write(self, offset: u64, data: &[u8]) -> Result<(), Errno> {
        let len = abi_len(data.len())?;
        let offset = i64::try_from(offset).map_err(|_| Errno::Inval)?;
        // SAFETY: `data` is a live slice of `len` bytes the kernel reads;
        // nothing is written.
        let r = unsafe { raw::memobj_write(self.0.raw(), offset, raw::addr(data.as_ptr()), len) };
        Errno::check(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_report_stub_errors_and_reject_bad_sizes() {
        assert_eq!(MemObj::create(64).err(), Some(Errno::Fault));
        assert_eq!(MemObj::create(u64::MAX).err(), Some(Errno::TooBig));
        let m = MemObj::from_cap(Cap::slot(2));
        assert_eq!(m.cap(), Cap(2));
        assert_eq!(m.size(), Err(Errno::Fault));
        let mut buf = [0u8; 4];
        assert_eq!(m.read(0, &mut buf), Err(Errno::Fault));
        assert_eq!(m.read(u64::MAX, &mut buf), Err(Errno::Inval));
        assert_eq!(m.write(0, b"abcd"), Err(Errno::Fault));
        assert_eq!(m.write(u64::MAX, b"abcd"), Err(Errno::Inval));
    }
}
