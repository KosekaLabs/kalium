//! Globals: typed, shareable value cells.
//!
//! A [`Global`] is a value of a fixed type, mutable or not. Instances share
//! globals through a [`GlobalRef`] (`Rc<Global>`): an exported mutable
//! global imported by another instance is the same cell, so a `global.set`
//! in one instance is visible in the other. The value lives in a
//! [`core::cell::Cell`] (`Value` is `Copy`), so neither reads nor writes
//! can fail or conflict; see [`crate::instance`] for why single-threaded
//! shared ownership is the right model in this kernel.

use crate::error::Error;
use crate::exec::Value;
use crate::types::GlobalType;
use alloc::rc::Rc;
use core::cell::Cell;

/// A shared handle to a global: what an instance holds for each global in
/// its index space, what it exports and what it imports.
pub type GlobalRef = Rc<Global>;

/// A global variable: a typed value cell.
#[derive(Debug)]
pub struct Global {
    ty: GlobalType,
    value: Cell<Value>,
}

impl Global {
    /// Create a global of type `ty` holding `value`; `None` if the value
    /// has the wrong type.
    pub fn new(ty: GlobalType, value: Value) -> Option<Global> {
        if value.ty() != ty.ty {
            return None;
        }
        Some(Global { ty, value: Cell::new(value) })
    }

    /// Wrap the global in a shared handle.
    pub fn into_shared(self) -> GlobalRef {
        Rc::new(self)
    }

    /// The global's type (value type and mutability).
    pub fn ty(&self) -> GlobalType {
        self.ty
    }

    /// Whether this global can be imported as `import`: exact type and
    /// mutability.
    pub fn matches(&self, import: &GlobalType) -> bool {
        self.ty == *import
    }

    /// The current value.
    pub fn get(&self) -> Value {
        self.value.get()
    }

    /// Set the value. Fails with [`Error::ArgumentMismatch`] if `value` has
    /// the wrong type. Mutability is *not* checked: this is the host's door,
    /// and the validator already keeps `global.set` away from immutable
    /// globals.
    pub fn set(&self, value: Value) -> Result<(), Error> {
        if value.ty() != self.ty.ty {
            return Err(Error::ArgumentMismatch);
        }
        self.value.set(value);
        Ok(())
    }

    /// The interpreter's `global.set`: the validator has checked the type.
    #[inline(always)]
    pub(crate) fn store(&self, value: Value) {
        self.value.set(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ValType;

    #[test]
    fn typed_cell() {
        let ty = GlobalType { ty: ValType::I32, mutable: true };
        assert!(Global::new(ty, Value::I64(1)).is_none());
        let g = Global::new(ty, Value::I32(1)).unwrap().into_shared();
        let h = g.clone();
        assert_eq!(g.get(), Value::I32(1));
        assert_eq!(g.set(Value::F32(1.0)), Err(Error::ArgumentMismatch));
        g.set(Value::I32(2)).unwrap();
        assert_eq!(h.get(), Value::I32(2));
        assert!(g.matches(&ty));
        assert!(!g.matches(&GlobalType { ty: ValType::I32, mutable: false }));
        assert!(!g.matches(&GlobalType { ty: ValType::I64, mutable: true }));
    }
}
