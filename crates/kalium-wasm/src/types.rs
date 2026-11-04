//! Static types of the WebAssembly type system: value types, function
//! types and the types of tables, memories and globals.

use alloc::vec::Vec;
use core::fmt;

/// A value type (`valtype` in the spec).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
    FuncRef,
    ExternRef,
}

impl ValType {
    /// Numeric types (`i32`, `i64`, `f32`, `f64`).
    pub fn is_num(self) -> bool {
        !self.is_ref()
    }

    /// Reference types (`funcref`, `externref`).
    pub fn is_ref(self) -> bool {
        matches!(self, ValType::FuncRef | ValType::ExternRef)
    }

    /// The reference type, if this is one.
    pub fn as_ref_type(self) -> Option<RefType> {
        match self {
            ValType::FuncRef => Some(RefType::FuncRef),
            ValType::ExternRef => Some(RefType::ExternRef),
            _ => None,
        }
    }
}

impl fmt::Display for ValType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ValType::I32 => "i32",
            ValType::I64 => "i64",
            ValType::F32 => "f32",
            ValType::F64 => "f64",
            ValType::FuncRef => "funcref",
            ValType::ExternRef => "externref",
        })
    }
}

/// A reference type (`reftype` in the spec).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefType {
    FuncRef,
    ExternRef,
}

impl From<RefType> for ValType {
    fn from(r: RefType) -> ValType {
        match r {
            RefType::FuncRef => ValType::FuncRef,
            RefType::ExternRef => ValType::ExternRef,
        }
    }
}

/// A function type: parameter and result types (`functype`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

impl FuncType {
    pub fn new(params: Vec<ValType>, results: Vec<ValType>) -> FuncType {
        FuncType { params, results }
    }
}

impl fmt::Display for FuncType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        for (i, p) in self.params.iter().enumerate() {
            if i > 0 {
                write!(f, " ")?;
            }
            write!(f, "{p}")?;
        }
        write!(f, "] -> [")?;
        for (i, r) in self.results.iter().enumerate() {
            if i > 0 {
                write!(f, " ")?;
            }
            write!(f, "{r}")?;
        }
        write!(f, "]")
    }
}

/// Size limits of a table or memory (`limits` in the spec). For memories the
/// unit is 64 KiB pages; for tables it is elements.
///
/// This is the *static* type-level limits of the binary format. The runtime
/// resource limits the kernel imposes on an instance are
/// [`crate::instance::Limits`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResizableLimits {
    pub min: u32,
    pub max: Option<u32>,
}

/// Type of a table: its element reference type and limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TableType {
    pub elem: RefType,
    pub limits: ResizableLimits,
}

/// Type of a linear memory: its limits in pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MemType {
    pub limits: ResizableLimits,
}

/// Size of a linear memory page in bytes.
pub const PAGE_SIZE: usize = 65536;

/// Largest number of pages a 32-bit linear memory can have (4 GiB).
pub const MAX_PAGES: u32 = 65536;

/// Type of a global: its value type and whether it is mutable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GlobalType {
    pub ty: ValType,
    pub mutable: bool,
}
