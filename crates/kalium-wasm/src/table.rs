//! Tables: bounds-checked vectors of references.
//!
//! A table holds `funcref` or `externref` values ([`Value::FuncRef`] /
//! [`Value::ExternRef`]). Every access is bounds-checked and reports
//! [`Trap::TableOutOfBounds`]; the bulk operations check both ranges before
//! writing anything, as the spec requires.
//!
//! A `funcref` stored in a table is a *function address* (see
//! [`crate::Instance::func_addr`]), not a function index: tables are shared
//! between instances through a [`TableRef`] (`Rc<RefCell<Table>>`), and an
//! entry one instance stores must name the same function when another
//! instance calls through it.

use crate::budget::BudgetRef;
use crate::error::Trap;
use crate::exec::Value;
use crate::memory::limits_match;
use crate::types::{RefType, ResizableLimits, TableType};
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

/// A shared handle to a table: what an instance holds for each table in its
/// index space, what it exports and what it imports. The interpreter
/// borrows a table only for the duration of one table instruction, so a
/// host function may borrow it freely.
pub type TableRef = Rc<RefCell<Table>>;

/// Hard cap on the number of elements of any table, whatever the type says.
/// `table.grow` past it returns `-1`; a module declaring a larger minimum
/// fails to instantiate with `Error::Limit`. On top of this per-table cap,
/// [`crate::Instance`] enforces the instance-wide budget over all tables,
/// [`crate::Limits::max_table_elements`].
pub const MAX_TABLE_ELEMENTS: u32 = 1 << 20;

/// A bounds-checked table of references.
///
/// Like a [`Memory`](crate::Memory), a table may be charged to a
/// [`Budget`](crate::Budget) ([`set_budget`](Table::set_budget)): the bytes
/// of its entries count against the budget, a `grow` the budget refuses
/// fails like one the allocator refuses, and the charge is released when
/// the table is dropped.
#[derive(Debug, PartialEq)]
pub struct Table {
    elem: RefType,
    entries: Vec<Value>,
    /// Upper bound in elements (the smaller of the type's max and
    /// [`MAX_TABLE_ELEMENTS`]).
    max: u32,
    /// The `max` of the table's declared type, which import matching sees.
    declared_max: Option<u32>,
    /// The budget the table is charged to, and how many bytes it holds of
    /// it.
    budget: Option<BudgetRef>,
    charged: usize,
}

impl Clone for Table {
    /// A copy of the entries and limits that is charged to no budget (the
    /// original keeps its charge).
    fn clone(&self) -> Table {
        Table {
            elem: self.elem,
            entries: self.entries.clone(),
            max: self.max,
            declared_max: self.declared_max,
            budget: None,
            charged: 0,
        }
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        if let Some(b) = &self.budget {
            b.release(self.charged);
        }
    }
}

/// Bytes an entry of a table costs.
const ENTRY_BYTES: usize = core::mem::size_of::<Value>();

impl Table {
    /// Charges the bytes of the table's entries to `budget`, and every
    /// later growth to it, until the table is dropped (which releases the
    /// charge) or `set_budget` is called again (which moves it). Returns
    /// `false`, leaving the table charged to its previous budget if any,
    /// when the current size does not fit `budget`.
    pub fn set_budget(&mut self, budget: &BudgetRef) -> bool {
        let bytes = self.entries.len().saturating_mul(ENTRY_BYTES);
        if !budget.reserve(bytes) {
            return false;
        }
        if let Some(old) = self.budget.take() {
            old.release(self.charged);
        }
        self.charged = bytes;
        self.budget = Some(budget.clone());
        true
    }

    /// The budget the table is charged to, if any.
    pub fn budget(&self) -> Option<&BudgetRef> {
        self.budget.as_ref()
    }
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
            declared_max: ty.limits.max,
            budget: None,
            charged: 0,
        })
    }

    /// Create a table of type `ty` (capped like [`Table::new`]) holding
    /// `entries`, whose number must lie within the type's limits and
    /// `limit` and which must all be references of the table's element
    /// type. Returns `None` otherwise. The entries are taken over, not
    /// copied.
    pub fn with_entries(ty: TableType, entries: Vec<Value>, limit: u32) -> Option<Table> {
        let max = ty.limits.max.map_or(limit, |m| m.min(limit));
        let len = u32::try_from(entries.len()).ok()?;
        if len < ty.limits.min || len > max {
            return None;
        }
        if entries.iter().any(|v| v.ty() != ty.elem.into()) {
            return None;
        }
        Some(Table {
            elem: ty.elem,
            entries,
            max,
            declared_max: ty.limits.max,
            budget: None,
            charged: 0,
        })
    }

    /// Wrap the table in a shared handle.
    pub fn into_shared(self) -> TableRef {
        Rc::new(RefCell::new(self))
    }

    /// The element type.
    pub fn elem_type(&self) -> RefType {
        self.elem
    }

    /// The table's current type: its size as the minimum and the declared
    /// maximum. This is what an import of the table is matched against.
    pub fn ty(&self) -> TableType {
        TableType {
            elem: self.elem,
            limits: ResizableLimits {
                min: self.size(),
                max: self.declared_max,
            },
        }
    }

    /// Whether this table can be imported as `import` (import matching per
    /// the spec: same element type, limits subtyping).
    pub fn matches(&self, import: &TableType) -> bool {
        self.elem == import.elem && limits_match(&self.ty().limits, &import.limits)
    }

    /// Current size in elements.
    pub fn size(&self) -> u32 {
        self.entries.len() as u32
    }

    /// Maximum size in elements the table can grow to (the runtime cap, not
    /// necessarily the declared maximum).
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
    /// the old size, or `None` if the bound would be exceeded, the budget
    /// refuses the growth or the allocation fails (the caller turns that
    /// into `-1`).
    pub fn grow(&mut self, delta: u32, init: Value) -> Option<u32> {
        let old = self.size();
        let new = old.checked_add(delta)?;
        if new > self.max {
            return None;
        }
        let added = (delta as usize).checked_mul(ENTRY_BYTES)?;
        if self.budget.as_ref().is_some_and(|b| !b.reserve(added)) {
            return None;
        }
        if self.entries.try_reserve_exact(delta as usize).is_err() {
            if let Some(b) = &self.budget {
                b.release(added);
            }
            return None;
        }
        self.entries.resize(new as usize, init);
        if self.budget.is_some() {
            self.charged += added;
        }
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
    fn types_and_import_matching() {
        let mut t = Table::new(ty(2, None), 10).unwrap();
        assert_eq!(t.ty(), ty(2, None));
        assert!(t.matches(&ty(0, None)));
        assert!(t.matches(&ty(2, None)));
        assert!(!t.matches(&ty(3, None)));
        // No declared maximum: the runtime cap is not a type maximum.
        assert!(!t.matches(&ty(0, Some(10))));
        assert!(!t.matches(&TableType {
            elem: RefType::ExternRef,
            limits: ResizableLimits { min: 0, max: None },
        }));
        assert_eq!(t.grow(1, Value::FuncRef(None)), Some(2));
        assert_eq!(t.ty(), ty(3, None));
        let t = Table::new(ty(10, Some(20)), MAX_TABLE_ELEMENTS).unwrap();
        assert!(t.matches(&ty(10, Some(20))));
        assert!(t.matches(&ty(5, Some(25))));
        assert!(!t.matches(&ty(10, Some(19))));
        assert!(!t.matches(&ty(11, Some(20))));
        // Shared handles see each other's changes.
        let a = t.into_shared();
        let b = a.clone();
        a.borrow_mut().set(0, Value::FuncRef(Some(7))).unwrap();
        assert_eq!(b.borrow().get(0), Ok(Value::FuncRef(Some(7))));
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
