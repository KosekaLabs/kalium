//! Tables: bounds-checked vectors of references.
//!
//! A table holds `funcref` or `externref` values ([`Value::FuncRef`] /
//! [`Value::ExternRef`]). Every access is bounds-checked and reports
//! [`Trap::TableOutOfBounds`]; the bulk operations check both ranges before
//! writing anything, as the spec requires.

use crate::error::Trap;
use crate::exec::Value;
use crate::types::{RefType, TableType};
use alloc::vec::Vec;

/// Hard cap on the number of elements of any table, whatever the type says.
/// `table.grow` past it returns `-1`; a module declaring a larger minimum
/// fails to instantiate with `Error::Limit`.
pub const MAX_TABLE_ELEMENTS: u32 = 1 << 20;

/// A bounds-checked table of references.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    elem: RefType,
    entries: Vec<Value>,
    /// Upper bound in elements (the smaller of the type's max and
    /// [`MAX_TABLE_ELEMENTS`]).
    max: u32,
}

impl Table {
    /// Create a table of `ty.limits.min` null entries that may grow to the
    /// smaller of `ty.limits.max` and `limit`. Returns `None` if the minimum
    /// exceeds that bound or the allocation fails.
    pub fn new(ty: TableType, limit: u32) -> Option<Table> {
        let max = ty.limits.max.map_or(limit, |m| m.min(limit));
        if ty.limits.min > max {
            return None;
        }
        let mut entries = Vec::new();
        entries.try_reserve_exact(ty.limits.min as usize).ok()?;
        entries.resize(ty.limits.min as usize, Value::null(ty.elem));
        Some(Table {
            elem: ty.elem,
            entries,
            max,
        })
    }

    /// The element type.
    pub fn elem_type(&self) -> RefType {
        self.elem
    }

    /// Current size in elements.
    pub fn size(&self) -> u32 {
        self.entries.len() as u32
    }

    /// Maximum size in elements.
    pub fn max(&self) -> u32 {
        self.max
    }

    /// All entries.
    pub fn entries(&self) -> &[Value] {
        &self.entries
    }

    /// `table.get`.
    pub fn get(&self, idx: u32) -> Result<Value, Trap> {
        self.entries
            .get(idx as usize)
            .copied()
            .ok_or(Trap::TableOutOfBounds)
    }

    /// `table.set`.
    pub fn set(&mut self, idx: u32, value: Value) -> Result<(), Trap> {
        let slot = self
            .entries
            .get_mut(idx as usize)
            .ok_or(Trap::TableOutOfBounds)?;
        *slot = value;
        Ok(())
    }

    /// `table.grow`: grow by `delta` entries initialized to `init`. Returns
    /// the old size, or `None` if the bound would be exceeded or the
    /// allocation fails (the caller turns that into `-1`).
    pub fn grow(&mut self, delta: u32, init: Value) -> Option<u32> {
        let old = self.size();
        let new = old.checked_add(delta)?;
        if new > self.max {
            return None;
        }
        self.entries.try_reserve_exact(delta as usize).ok()?;
        self.entries.resize(new as usize, init);
        Some(old)
    }

    /// The index range `[off, off + len)` if it lies within the table.
    fn range(&self, off: u32, len: u32) -> Result<core::ops::Range<usize>, Trap> {
        let start = off as usize;
        let end = start
            .checked_add(len as usize)
            .ok_or(Trap::TableOutOfBounds)?;
        if end > self.entries.len() {
            return Err(Trap::TableOutOfBounds);
        }
        Ok(start..end)
    }

    /// `table.fill`: set `len` entries starting at `off` to `value`.
    pub fn fill(&mut self, off: u32, value: Value, len: u32) -> Result<(), Trap> {
        let r = self.range(off, len)?;
        self.entries[r].fill(value);
        Ok(())
    }

    /// `table.copy` within one table (overlap allowed). Both ranges are
    /// checked before anything is written.
    pub fn copy_within(&mut self, dst: u32, src: u32, len: u32) -> Result<(), Trap> {
        let s = self.range(src, len)?;
        let _ = self.range(dst, len)?;
        self.entries.copy_within(s, dst as usize);
        Ok(())
    }

    /// `table.init` / `table.copy` from another source: copy
    /// `src[src_off .. src_off + len]` to `dst`. Both ranges are checked
    /// before anything is written.
    pub fn init(&mut self, dst: u32, src: &[Value], src_off: u32, len: u32) -> Result<(), Trap> {
        let s = src_off as usize;
        let s_end = s.checked_add(len as usize).ok_or(Trap::TableOutOfBounds)?;
        if s_end > src.len() {
            return Err(Trap::TableOutOfBounds);
        }
        let r = self.range(dst, len)?;
        self.entries[r].copy_from_slice(&src[s..s_end]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ResizableLimits;

    fn ty(min: u32, max: Option<u32>) -> TableType {
        TableType {
            elem: RefType::FuncRef,
            limits: ResizableLimits { min, max },
        }
    }

    #[test]
    fn new_and_grow() {
        let mut t = Table::new(ty(2, Some(4)), MAX_TABLE_ELEMENTS).unwrap();
        assert_eq!(t.size(), 2);
        assert_eq!(t.max(), 4);
        assert_eq!(t.get(1), Ok(Value::FuncRef(None)));
        assert_eq!(t.get(2), Err(Trap::TableOutOfBounds));
        assert_eq!(t.grow(1, Value::FuncRef(Some(7))), Some(2));
        assert_eq!(t.get(2), Ok(Value::FuncRef(Some(7))));
        assert_eq!(t.grow(2, Value::FuncRef(None)), None);
        assert_eq!(t.grow(u32::MAX, Value::FuncRef(None)), None);
        assert_eq!(t.grow(1, Value::FuncRef(None)), Some(3));
        assert!(Table::new(ty(5, Some(4)), MAX_TABLE_ELEMENTS).is_none());
        assert!(Table::new(ty(MAX_TABLE_ELEMENTS + 1, None), MAX_TABLE_ELEMENTS).is_none());
        // The runtime cap wins over a larger type max.
        let t = Table::new(ty(0, Some(u32::MAX)), 10).unwrap();
        assert_eq!(t.max(), 10);
    }

    #[test]
    fn bulk_ops_check_before_writing() {
        let mut t = Table::new(ty(4, None), MAX_TABLE_ELEMENTS).unwrap();
        let f = |i| Value::FuncRef(Some(i));
        t.set(0, f(0)).unwrap();
        assert_eq!(t.set(4, f(0)), Err(Trap::TableOutOfBounds));
        assert_eq!(t.fill(2, f(9), 3), Err(Trap::TableOutOfBounds));
        assert_eq!(t.get(2), Ok(Value::FuncRef(None)));
        t.fill(2, f(9), 2).unwrap();
        assert_eq!(t.entries(), &[f(0), Value::FuncRef(None), f(9), f(9)]);
        assert_eq!(t.copy_within(3, 0, 2), Err(Trap::TableOutOfBounds));
        assert_eq!(t.copy_within(0, 3, 2), Err(Trap::TableOutOfBounds));
        t.copy_within(1, 0, 3).unwrap();
        assert_eq!(t.entries(), &[f(0), f(0), Value::FuncRef(None), f(9)]);
        let seg = [f(1), f(2), f(3)];
        assert_eq!(t.init(0, &seg, 1, 3), Err(Trap::TableOutOfBounds));
        assert_eq!(t.init(2, &seg, 0, 3), Err(Trap::TableOutOfBounds));
        assert_eq!(t.entries()[0], f(0));
        t.init(1, &seg, 1, 2).unwrap();
        assert_eq!(t.entries(), &[f(0), f(2), f(3), f(9)]);
        assert!(t.init(4, &seg, 3, 0).is_ok());
        assert_eq!(t.init(5, &seg, 3, 0), Err(Trap::TableOutOfBounds));
        assert_eq!(t.init(0, &seg, 4, 0), Err(Trap::TableOutOfBounds));
    }
}
