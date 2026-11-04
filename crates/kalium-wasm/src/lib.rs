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
//! | `code` | the interpreter's compiled form of a module (private; see its docs) |
//! | [`decode`] | the binary decoder |
//! | [`error`] | `Error` (decode/validate/instantiate) and `Trap` (runtime) |
//! | [`validate`] | the validator |
//! | [`exec`] | `Value`, `Execution`, `ExecResult`: the resumable interpreter |
//! | [`instance`] | `Instance`, `Limits`, the `Host` trait, host call types, `Extern`/`ExternType` |
//! | [`snapshot`] | `Snapshot`: the state of an instance's own objects, restorable into a new instance |
//! | [`memory`] | `Memory`: bounds-checked linear memory, shared as `MemoryRef` |
//! | [`table`] | `Table`: bounds-checked tables of references, shared as `TableRef` |
//! | [`global`] | `global::Global`: typed value cells, shared as `GlobalRef` |
//!
//! The commonly used types are re-exported at the crate root.
//!
//! # Linking
//!
//! [`Instance::new`] resolves function imports only (what the kernel needs
//! in M1). [`Instance::instantiate`] resolves every kind of import: the
//! resolver returns an [`Extern`] (a host function with its type, or a
//! shared [`TableRef`], [`MemoryRef`] or [`GlobalRef`] obtained from another
//! instance's [`Instance::export_table`], [`Instance::export_memory`] or
//! [`Instance::export_global`], or created by the host), and the instance
//! checks it against the import type ([`Error::IncompatibleImport`]).
//! Imported objects are *shared*: one memory, table or global cell seen by
//! every importer, with `memory.grow`/`table.grow` visible to all of them.
//! Function references are store-wide *function addresses*
//! ([`Instance::func_addr`]); a `call_indirect` through the address of a
//! function of another instance is routed to the embedder through
//! [`Host::call_foreign`]. See [`instance`] for the design, including why
//! the shared handles are `Rc`/`RefCell` (the kernel is single-threaded, and
//! the types make that invariant compiler-checked).
//!
//! # Fast start
//!
//! A [`Module`] carries its compiled code ([`module::ProgramCache`], filled
//! by [`Module::new`]), so instantiating it again never compiles. The
//! state of an instance's own memory, tables and globals can be captured
//! as a [`Snapshot`] ([`Instance::snapshot`]) and restored into a new
//! instance ([`Instance::from_snapshot`]) without running the module's
//! initialisation again; [`Snapshot::to_bytes`] / [`Snapshot::from_bytes`]
//! let an embedder keep it in a memory object.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

extern crate alloc;

#[cfg(test)]
extern crate std;

mod code;
pub mod decode;
pub mod error;
pub mod exec;
pub mod global;
pub mod instance;
pub mod instr;
pub mod memory;
pub mod module;
pub mod snapshot;
pub mod table;
pub mod types;
pub mod validate;

pub use error::{Error, Trap};
pub use exec::{ExecResult, Execution, Value};
// `global::Global` (the runtime cell) stays behind its module path: the
// root `Global` is the module section's global definition.
pub use global::GlobalRef;
pub use instance::{Extern, ExternType, Host, HostCtx, HostFuncId, HostOutcome, Instance, Limits};
pub use instr::{BlockType, BrTable, ConstExpr, FuncBody, Instr, MemArg};
pub use memory::{Memory, MemoryRef};
pub use module::{
    Data, DataMode, ElemItems, ElemMode, Element, Export, ExportDesc, Global, Import, ImportDesc,
    Module, ProgramCache,
};
pub use snapshot::Snapshot;
pub use table::{Table, TableRef, MAX_TABLE_ELEMENTS};
pub use types::{
    FuncType, GlobalType, MemType, RefType, ResizableLimits, TableType, ValType, MAX_PAGES,
    PAGE_SIZE,
};
