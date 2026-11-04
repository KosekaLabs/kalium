//! A byte budget shared by the objects that grow on an instance's behalf.
//!
//! The embedder decides how much of its heap the instances it runs may
//! consume between them; a [`Budget`] is that number and a running count
//! of what has been charged against it. A [`Memory`](crate::Memory) or a
//! [`Table`](crate::Table) given a budget ([`Memory::set_budget`](crate::Memory::set_budget),
//! [`Table::set_budget`](crate::Table::set_budget)) charges its current
//! size at once, every later `memory.grow`/`table.grow` before it
//! allocates (a grow the budget refuses fails exactly like one the
//! allocator refuses: `-1` to the Wasm code, nothing changed), and
//! releases what it charged when it is dropped. The embedder charges the
//! rest of what it allocates for instances (its own copies, its message
//! queues) through [`Budget::reserve`] and [`Budget::release`] directly,
//! so that one counter bounds everything an instance can make the kernel
//! allocate and the heap keeps the headroom the embedder's own bookkeeping
//! needs.
//!
//! The budget is shared through a [`BudgetRef`] (`Rc`), like the memories
//! and tables it accounts for: the embedder is single-threaded (see
//! [`crate::instance`]). A budget created with [`Budget::unlimited`]
//! refuses nothing; an object without a budget is accounted by nobody.

use alloc::rc::Rc;
use core::cell::Cell;

/// A shared handle to a [`Budget`].
pub type BudgetRef = Rc<Budget>;

/// A limit on bytes and the number charged against it so far.
#[derive(Debug, PartialEq, Eq)]
pub struct Budget {
    limit: usize,
    used: Cell<usize>,
}

impl Budget {
    /// A budget of `limit` bytes with nothing charged.
    pub const fn new(limit: usize) -> Budget {
        Budget {
            limit,
            used: Cell::new(0),
        }
    }

    /// A budget that refuses nothing (`usize::MAX`).
    pub const fn unlimited() -> Budget {
        Budget::new(usize::MAX)
    }

    /// Wraps the budget in a shared handle.
    pub fn into_shared(self) -> BudgetRef {
        Rc::new(self)
    }

    /// The limit in bytes.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes charged right now.
    pub fn used(&self) -> usize {
        self.used.get()
    }

    /// Bytes that may still be charged.
    pub fn available(&self) -> usize {
        self.limit.saturating_sub(self.used.get())
    }

    /// Charges `bytes` if they fit; `false` (and nothing charged) if not.
    pub fn reserve(&self, bytes: usize) -> bool {
        match self.used.get().checked_add(bytes) {
            Some(total) if total <= self.limit => {
                self.used.set(total);
                true
            }
            _ => false,
        }
    }

    /// Gives `bytes` back. Saturates at zero: releasing more than was
    /// charged is a bug in the caller, not a reason to wrap.
    pub fn release(&self, bytes: usize) {
        self.used.set(self.used.get().saturating_sub(bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemType, RefType, ResizableLimits, TableType, PAGE_SIZE};
    use crate::{Memory, Table, Value};

    #[test]
    fn reserve_and_release() {
        let b = Budget::new(100);
        assert_eq!(b.limit(), 100);
        assert_eq!(b.available(), 100);
        assert!(b.reserve(60));
        assert!(!b.reserve(41));
        assert_eq!(b.used(), 60);
        assert!(b.reserve(40));
        assert_eq!(b.available(), 0);
        assert!(!b.reserve(1));
        assert!(b.reserve(0));
        b.release(30);
        assert_eq!(b.used(), 70);
        b.release(usize::MAX);
        assert_eq!(b.used(), 0);
        assert!(!b.reserve(usize::MAX));
        assert!(Budget::unlimited().reserve(usize::MAX - 1));
    }

    #[test]
    fn memory_charges_its_size_and_growth_and_releases_on_drop() {
        let budget = Budget::new(3 * PAGE_SIZE).into_shared();
        let mut m = Memory::new(1, 10).unwrap();
        assert!(m.set_budget(&budget));
        assert_eq!(budget.used(), PAGE_SIZE);
        assert_eq!(m.grow(1), Some(1));
        assert_eq!(budget.used(), 2 * PAGE_SIZE);
        // Two more pages do not fit the budget: the grow fails like one
        // the limit refuses, and nothing is charged.
        assert_eq!(m.grow(2), None);
        assert_eq!(m.size_pages(), 2);
        assert_eq!(budget.used(), 2 * PAGE_SIZE);
        assert_eq!(m.grow(1), Some(2));
        assert_eq!(budget.used(), 3 * PAGE_SIZE);
        // A copy is not accounted; the original still is.
        let copy = m.clone();
        assert_eq!(copy.size_pages(), 3);
        assert_eq!(budget.used(), 3 * PAGE_SIZE);
        drop(copy);
        assert_eq!(budget.used(), 3 * PAGE_SIZE);
        drop(m);
        assert_eq!(budget.used(), 0);
        // A memory larger than the whole budget cannot take it.
        let mut big = Memory::new(4, 10).unwrap();
        assert!(!big.set_budget(&budget));
        assert_eq!(budget.used(), 0);
        assert_eq!(big.grow(1), Some(4));
        drop(big);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn memory_moves_between_budgets() {
        let a = Budget::new(PAGE_SIZE).into_shared();
        let b = Budget::new(PAGE_SIZE).into_shared();
        let mut m = Memory::new(1, 2).unwrap();
        assert!(m.set_budget(&a));
        assert!(m.set_budget(&b));
        assert_eq!((a.used(), b.used()), (0, PAGE_SIZE));
        assert_eq!(m.grow(1), None);
        drop(m);
        assert_eq!((a.used(), b.used()), (0, 0));
    }

    #[test]
    fn table_charges_its_entries() {
        let entry = core::mem::size_of::<Value>();
        let budget = Budget::new(10 * entry).into_shared();
        let ty = TableType {
            elem: RefType::FuncRef,
            limits: ResizableLimits { min: 4, max: None },
        };
        let mut t = Table::new(ty, 100).unwrap();
        assert!(t.set_budget(&budget));
        assert_eq!(budget.used(), 4 * entry);
        assert_eq!(t.grow(6, Value::FuncRef(None)), Some(4));
        assert_eq!(budget.used(), 10 * entry);
        assert_eq!(t.grow(1, Value::FuncRef(None)), None);
        assert_eq!(t.size(), 10);
        drop(t);
        assert_eq!(budget.used(), 0);
        let m = Memory::with_type(
            MemType {
                limits: ResizableLimits { min: 0, max: None },
            },
            1,
        )
        .unwrap();
        assert_eq!(m.size_bytes(), 0);
    }
}
