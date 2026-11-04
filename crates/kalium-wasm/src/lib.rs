//! WebAssembly decoder, validator and interpreter for the Kalium kernel.
//!
//! `no_std` + `alloc`, no `unsafe`. See `docs/prototype-plan.md` for the
//! contract this crate fulfils and `docs/architecture.md` for the design.
//!
//! Module map:
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`types`] | static types: `ValType`, `FuncType`, `TableType`, ... |
//! | [`module`] | the decoded module IR (`Module`, `Import`, `Export`, segments) |
//! | [`instr`] | the instruction enum and function bodies (control flow pre-resolved) |
//! | [`decode`] | the binary decoder |
//! | [`error`] | `Error` (decode/validate/instantiate) and `Trap` (runtime) |
//! | [`validate`] | the validator |
//! | [`exec`] | `Value`, `Execution`, `ExecResult`: the resumable interpreter |
//! | [`instance`] | `Instance`, `Limits`, the `Host` trait and host call types |
//! | [`memory`] | `Memory`: bounds-checked linear memory |
//! | [`table`] | `Table`: bounds-checked tables of references |
//!
//! The commonly used types are re-exported at the crate root.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod decode;
pub mod error;
pub mod exec;
pub mod instance;
pub mod instr;
pub mod memory;
pub mod module;
pub mod table;
pub mod types;
pub mod validate;

pub use error::{Error, Trap};
pub use exec::{ExecResult, Execution, Value};
pub use instance::{Host, HostCtx, HostFuncId, HostOutcome, Instance, Limits};
pub use instr::{BlockType, BrTable, ConstExpr, FuncBody, Instr, MemArg};
pub use memory::Memory;
pub use table::{Table, MAX_TABLE_ELEMENTS};
pub use module::{
    Data, DataMode, ElemItems, ElemMode, Element, Export, ExportDesc, Global, Import, ImportDesc,
    Module,
};
pub use types::{
    FuncType, GlobalType, MemType, RefType, ResizableLimits, TableType, ValType, MAX_PAGES,
    PAGE_SIZE,
};
