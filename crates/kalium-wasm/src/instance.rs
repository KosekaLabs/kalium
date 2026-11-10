//! Instances, instantiation limits and the host interface.
//!
//! [`Instance::instantiate`] resolves every import through the embedder's
//! resolver, checks the imported objects against the import types (import
//! matching per the spec: exact function types, limits subtyping for
//! tables and memories, exact type and mutability for globals), builds the
//! defined memory, tables and globals, evaluates the element segments and
//! applies the active element and data segments in the order the spec
//! prescribes (elements first, then data, each with bounds checks that fail
//! instantiation with [`Error::Trap`], earlier segments staying applied).
//! The two halves are available separately as [`Instance::allocate`] and
//! [`Instance::init_segments`], for embedders that must keep an instance
//! whose segments trapped (its functions may already sit in shared
//! tables). The start function is only recorded; the kernel runs it
//! through [`crate::exec::Execution::start`]. [`Instance::new`] is the
//! functions-only front end the kernel uses.
//!
//! # Shared objects
//!
//! Memories, tables and globals are *shared objects*: an instance holds a
//! [`MemoryRef`], [`TableRef`] or [`GlobalRef`] for each one in its index
//! spaces, whether it defined or imported it, and exporting one hands out a
//! clone of the handle. Two instances that import the same object therefore
//! see one memory, one table or one cell: a `memory.grow` or `table.grow`
//! through either is visible to both, and the interpreter re-reads the
//! object's size whenever it starts running, after every host call and
//! after every grow, so a grow done by another instance between two `run`
//! calls (the kernel runs instances interleaved) is always seen.
//!
//! The handles are `Rc<RefCell<_>>` (and `Rc<Global>` with a `Cell`
//! inside), not atomics or locks, because Kalium's kernel is
//! single-threaded: it runs on core 0 only (the HAL parks every other core
//! at boot), the scheduler runs one `Execution` at a time and host calls
//! run on the same stack, nested inside the interpreter. `Rc` and `RefCell`
//! make that invariant explicit and compiler-checked: an [`Instance`] is
//! neither `Send` nor `Sync`, so it can never be handed to another core by
//! accident, and a borrow conflict (a host holding a memory borrow while
//! running an instance that shares it) is reported as [`Trap::Host`], never
//! as a data race. Should the kernel ever go SMP, these aliases are the one
//! place to change (`Arc<Mutex<_>>`), and the borrow discipline below
//! becomes the locking discipline.
//!
//! Borrow discipline: the interpreter borrows the memory mutably for the
//! duration of an interpreter loop and *releases it around every host
//! call*, so a host function may borrow the memory through
//! [`HostCtx::memory`] and may run another instance that shares it (the
//! spec test runner does exactly that for cross-module calls). Tables are
//! borrowed for one instruction at a time; globals need no borrow at all.
//!
//! # Function addresses
//!
//! A `funcref` ([`crate::Value::FuncRef`]) holds a *function address*, not
//! a function index: tables are shared, and an entry one instance stores
//! must still name the same function when another instance calls through
//! it. Every instance owns a range of addresses, `func_base ..
//! func_base + num_funcs`, chosen by the embedder at instantiation, and
//! function `f` of the instance has address `func_base + f`
//! ([`Instance::func_addr`]); an imported function keeps the address of
//! the function it resolves to ([`Extern::Func::addr`]), so that `ref.func`
//! of an import and `ref.func` in the exporting instance agree. The
//! functions-only [`Instance::new`] uses `func_base = 0`, so for a kernel
//! instance, which shares no tables, addresses and indices coincide.
//!
//! `call_indirect` through an address of the calling instance is a direct
//! call; through the address of an imported function it is the import's
//! host call; through any other address (a function another instance
//! stored in a shared table) it is a *foreign call*, routed to the embedder
//! through [`Host::call_foreign`], which knows every instance of the store.
//!
//! # Snapshots
//!
//! [`Instance::snapshot`] captures the instance's own memory, tables and
//! globals (and which segments it has dropped) as a [`Snapshot`], and
//! [`Instance::from_snapshot`] allocates a new instance of the module with
//! that state instead of applying the segments: the fast way to start an
//! instance whose initialisation has been run once. A snapshot can only be
//! taken of objects nobody else can see: the module must import no table,
//! memory or global (those belong to their exporter), and every handle the
//! instance holds must be the only one, `Rc::strong_count == 1`, so none
//! has been exported to a live instance or kept by the host (either is
//! [`Error::Unsupported`]: a memory another instance can write to has no
//! consistent contents to capture). The interpreter releases its handle
//! clones at the end of every `run`, so a snapshot between two runs of an
//! execution, after a host call returned `Block` for instance, sees the
//! memory as that execution left it. See [`crate::snapshot`].

use crate::code::Program;
use crate::error::{Error, Trap};
use crate::exec::Value;
use crate::global::{Global, GlobalRef};
use crate::instr::{ConstExpr, Instr};
use crate::memory::{Memory, MemoryRef};
use crate::module::{DataMode, ElemItems, ElemMode, ImportDesc, Module};
use crate::snapshot::Snapshot;
use crate::table::{MAX_TABLE_ELEMENTS, Table, TableRef};
use crate::types::{FuncType, GlobalType, MemType, PAGE_SIZE, TableType};
use alloc::rc::Rc;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{Ref, RefMut};
use core::fmt;

/// Identifies a host function to the kernel's `Host` implementation. The
/// kernel hands these out when resolving imports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostFuncId(pub u32);

/// What a host call did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOutcome {
    /// The call completed; `results` has been filled in.
    Continue,
    /// The call cannot complete yet. The interpreter leaves the arguments on
    /// the stack and rewinds to the call instruction so that resuming the
    /// execution re-issues the call.
    Block,
    /// The instance asked to terminate with this exit code.
    Exit(i32),
}

/// What a host function may touch: the calling instance's linear memory.
///
/// The memory is handed out as its shared handle, not borrowed: the
/// interpreter has released its own borrow for the duration of the call, so
/// `ctx.memory.borrow_mut()` succeeds (unless the host itself still holds a
/// borrow), the host may grow the memory, and it may run another instance
/// that imports the same memory.
#[derive(Debug)]
pub struct HostCtx<'a> {
    pub memory: &'a MemoryRef,
}

/// The kernel's side of a host call.
pub trait Host {
    /// Called with the arguments of an imported function. On `Continue`,
    /// `results` (sized to the function's result count and pre-filled with
    /// zero values of the right types) must be filled; a result of the wrong
    /// type is reported to the instance as [`Trap::Host`].
    fn call(
        &mut self,
        ctx: &mut HostCtx<'_>,
        func: HostFuncId,
        args: &[Value],
        results: &mut [Value],
    ) -> Result<HostOutcome, Trap>;

    /// Called for a `call_indirect` through a function address that belongs
    /// to neither the calling instance nor one of its imports: a function
    /// another instance stored in a table the caller shares (see the
    /// module documentation). `ty` is the type `call_indirect` expects;
    /// `args` and `results` are sized accordingly. The host must check that
    /// the function at `addr` has exactly that type (else return
    /// [`Trap::IndirectCallTypeMismatch`]) and run it with the same
    /// `Continue`/`Block`/`Exit` contract as [`Host::call`].
    ///
    /// The default refuses with [`Trap::Host`]: an embedder whose instances
    /// share no tables (the kernel in M1) never sees such a call.
    fn call_foreign(
        &mut self,
        ctx: &mut HostCtx<'_>,
        addr: u32,
        ty: &FuncType,
        args: &[Value],
        results: &mut [Value],
    ) -> Result<HostOutcome, Trap> {
        let _ = (ctx, addr, ty, args, results);
        Err(Trap::Host)
    }
}

/// The type of an import, as the resolver sees it (`externtype` in the
/// spec).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternType<'a> {
    Func(&'a FuncType),
    Table(TableType),
    Memory(MemType),
    Global(GlobalType),
}

/// What an import resolves to. The instance checks it against the import's
/// type ([`Error::IncompatibleImport`] on a mismatch of kind or type).
#[derive(Clone, Debug)]
pub enum Extern {
    /// A function the instance calls through [`Host::call`] with `id`.
    Func {
        id: HostFuncId,
        /// Its type; must equal the import's type exactly.
        ty: FuncType,
        /// Its function address ([`Instance::func_addr`] in the exporting
        /// instance) when it is a function of another instance, so that
        /// `ref.func` of the import names the same function; `None` for a
        /// host function, which takes the address of its import slot in
        /// the importing instance. An address inside the importing
        /// instance's own range (`func_base..func_base + num_funcs`) would
        /// alias one of its functions and is rejected with
        /// [`Error::IncompatibleImport`]; see [`Instance::allocate`].
        addr: Option<u32>,
    },
    Table(TableRef),
    Memory(MemoryRef),
    Global(GlobalRef),
}

/// Resource limits imposed on an instance by the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Hard cap on linear memory, in 64 KiB pages. Applies to the memory
    /// the instance defines; an imported memory keeps the cap of the
    /// instance that created it.
    pub max_memory_pages: u32,
    /// Hard cap on the depth of the value stack (locals included).
    pub max_stack_values: usize,
    /// Hard cap on the depth of the call stack.
    pub max_call_depth: usize,
    /// Hard cap on the number of table elements of the instance, summed over
    /// all its tables, imported ones included (each element is a 16-byte
    /// [`Value`]). A module whose declared table minimums add up to more
    /// fails to instantiate with [`Error::Limit`]; `table.grow` past it
    /// returns `-1`.
    pub max_table_elements: u32,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_memory_pages: 256, max_stack_values: 64 * 1024, max_call_depth: 1024, max_table_elements: 65536 }
    }
}

/// An instantiated module: its memory, tables, globals and resolved
/// imports. Executions ([`crate::exec::Execution`]) borrow it while they
/// run; several may exist for one instance.
///
/// Memory, tables and globals are shared handles (see the module
/// documentation); the instance is therefore neither `Send` nor `Sync`.
pub struct Instance {
    module: Arc<Module>,
    limits: Limits,
    /// Memory 0 (imported or defined), or an empty, non-growable memory if
    /// the module has none.
    pub(crate) memory: MemoryRef,
    /// Tables, imported ones first.
    pub(crate) tables: Vec<TableRef>,
    /// Globals, imported ones first.
    pub(crate) globals: Vec<GlobalRef>,
    /// Host function id of each imported function, by function index.
    host_funcs: Vec<HostFuncId>,
    /// Function address of each imported function, by function index.
    import_addrs: Vec<u32>,
    /// First address of this instance's function address range (see the
    /// module documentation).
    pub(crate) func_base: u32,
    /// Type index of every function (imports first), by function index.
    func_types: Vec<u32>,
    /// The compiled code of the module (see [`crate::code`]): the module's
    /// cached program, shared by every instance of the module, and held so
    /// that the interpreter loop can keep it while it mutates the instance.
    program: Arc<Program>,
    /// Evaluated element segments; emptied when dropped (or after an active
    /// segment has been applied).
    pub(crate) elems: Vec<Vec<Value>>,
    /// Which data segments have been dropped (or applied, for active ones).
    pub(crate) data_dropped: Vec<bool>,
    /// Whether [`Instance::init_segments`] has run.
    initialized: bool,
}

impl fmt::Debug for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instance")
            .field("memory_pages", &self.memory.try_borrow().map(|m| m.size_pages()).ok())
            .field("tables", &self.tables.len())
            .field("globals", &self.globals.len())
            .field("imported_funcs", &self.host_funcs.len())
            .field("func_base", &self.func_base)
            .finish_non_exhaustive()
    }
}

fn invalid(msg: &'static str) -> Error {
    Error::Invalid { msg, func: None, instr: None }
}

/// A shared object is mutably borrowed by the host while the instance
/// needs it (see the module documentation).
fn busy() -> Error {
    Error::Trap(Trap::Host)
}

fn unsupported(msg: &'static str) -> Error {
    Error::Unsupported { offset: 0, msg }
}

fn bad_snapshot(msg: &'static str) -> Error {
    Error::Snapshot { msg }
}

/// Whether `module` imports a table, memory or global: objects that belong
/// to their exporter and so are neither captured nor restored.
fn imports_objects(module: &Module) -> bool {
    module.imports.iter().any(|i| !matches!(i.desc, ImportDesc::Func(_)))
}

/// The function address space of an instance under construction.
struct Addrs<'a> {
    base: u32,
    import_addrs: &'a [u32],
    num_funcs: usize,
}

impl Addrs<'_> {
    /// A snapshot value as the instance holds it: a function reference by
    /// address instead of function index (see [`crate::snapshot`]).
    fn relocate(&self, v: Value) -> Result<Value, Error> {
        match v {
            Value::FuncRef(Some(f)) => self
                .addr(f)
                .map(|a| Value::FuncRef(Some(a)))
                .ok_or_else(|| bad_snapshot("function reference names no function of the module")),
            other => Ok(other),
        }
    }

    fn addr(&self, f: u32) -> Option<u32> {
        if let Some(&a) = self.import_addrs.get(f as usize) {
            return Some(a);
        }
        if (f as usize) < self.num_funcs {
            // The range was checked not to overflow at instantiation.
            return self.base.checked_add(f);
        }
        None
    }
}

/// Evaluate a constant expression (global initializer, segment offset or
/// element item) against the globals initialized so far.
fn eval_const(addrs: &Addrs<'_>, globals: &[GlobalRef], expr: &ConstExpr) -> Result<Value, Error> {
    let instr = expr.single().ok_or_else(|| invalid("constant expression must be a single instruction"))?;
    Ok(match instr {
        Instr::I32Const(v) => Value::I32(v),
        Instr::I64Const(v) => Value::I64(v),
        Instr::F32Const(bits) => Value::F32(f32::from_bits(bits)),
        Instr::F64Const(bits) => Value::F64(f64::from_bits(bits)),
        Instr::RefNull(t) => Value::null(t),
        Instr::RefFunc(f) => {
            Value::FuncRef(Some(addrs.addr(f).ok_or_else(|| invalid("ref.func in constant expression: unknown function"))?))
        }
        Instr::GlobalGet(g) => globals
            .get(g as usize)
            .map(|g| g.get())
            .ok_or_else(|| invalid("global.get in constant expression: unknown global"))?,
        _ => return Err(invalid("non-constant instruction in constant expression")),
    })
}

impl Instance {
    /// Instantiate `module`, resolving each function import through
    /// `resolve(module_name, field_name, type)`; `Err(())` from `resolve` is
    /// a link error (`Error::Link`), as is any table, memory or global
    /// import (this front end resolves functions only; see
    /// [`Instance::instantiate`] for the general one). The function address
    /// range starts at 0, so function addresses equal function indices.
    /// Active segments are applied here; an out-of-bounds segment fails
    /// with `Error::Trap`. The start function is *not* run; see
    /// `Execution::start`.
    pub fn new(
        module: Arc<Module>,
        resolve: &mut dyn FnMut(&str, &str, &FuncType) -> Result<HostFuncId, ()>,
        limits: &Limits,
    ) -> Result<Instance, Error> {
        Instance::instantiate(
            module,
            &mut |m, n, ty| match *ty {
                ExternType::Func(ft) => resolve(m, n, ft).ok().map(|id| Extern::Func { id, ty: ft.clone(), addr: None }),
                _ => None,
            },
            limits,
            0,
        )
    }

    /// Instantiate `module`: [`Instance::allocate`] followed by
    /// [`Instance::init_segments`], a trap in the latter reported as
    /// `Error::Trap` (the half-initialized instance is dropped; an embedder
    /// that must keep it, because its functions already sit in shared
    /// tables, calls the two steps itself). The start function is *not*
    /// run; see `Execution::start`.
    pub fn instantiate(
        module: Arc<Module>,
        resolve: &mut dyn FnMut(&str, &str, &ExternType<'_>) -> Option<Extern>,
        limits: &Limits,
        func_base: u32,
    ) -> Result<Instance, Error> {
        let mut inst = Instance::allocate(module, resolve, limits, func_base)?;
        inst.init_segments().map_err(Error::Trap)?;
        Ok(inst)
    }

    /// Link and allocate an instance of `module` without applying its
    /// active segments (the first half of instantiation; see
    /// [`Instance::init_segments`] for the second).
    ///
    /// Each import is resolved through `resolve(module_name, field_name,
    /// type)`: `None` is a link error ([`Error::Link`], "unknown import");
    /// an [`Extern`] of another kind or of a type that does not match the
    /// import (see [`crate::Table::matches`], [`crate::Memory::matches`],
    /// [`crate::global::Global::matches`]; function types must be equal)
    /// fails with [`Error::IncompatibleImport`]. Imports are resolved in
    /// module order, so the resolver may observe the order.
    ///
    /// `func_base` is the first function address of the instance (see the
    /// module documentation); the embedder keeps the ranges of instances
    /// that may share tables disjoint. `Error::Limit` if the range would
    /// overflow `u32`. A function import whose [`Extern::Func::addr`] lies
    /// inside that range is refused with [`Error::IncompatibleImport`]: the
    /// address would be indistinguishable from one of the instance's own
    /// functions, and a `call_indirect` through it would run that function
    /// instead of the import.
    pub fn allocate(
        module: Arc<Module>,
        resolve: &mut dyn FnMut(&str, &str, &ExternType<'_>) -> Option<Extern>,
        limits: &Limits,
        func_base: u32,
    ) -> Result<Instance, Error> {
        Instance::build(module, resolve, limits, func_base, None)
    }

    /// Allocate an instance of `module` whose memory, tables and globals
    /// hold the state captured in `snapshot` (see [`Instance::snapshot`]
    /// and [`crate::snapshot`]) instead of their initial values, with the
    /// segments the snapshot recorded as dropped already dropped, so that
    /// no segment is applied and [`Instance::init_segments`] does nothing.
    /// Imports are resolved exactly as in [`Instance::allocate`] (same
    /// errors), and `func_base` may differ from the snapshotted instance's:
    /// function references are relocated.
    ///
    /// The snapshot is checked against the module and the limits before
    /// anything is built; a mismatch is [`Error::Snapshot`]: the memory's
    /// page count must lie within the module's memory limits and
    /// [`Limits::max_memory_pages`] (and be zero for a module without a
    /// memory), the globals must match the module's in number and type,
    /// each table's length must lie within its type's limits, the lengths
    /// together within [`Limits::max_table_elements`], every table entry
    /// must be a reference of the table's element type naming a function
    /// of the module, and the segment flags must match the module's
    /// segment counts. A module that imports a table, memory or global is
    /// [`Error::Unsupported`], as for [`Instance::snapshot`].
    pub fn from_snapshot(
        module: Arc<Module>,
        snapshot: &Snapshot,
        resolve: &mut dyn FnMut(&str, &str, &ExternType<'_>) -> Option<Extern>,
        limits: &Limits,
        func_base: u32,
    ) -> Result<Instance, Error> {
        if imports_objects(&module) {
            return Err(unsupported("restoring a snapshot into a module that imports a table, memory or global"));
        }
        Instance::build(module, resolve, limits, func_base, Some(snapshot))
    }

    /// [`Instance::allocate`], with the objects' contents taken from
    /// `snapshot` when one is given (see [`Instance::from_snapshot`]).
    fn build(
        module: Arc<Module>,
        resolve: &mut dyn FnMut(&str, &str, &ExternType<'_>) -> Option<Extern>,
        limits: &Limits,
        func_base: u32,
        snapshot: Option<&Snapshot>,
    ) -> Result<Instance, Error> {
        if module.funcs.len() != module.code.len() {
            return Err(invalid("function and code section lengths differ"));
        }
        let num_funcs = module.num_funcs();
        u32::try_from(num_funcs)
            .ok()
            .and_then(|n| func_base.checked_add(n))
            .ok_or(Error::Limit { msg: "function address range overflows" })?;

        // Imports, in order; each kind goes to the front of its index space.
        let mut host_funcs = Vec::new();
        let mut import_addrs = Vec::new();
        let mut func_types = Vec::new();
        let mut tables: Vec<TableRef> = Vec::new();
        let mut memories: Vec<MemoryRef> = Vec::new();
        let mut globals: Vec<GlobalRef> = Vec::new();
        for (i, imp) in module.imports.iter().enumerate() {
            let import = i as u32;
            let ety = match &imp.desc {
                ImportDesc::Func(ty) => {
                    ExternType::Func(module.get_type(*ty).ok_or_else(|| invalid("import type index out of range"))?)
                }
                ImportDesc::Table(t) => ExternType::Table(*t),
                ImportDesc::Memory(m) => ExternType::Memory(*m),
                ImportDesc::Global(g) => ExternType::Global(*g),
            };
            let ext = resolve(&imp.module, &imp.name, &ety).ok_or(Error::Link { import })?;
            let incompatible = Error::IncompatibleImport { import };
            match (ety, ext) {
                (ExternType::Func(expected), Extern::Func { id, ty: actual, addr }) => {
                    if actual != *expected {
                        return Err(incompatible);
                    }
                    let ImportDesc::Func(ty) = imp.desc else {
                        return Err(invalid("import kind mismatch"));
                    };
                    // The slot's own address (within the checked range).
                    let slot = func_base.wrapping_add(host_funcs.len() as u32);
                    let addr = match addr {
                        None => slot,
                        // Another instance's function: its address must not
                        // fall into this instance's range, where
                        // `call_indirect` and `local_func` would take it for
                        // a function of this instance (the range does not
                        // overflow, see above, so the subtraction is the
                        // in-range test).
                        Some(a) if (a.wrapping_sub(func_base) as usize) < num_funcs => {
                            return Err(incompatible);
                        }
                        Some(a) => a,
                    };
                    import_addrs.push(addr);
                    host_funcs.push(id);
                    func_types.push(ty);
                }
                (ExternType::Table(t), Extern::Table(table)) => {
                    if !table.try_borrow().map_err(|_| busy())?.matches(&t) {
                        return Err(incompatible);
                    }
                    tables.push(table);
                }
                (ExternType::Memory(m), Extern::Memory(memory)) => {
                    if !memory.try_borrow().map_err(|_| busy())?.matches(&m) {
                        return Err(incompatible);
                    }
                    memories.push(memory);
                }
                (ExternType::Global(g), Extern::Global(global)) => {
                    if !global.matches(&g) {
                        return Err(incompatible);
                    }
                    globals.push(global);
                }
                _ => return Err(incompatible),
            }
        }
        for &ty in &module.funcs {
            if module.get_type(ty).is_none() {
                return Err(invalid("function type index out of range"));
            }
            func_types.push(ty);
        }
        let addrs = Addrs { base: func_base, import_addrs: &import_addrs, num_funcs };

        // Defined globals, in order; initializers may read earlier globals.
        // From a snapshot, the captured values (which the snapshot of a
        // module without imported globals covers completely).
        match snapshot {
            None => {
                for g in &module.globals {
                    let v = eval_const(&addrs, &globals, &g.init)?;
                    let global = Global::new(g.ty, v).ok_or_else(|| invalid("global initializer has the wrong type"))?;
                    globals.push(global.into_shared());
                }
            }
            Some(snap) => {
                if snap.globals.len() != module.globals.len() || !globals.is_empty() {
                    return Err(bad_snapshot("global count does not match the module"));
                }
                for (g, &v) in module.globals.iter().zip(&snap.globals) {
                    let global =
                        Global::new(g.ty, addrs.relocate(v)?).ok_or_else(|| bad_snapshot("global value has the wrong type"))?;
                    globals.push(global.into_shared());
                }
            }
        }

        // Defined tables. Each one is bounded by the smaller of the
        // per-table cap and the instance's element budget, and the declared
        // minimums together must fit the budget before anything is
        // allocated.
        let per_table = MAX_TABLE_ELEMENTS.min(limits.max_table_elements);
        let mut declared: u32 = 0;
        for t in &module.tables {
            declared = declared
                .checked_add(t.limits.min)
                .filter(|&n| n <= limits.max_table_elements)
                .ok_or(Error::Limit { msg: "declared table sizes exceed the instance's table element limit" })?;
        }
        match snapshot {
            None => {
                for t in &module.tables {
                    let table = Table::new(*t, per_table).ok_or(Error::Limit { msg: "table size exceeds limit" })?;
                    tables.push(table.into_shared());
                }
            }
            Some(snap) => {
                if snap.tables.len() != module.tables.len() || !tables.is_empty() {
                    return Err(bad_snapshot("table count does not match the module"));
                }
                let mut total: u32 = 0;
                for (t, entries) in module.tables.iter().zip(&snap.tables) {
                    total = u32::try_from(entries.len())
                        .ok()
                        .and_then(|n| total.checked_add(n))
                        .filter(|&n| n <= limits.max_table_elements)
                        .ok_or_else(|| bad_snapshot("table sizes exceed the instance's table element limit"))?;
                    let mut relocated = Vec::new();
                    relocated.try_reserve_exact(entries.len()).map_err(|_| Error::OutOfMemory)?;
                    for &v in entries {
                        relocated.push(addrs.relocate(v)?);
                    }
                    let table = Table::with_entries(*t, relocated, per_table)
                        .ok_or_else(|| bad_snapshot("table length or entry type does not match the module"))?;
                    tables.push(table.into_shared());
                }
            }
        }

        // Memory 0 (an empty, non-growable one when the module has none).
        if memories.len() + module.memories.len() > 1 {
            return Err(Error::Unsupported { offset: 0, msg: "multiple memories are not supported" });
        }
        let memory = match (memories.pop(), module.memories.first(), snapshot) {
            (Some(imported), _, None) => imported,
            (Some(_), _, Some(_)) => {
                return Err(bad_snapshot("the module imports its memory"));
            }
            (None, Some(m), None) => {
                let cap = m.limits.max.map_or(limits.max_memory_pages, |x| x.min(limits.max_memory_pages));
                if m.limits.min > cap {
                    return Err(Error::Limit { msg: "memory size exceeds limit" });
                }
                Memory::with_type(*m, limits.max_memory_pages).ok_or(Error::OutOfMemory)?.into_shared()
            }
            (None, Some(m), Some(snap)) => {
                let cap = m.limits.max.map_or(limits.max_memory_pages, |x| x.min(limits.max_memory_pages));
                let pages = snap.memory_pages();
                if !snap.memory.len().is_multiple_of(PAGE_SIZE) {
                    return Err(bad_snapshot("memory contents are not whole pages"));
                }
                if pages < m.limits.min {
                    return Err(bad_snapshot("memory is smaller than the module's minimum"));
                }
                if pages > cap {
                    return Err(bad_snapshot("memory exceeds the module's maximum or the limit"));
                }
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(snap.memory.len()).map_err(|_| Error::OutOfMemory)?;
                bytes.extend_from_slice(&snap.memory);
                Memory::with_contents(*m, bytes, limits.max_memory_pages)
                    .ok_or_else(|| bad_snapshot("memory does not fit the module's type"))?
                    .into_shared()
            }
            (None, None, Some(snap)) if !snap.memory.is_empty() => {
                return Err(bad_snapshot("memory contents for a module without a memory"));
            }
            (None, None, _) => Memory::new(0, 0).ok_or(Error::OutOfMemory)?.into_shared(),
        };

        // Element segments are evaluated eagerly into element instances.
        let mut elems = Vec::with_capacity(module.elements.len());
        for e in &module.elements {
            let items: Vec<Value> = match &e.items {
                ElemItems::Funcs(fs) => {
                    let mut v = Vec::with_capacity(fs.len());
                    for &f in fs {
                        let a = addrs.addr(f).ok_or_else(|| invalid("element segment: unknown function"))?;
                        v.push(Value::FuncRef(Some(a)));
                    }
                    v
                }
                ElemItems::Exprs(exprs) => {
                    let mut v = Vec::with_capacity(exprs.len());
                    for ex in exprs {
                        let val = eval_const(&addrs, &globals, ex)?;
                        if val.ty() != e.ty.into() {
                            return Err(invalid("element item has the wrong type"));
                        }
                        v.push(val);
                    }
                    v
                }
            };
            elems.push(items);
        }

        // The segments' drop state: from the snapshot, which also means
        // the active segments have been applied, so `init_segments` has
        // nothing left to do.
        let mut data_dropped = alloc::vec![false; module.data.len()];
        if let Some(snap) = snapshot {
            if snap.elem_dropped.len() != elems.len() {
                return Err(bad_snapshot("element segment count does not match the module"));
            }
            if snap.data_dropped.len() != data_dropped.len() {
                return Err(bad_snapshot("data segment count does not match the module"));
            }
            for (seg, &dropped) in elems.iter_mut().zip(&snap.elem_dropped) {
                if dropped {
                    *seg = Vec::new();
                }
            }
            data_dropped.copy_from_slice(&snap.data_dropped);
        }

        let program = module.program();

        Ok(Instance {
            limits: *limits,
            memory,
            tables,
            globals,
            host_funcs,
            import_addrs,
            func_base,
            program,
            func_types,
            elems,
            data_dropped,
            initialized: snapshot.is_some(),
            module,
        })
    }

    /// Capture the instance's own memory, tables and globals, and which
    /// data and element segments it has dropped, as a [`Snapshot`] that
    /// [`Instance::from_snapshot`] restores into a new instance of the
    /// module. Nothing about an execution in flight is captured.
    ///
    /// Allowed exactly when every object is the instance's alone:
    ///
    /// - the module imports no table, memory or global (an imported object
    ///   belongs to its exporter), and
    /// - every handle the instance holds (its memory, each table, each
    ///   global) has `Rc::strong_count == 1`: it has not been exported to
    ///   a live instance through [`Instance::export_memory`] and the like,
    ///   nor kept by the host.
    ///
    /// Otherwise [`Error::Unsupported`] (with `offset` 0): an object
    /// another instance may write to has no consistent contents to
    /// capture. A table holding a reference to a function of another
    /// instance (a foreign address) is also [`Error::Unsupported`], and a
    /// memory or table the host has mutably borrowed is
    /// [`Error::Trap`]`(`[`Trap::Host`]`)`.
    ///
    /// Every copy the snapshot makes (the memory, the globals, each
    /// table's entries, the segment flags) is allocated fallibly: a short
    /// heap is [`Error::OutOfMemory`], never an allocation failure. The
    /// sizes are the instance's to choose (a table may hold the element
    /// limit, 1 MiB of entries), and the embedder charges the snapshot to
    /// its budget only once it exists.
    pub fn snapshot(&self) -> Result<Snapshot, Error> {
        if imports_objects(&self.module) {
            return Err(unsupported("snapshot of an instance that imports a table, memory or global"));
        }
        let shared = Rc::strong_count(&self.memory) != 1
            || self.tables.iter().any(|t| Rc::strong_count(t) != 1)
            || self.globals.iter().any(|g| Rc::strong_count(g) != 1);
        if shared {
            return Err(unsupported("snapshot of an instance whose memory, table or global is shared"));
        }
        let memory = self.memory.try_borrow().map_err(|_| busy())?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(memory.size_bytes()).map_err(|_| Error::OutOfMemory)?;
        bytes.extend_from_slice(memory.as_slice());
        drop(memory);
        let mut globals = Vec::new();
        globals.try_reserve_exact(self.globals.len()).map_err(|_| Error::OutOfMemory)?;
        for g in &self.globals {
            globals.push(self.unrelocate(g.get())?);
        }
        let mut tables = Vec::new();
        tables.try_reserve_exact(self.tables.len()).map_err(|_| Error::OutOfMemory)?;
        for t in &self.tables {
            let t = t.try_borrow().map_err(|_| busy())?;
            let mut entries = Vec::new();
            entries.try_reserve_exact(t.entries().len()).map_err(|_| Error::OutOfMemory)?;
            for &v in t.entries() {
                entries.push(self.unrelocate(v)?);
            }
            tables.push(entries);
        }
        let mut data_dropped = Vec::new();
        data_dropped.try_reserve_exact(self.data_dropped.len()).map_err(|_| Error::OutOfMemory)?;
        data_dropped.extend_from_slice(&self.data_dropped);
        let mut elem_dropped = Vec::new();
        elem_dropped.try_reserve_exact(self.elems.len()).map_err(|_| Error::OutOfMemory)?;
        elem_dropped.extend(self.elems.iter().map(Vec::is_empty));
        Ok(Snapshot { memory: bytes, globals, tables, data_dropped, elem_dropped })
    }

    /// A value as the snapshot stores it: a function reference by function
    /// index instead of address (see [`crate::snapshot`]).
    fn unrelocate(&self, v: Value) -> Result<Value, Error> {
        match v {
            Value::FuncRef(Some(addr)) => self
                .local_func(addr)
                .map(|f| Value::FuncRef(Some(f)))
                .ok_or_else(|| unsupported("snapshot of a reference to another instance's function")),
            other => Ok(other),
        }
    }

    /// Apply the active element and data segments (the second half of
    /// instantiation): every active element segment as `table.init` then
    /// `elem.drop`, in order, then every active data segment as
    /// `memory.init` then `data.drop`, in order; declarative element
    /// segments are dropped. An out-of-bounds segment traps immediately
    /// (`Trap::TableOutOfBounds` / `Trap::OutOfBounds`); the segments
    /// before it stay applied, as the spec requires, and the instance stays
    /// usable, so that functions it has already put into shared tables can
    /// still be called. `Trap::Host` reports a shared object the host has
    /// mutably borrowed. A second call does nothing.
    pub fn init_segments(&mut self) -> Result<(), Trap> {
        if self.initialized {
            return Ok(());
        }
        self.initialized = true;
        let module = self.module.clone();
        let addrs = Addrs { base: self.func_base, import_addrs: &self.import_addrs, num_funcs: self.func_types.len() };
        let offset_of = |expr| match eval_const(&addrs, &self.globals, expr) {
            Ok(Value::I32(v)) => Ok(v as u32),
            _ => Err(Trap::Invalid),
        };

        for (i, e) in module.elements.iter().enumerate() {
            match &e.mode {
                ElemMode::Active { table, offset } => {
                    let dst = offset_of(offset)?;
                    let seg = self.elems.get(i).ok_or(Trap::Invalid)?;
                    let n = seg.len() as u32;
                    self.tables
                        .get(*table as usize)
                        .ok_or(Trap::Invalid)?
                        .try_borrow_mut()
                        .map_err(|_| Trap::Host)?
                        .init(dst, seg, 0, n)?;
                    self.elems[i] = Vec::new();
                }
                ElemMode::Declarative => {
                    *self.elems.get_mut(i).ok_or(Trap::Invalid)? = Vec::new();
                }
                ElemMode::Passive => {}
            }
        }

        for (i, d) in module.data.iter().enumerate() {
            if let DataMode::Active { memory: mem_idx, offset } = &d.mode {
                if *mem_idx != 0 || module.num_memories() == 0 {
                    return Err(Trap::Invalid);
                }
                let dst = offset_of(offset)?;
                self.memory.try_borrow_mut().map_err(|_| Trap::Host)?.write(dst, &d.bytes)?;
                *self.data_dropped.get_mut(i).ok_or(Trap::Invalid)? = true;
            }
        }
        Ok(())
    }

    /// The module this instance was created from.
    pub fn module(&self) -> &Arc<Module> {
        &self.module
    }

    /// The limits this instance was created with.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The function index exported under `name`, if any.
    pub fn export_func(&self, name: &str) -> Option<u32> {
        self.module.export_func(name)
    }

    /// The table exported under `name`, if any (a handle to the shared
    /// object, ready to be imported by another instance).
    pub fn export_table(&self, name: &str) -> Option<TableRef> {
        match self.module.export(name)?.desc {
            crate::module::ExportDesc::Table(t) => self.table_ref(t).cloned(),
            _ => None,
        }
    }

    /// The memory exported under `name`, if any.
    pub fn export_memory(&self, name: &str) -> Option<MemoryRef> {
        match self.module.export(name)?.desc {
            crate::module::ExportDesc::Memory(0) => Some(self.memory.clone()),
            _ => None,
        }
    }

    /// The global exported under `name`, if any.
    pub fn export_global(&self, name: &str) -> Option<GlobalRef> {
        match self.module.export(name)?.desc {
            crate::module::ExportDesc::Global(g) => self.global_ref(g).cloned(),
            _ => None,
        }
    }

    /// The module's start function, if it declares one. The kernel runs it
    /// first through `Execution::start`.
    pub fn start_function(&self) -> Option<u32> {
        self.module.start
    }

    /// Linear memory 0 (empty if the module declares none).
    ///
    /// # Panics
    ///
    /// Panics if the host currently holds a mutable borrow of the memory
    /// (see the module documentation; the interpreter never does across a
    /// host call). Use [`Instance::memory_ref`] for a fallible borrow.
    pub fn memory(&self) -> Ref<'_, Memory> {
        self.memory.borrow()
    }

    /// Mutable access to linear memory 0. Panics like [`Instance::memory`].
    pub fn memory_mut(&self) -> RefMut<'_, Memory> {
        self.memory.borrow_mut()
    }

    /// The shared handle of linear memory 0.
    pub fn memory_ref(&self) -> &MemoryRef {
        &self.memory
    }

    /// Table `idx`, if it exists. Panics like [`Instance::memory`] if the
    /// host holds a mutable borrow of it.
    pub fn table(&self, idx: u32) -> Option<Ref<'_, Table>> {
        self.tables.get(idx as usize).map(|t| t.borrow())
    }

    /// The shared handle of table `idx`, if it exists.
    pub fn table_ref(&self, idx: u32) -> Option<&TableRef> {
        self.tables.get(idx as usize)
    }

    /// Number of tables (imported ones included).
    pub fn num_tables(&self) -> usize {
        self.tables.len()
    }

    /// Number of table elements currently in use, summed over all tables
    /// (never more than [`Limits::max_table_elements`] through this
    /// instance's own grows; a shared table grown elsewhere may push it
    /// over, after which grows through this instance fail). A table the
    /// host holds a mutable borrow of counts as full.
    pub fn table_elements(&self) -> u32 {
        self.tables.iter().fold(0u32, |n, t| n.saturating_add(t.try_borrow().map_or(u32::MAX, |t| t.size())))
    }

    /// `table.grow` on table `idx`: grow by `delta` entries set to `init`.
    /// Returns the old size, or `None` (which Wasm sees as `-1`, the spec's
    /// failed grow) when the table's own bound or the instance's element
    /// budget ([`Limits::max_table_elements`]) would be exceeded.
    /// `Trap::Invalid` names a table that does not exist; `Trap::Host`
    /// reports a table the host has mutably borrowed.
    pub(crate) fn grow_table(&mut self, idx: u32, delta: u32, init: Value) -> Result<Option<u32>, Trap> {
        let in_use = self.table_elements();
        let table = self.tables.get(idx as usize).ok_or(Trap::Invalid)?;
        match in_use.checked_add(delta) {
            Some(total) if total <= self.limits.max_table_elements => {
                Ok(table.try_borrow_mut().map_err(|_| Trap::Host)?.grow(delta, init))
            }
            _ => Ok(None),
        }
    }

    /// The current value of global `idx`, if it exists.
    pub fn global(&self, idx: u32) -> Option<Value> {
        self.globals.get(idx as usize).map(|g| g.get())
    }

    /// Set global `idx`. Fails if the index is unknown or the value has the
    /// wrong type (mutability is not checked: this is the host's door).
    pub fn set_global(&mut self, idx: u32, value: Value) -> Result<(), Error> {
        self.globals.get(idx as usize).ok_or_else(|| invalid("unknown global"))?.set(value)
    }

    /// The shared handle of global `idx`, if it exists.
    pub fn global_ref(&self, idx: u32) -> Option<&GlobalRef> {
        self.globals.get(idx as usize)
    }

    /// Number of globals (imported ones included).
    pub fn num_globals(&self) -> usize {
        self.globals.len()
    }

    /// Number of imported functions (function indices below this are
    /// host calls).
    pub fn num_imported_funcs(&self) -> usize {
        self.host_funcs.len()
    }

    /// Total number of functions (imports + definitions).
    pub fn num_funcs(&self) -> usize {
        self.func_types.len()
    }

    /// Whether function `func` is an import.
    pub fn is_imported_func(&self, func: u32) -> bool {
        (func as usize) < self.host_funcs.len()
    }

    /// The host id resolved for imported function `func`.
    pub fn host_func(&self, func: u32) -> Option<HostFuncId> {
        self.host_funcs.get(func as usize).copied()
    }

    /// The first address of this instance's function address range (see
    /// the module documentation).
    pub fn func_addr_base(&self) -> u32 {
        self.func_base
    }

    /// The function address of function `func` (function index space): the
    /// value `ref.func func` produces. An import has the address of the
    /// function it resolved to.
    pub fn func_addr(&self, func: u32) -> Option<u32> {
        if let Some(&a) = self.import_addrs.get(func as usize) {
            return Some(a);
        }
        if (func as usize) < self.func_types.len() {
            return self.func_base.checked_add(func);
        }
        None
    }

    /// The function index of this instance that function address `addr`
    /// names, if any: a defined function of this instance or one of its
    /// imports. `None` for a foreign address (see [`Host::call_foreign`]).
    pub fn local_func(&self, addr: u32) -> Option<u32> {
        let f = addr.wrapping_sub(self.func_base);
        if (f as usize) < self.func_types.len() && (f as usize) >= self.host_funcs.len() {
            return Some(f);
        }
        self.import_by_addr(addr)
    }

    /// The import whose function address is `addr`, if any.
    #[inline(never)]
    pub(crate) fn import_by_addr(&self, addr: u32) -> Option<u32> {
        self.import_addrs.iter().position(|&a| a == addr).map(|i| i as u32)
    }

    /// The type index of function `func` (function index space).
    pub fn func_type_index(&self, func: u32) -> Option<u32> {
        self.func_types.get(func as usize).copied()
    }

    /// The compiled code of the module.
    #[inline(always)]
    pub(crate) fn program(&self) -> &Arc<Program> {
        &self.program
    }

    /// The type of function `func` (function index space).
    pub fn func_type(&self, func: u32) -> Option<&FuncType> {
        self.module.get_type(self.func_type_index(func)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ExecResult, Execution};
    use alloc::rc::Rc;
    use alloc::vec;

    fn module(wat: &str) -> Arc<Module> {
        let bytes = wat::parse_str(wat).unwrap_or_else(|e| panic!("bad wat: {e}\n{wat}"));
        Arc::new(Module::new(&bytes).unwrap_or_else(|e| panic!("bad module: {e}\n{wat}")))
    }

    /// Instantiate `wat` with every import resolved from `exporter`'s
    /// exports under module name "a" (functions become cross calls served
    /// by [`Store`]), at function address range `base`.
    fn link(wat: &str, exporter: Option<&Instance>, base: u32, store: &mut Store) -> Result<Instance, Error> {
        let mut resolve = |m: &str, n: &str, _ty: &ExternType<'_>| -> Option<Extern> {
            if m != "a" {
                return None;
            }
            let ex = exporter?;
            Some(match ex.module().export(n)?.desc {
                crate::module::ExportDesc::Func(f) => {
                    let id = HostFuncId(store.cross.len() as u32);
                    store.cross.push(f);
                    Extern::Func { id, ty: ex.func_type(f)?.clone(), addr: ex.func_addr(f) }
                }
                crate::module::ExportDesc::Table(t) => Extern::Table(ex.table_ref(t)?.clone()),
                crate::module::ExportDesc::Memory(_) => Extern::Memory(ex.memory_ref().clone()),
                crate::module::ExportDesc::Global(g) => Extern::Global(ex.global_ref(g)?.clone()),
            })
        };
        Instance::instantiate(module(wat), &mut resolve, &Limits::default(), base)
    }

    /// A two-instance store: the host side of cross and foreign calls into
    /// `other` (the exporter), which is lent to a nested execution.
    #[derive(Default)]
    struct Store {
        other: Option<Instance>,
        /// Function index in `other` of each `HostFuncId`.
        cross: Vec<u32>,
        foreign_calls: u32,
    }

    impl Store {
        fn run_other(&mut self, func: u32, args: &[Value], results: &mut [Value]) -> Result<HostOutcome, Trap> {
            let mut other = self.other.take().ok_or(Trap::Host)?;
            let r = Execution::new(&other, func, args).map(|mut e| e.run(&mut other, self, 1 << 20));
            self.other = Some(other);
            match r {
                Ok(ExecResult::Done(v)) if v.len() == results.len() => {
                    results.copy_from_slice(&v);
                    Ok(HostOutcome::Continue)
                }
                Ok(ExecResult::Trap(t)) => Err(t),
                _ => Err(Trap::Host),
            }
        }
    }

    impl Host for Store {
        fn call(
            &mut self,
            _ctx: &mut HostCtx<'_>,
            func: HostFuncId,
            args: &[Value],
            results: &mut [Value],
        ) -> Result<HostOutcome, Trap> {
            let f = *self.cross.get(func.0 as usize).ok_or(Trap::Host)?;
            self.run_other(f, args, results)
        }

        fn call_foreign(
            &mut self,
            _ctx: &mut HostCtx<'_>,
            addr: u32,
            ty: &FuncType,
            args: &[Value],
            results: &mut [Value],
        ) -> Result<HostOutcome, Trap> {
            self.foreign_calls += 1;
            let other = self.other.as_ref().ok_or(Trap::Host)?;
            let f = other.local_func(addr).ok_or(Trap::Host)?;
            if other.func_type(f) != Some(ty) {
                return Err(Trap::IndirectCallTypeMismatch);
            }
            self.run_other(f, args, results)
        }
    }

    fn call(inst: &mut Instance, host: &mut dyn Host, name: &str, args: &[Value]) -> ExecResult {
        let f = inst.export_func(name).unwrap_or_else(|| panic!("no export {name}"));
        Execution::new(inst, f, args).unwrap().run(inst, host, 1 << 20)
    }

    fn one(inst: &mut Instance, host: &mut dyn Host, name: &str, args: &[Value]) -> Value {
        match call(inst, host, name, args) {
            ExecResult::Done(v) if v.len() == 1 => v[0],
            other => panic!("{name}: expected one result, got {other:?}"),
        }
    }

    const EXPORTER: &str = r#"(module
        (memory (export "mem") 1 4)
        (table (export "tab") 4 10 funcref)
        (global (export "g") (mut i32) (i32.const 10))
        (global (export "c") i32 (i32.const 7))
        (func (export "size") (result i32) (memory.size))
        (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0)))
        (func (export "load8") (param i32) (result i32) (i32.load8_u (local.get 0)))
        (func (export "store8") (param i32 i32) (i32.store8 (local.get 0) (local.get 1)))
        (func (export "get") (result i32) (global.get 0))
        (func $four (export "four") (result i32) (i32.const 4))
        (func (export "call") (param i32) (result i32) (call_indirect (type 0) (local.get 0)))
        (func (export "ref_four") (result funcref) (ref.func $four))
        (type (func (result i32)))
        (elem (i32.const 0) $four)
    )"#;

    #[test]
    fn shared_memory_grow_is_visible_to_every_importer_and_resumed_run() {
        let mut store = Store::default();
        let a = link(EXPORTER, None, 0, &mut store).unwrap();
        let mut b = link(
            r#"(module
                (import "a" "mem" (memory 1))
                (func (export "size") (result i32) (memory.size))
                (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0)))
                (func (export "load8") (param i32) (result i32) (i32.load8_u (local.get 0)))
                ;; Spins `n` times, then loads from `addr`: a run that can be
                ;; interrupted by fuel before the load.
                (func (export "spin_load") (param $n i32) (param $addr i32) (result i32)
                    (loop $l
                        (local.set $n (i32.sub (local.get $n) (i32.const 1)))
                        (br_if $l (local.get $n)))
                    (i32.load8_u (local.get $addr)))
            )"#,
            Some(&a),
            1 << 20,
            &mut store,
        )
        .unwrap();
        store.other = Some(a);
        assert!(Rc::ptr_eq(b.memory_ref(), store.other.as_ref().unwrap().memory_ref()));
        assert_eq!(one(&mut b, &mut store, "size", &[]), Value::I32(1));

        // Grown through the importer: the exporter sees it, and vice versa.
        assert_eq!(one(&mut b, &mut store, "grow", &[Value::I32(1)]), Value::I32(1));
        let mut a = store.other.take().unwrap();
        assert_eq!(one(&mut a, &mut store, "size", &[]), Value::I32(2));
        assert_eq!(one(&mut a, &mut store, "grow", &[Value::I32(1)]), Value::I32(2));
        assert_eq!(a.memory().size_pages(), 3);
        store.other = Some(a);
        assert_eq!(one(&mut b, &mut store, "size", &[]), Value::I32(3));

        // A run interrupted by fuel resumes after another instance grew the
        // memory and wrote into the new page: the load must see the new
        // size and the byte (the kernel interleaves instances like this).
        let addr = 3 * crate::types::PAGE_SIZE as i32 + 5;
        let spin = b.export_func("spin_load").unwrap();
        let mut e = Execution::new(&b, spin, &[Value::I32(1000), Value::I32(addr)]).unwrap();
        assert_eq!(e.run(&mut b, &mut store, 100), ExecResult::OutOfFuel);
        let mut a = store.other.take().unwrap();
        assert_eq!(one(&mut a, &mut store, "grow", &[Value::I32(1)]), Value::I32(3));
        assert_eq!(call(&mut a, &mut store, "store8", &[Value::I32(addr), Value::I32(0x5A)]), ExecResult::Done(vec![]));
        store.other = Some(a);
        assert_eq!(e.run(&mut b, &mut store, u64::MAX), ExecResult::Done(vec![Value::I32(0x5A)]));
        // The limit of the exporter's type (4 pages) holds for everyone.
        assert_eq!(one(&mut b, &mut store, "grow", &[Value::I32(1)]), Value::I32(-1));
        assert_eq!(one(&mut b, &mut store, "size", &[]), Value::I32(4));
    }

    /// The host grows the memory during a host call and the instance uses
    /// the new page right after the call returns.
    #[test]
    fn host_call_may_grow_the_memory() {
        struct Grower;
        impl Host for Grower {
            fn call(
                &mut self,
                ctx: &mut HostCtx<'_>,
                _func: HostFuncId,
                _args: &[Value],
                _results: &mut [Value],
            ) -> Result<HostOutcome, Trap> {
                let mut mem = ctx.memory.try_borrow_mut().map_err(|_| Trap::Host)?;
                mem.grow(1).ok_or(Trap::Host)?;
                mem.write(65536, &[1, 2, 3, 4])?;
                Ok(HostOutcome::Continue)
            }
        }
        let mut inst = Instance::new(
            module(
                r#"(module
                    (import "env" "grow" (func $grow))
                    (memory 1 2)
                    (func (export "run") (result i32)
                        (call $grow)
                        (i32.store (i32.const 65540) (i32.load (i32.const 65536)))
                        (i32.load (i32.const 65540)))
                )"#,
            ),
            &mut |_, _, _| Ok(HostFuncId(0)),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(call(&mut inst, &mut Grower, "run", &[]), ExecResult::Done(vec![Value::I32(0x0403_0201)]));
        assert_eq!(inst.memory().size_pages(), 2);
    }

    #[test]
    fn shared_table_function_addresses_and_foreign_calls() {
        let mut store = Store::default();
        let a = link(EXPORTER, None, 0, &mut store).unwrap();
        let a_four = a.export_func("four").unwrap();
        assert_eq!(a.func_addr(a_four), Some(a_four));
        let b = link(
            r#"(module
                (type $t (func (result i32)))
                (import "a" "tab" (table 4 funcref))
                (import "a" "four" (func $four (result i32)))
                (import "a" "load8" (func $load8 (param i32) (result i32)))
                (func $five (result i32) (i32.const 5))
                (func (export "call") (param i32) (result i32) (call_indirect (type $t) (local.get 0)))
                (func (export "ref_four") (result funcref) (ref.func $four))
                (func (export "ref_five") (result funcref) (ref.func $five))
                (func (export "ref_load8") (result funcref) (ref.func $load8))
                (elem (i32.const 1) $five $four $load8)
            )"#,
            Some(&a),
            1000,
            &mut store,
        )
        .unwrap();
        let mut b = b;
        // Imports of another instance's functions keep that instance's
        // addresses; defined functions live in the importer's range.
        assert_eq!(b.func_addr_base(), 1000);
        assert_eq!(b.func_addr(0), Some(a_four));
        assert_eq!(b.func_addr(2), Some(1002));
        assert_eq!(b.local_func(a_four), Some(0));
        assert_eq!(b.local_func(1002), Some(2));
        assert_eq!(b.local_func(500), None);
        store.other = Some(a);
        assert_eq!(one(&mut b, &mut store, "ref_four", &[]), Value::FuncRef(Some(a_four)));
        assert_eq!(one(&mut b, &mut store, "ref_five", &[]), Value::FuncRef(Some(1002)));
        // The elem segment went into the shared table.
        let a = store.other.take().unwrap();
        let tab = a.table(0).unwrap();
        assert_eq!(tab.get(0), Ok(Value::FuncRef(Some(a_four))));
        assert_eq!(tab.get(1), Ok(Value::FuncRef(Some(1002))));
        assert_eq!(tab.get(2), Ok(Value::FuncRef(Some(a_four))));
        let a_load8 = a.export_func("load8").unwrap();
        assert_eq!(tab.get(3), Ok(Value::FuncRef(Some(a_load8))));
        drop(tab);
        store.other = Some(a);

        // From b: index 2 is a's `four` through the import (a cross call),
        // index 1 is b's own function, index 3 has the wrong type.
        assert_eq!(one(&mut b, &mut store, "call", &[Value::I32(2)]), Value::I32(4));
        assert_eq!(one(&mut b, &mut store, "call", &[Value::I32(1)]), Value::I32(5));
        assert_eq!(call(&mut b, &mut store, "call", &[Value::I32(3)]), ExecResult::Trap(Trap::IndirectCallTypeMismatch));
        assert_eq!(store.foreign_calls, 0);

        // From a: index 1 is b's `five`, a foreign function, served by the
        // host; the type check is the host's too.
        let mut a = store.other.take().unwrap();
        store.other = Some(b);
        assert_eq!(one(&mut a, &mut store, "call", &[Value::I32(0)]), Value::I32(4));
        assert_eq!(one(&mut a, &mut store, "call", &[Value::I32(1)]), Value::I32(5));
        assert_eq!(store.foreign_calls, 1);
        // Index 3 is a's own `load8`, of the wrong type: checked locally.
        assert_eq!(call(&mut a, &mut store, "call", &[Value::I32(3)]), ExecResult::Trap(Trap::IndirectCallTypeMismatch));
        assert_eq!(store.foreign_calls, 1);
        assert_eq!(call(&mut a, &mut store, "call", &[Value::I32(4)]), ExecResult::Trap(Trap::UndefinedElement));
        // A foreign call with a mismatching type: b's `call` (param i32)
        // put into the table by the host and called as () -> i32.
        let b_ref = store.other.as_ref().unwrap();
        let b_call = b_ref.export_func("call").unwrap();
        let b_call_addr = b_ref.func_addr(b_call).unwrap();
        a.table_ref(0).unwrap().borrow_mut().set(0, Value::FuncRef(Some(b_call_addr))).unwrap();
        assert_eq!(call(&mut a, &mut store, "call", &[Value::I32(0)]), ExecResult::Trap(Trap::IndirectCallTypeMismatch));
        assert_eq!(store.foreign_calls, 2);
        // Without an embedder that knows the address, a foreign call is a
        // host error (the default `call_foreign`).
        struct NoForeign;
        impl Host for NoForeign {
            fn call(&mut self, _: &mut HostCtx<'_>, _: HostFuncId, _: &[Value], _: &mut [Value]) -> Result<HostOutcome, Trap> {
                Err(Trap::Host)
            }
        }
        assert_eq!(call(&mut a, &mut NoForeign, "call", &[Value::I32(1)]), ExecResult::Trap(Trap::Host));
    }

    #[test]
    fn shared_globals() {
        let mut store = Store::default();
        let a = link(EXPORTER, None, 0, &mut store).unwrap();
        let mut b = link(
            r#"(module
                (import "a" "g" (global $g (mut i32)))
                (import "a" "c" (global $c i32))
                (global (export "sum") i32 (i32.const 0))
                (func (export "set") (param i32) (global.set $g (local.get 0)))
                (func (export "get") (result i32) (i32.add (global.get $g) (global.get $c)))
            )"#,
            Some(&a),
            100,
            &mut store,
        )
        .unwrap();
        assert!(Rc::ptr_eq(b.global_ref(0).unwrap(), a.global_ref(0).unwrap()));
        assert_eq!(b.num_globals(), 3);
        assert_eq!(one(&mut b, &mut store, "get", &[]), Value::I32(17));
        assert_eq!(call(&mut b, &mut store, "set", &[Value::I32(32)]), ExecResult::Done(vec![]));
        store.other = Some(a);
        let mut a = store.other.take().unwrap();
        assert_eq!(one(&mut a, &mut store, "get", &[]), Value::I32(32));
        assert_eq!(a.global(0), Some(Value::I32(32)));
        a.export_global("g").unwrap().set(Value::I32(1)).unwrap();
        assert_eq!(one(&mut b, &mut store, "get", &[]), Value::I32(8));
        assert_eq!(b.export_global("sum").unwrap().get(), Value::I32(0));
        assert!(b.export_global("g").is_none());
    }

    #[test]
    fn import_matching() {
        let mut store = Store::default();
        let a = link(EXPORTER, None, 0, &mut store).unwrap();
        let ok = [
            r#"(import "a" "mem" (memory 1))"#,
            r#"(import "a" "mem" (memory 1 4))"#,
            r#"(import "a" "mem" (memory 0 5))"#,
            r#"(import "a" "tab" (table 4 funcref))"#,
            r#"(import "a" "tab" (table 2 10 funcref))"#,
            r#"(import "a" "tab" (table 4 11 funcref))"#,
            r#"(import "a" "g" (global (mut i32)))"#,
            r#"(import "a" "c" (global i32))"#,
            r#"(import "a" "four" (func (result i32)))"#,
        ];
        for imp in ok {
            let r = link(&alloc::format!("(module {imp})"), Some(&a), 50, &mut store);
            assert!(r.is_ok(), "{imp}: {:?}", r.err());
        }
        let incompatible = [
            r#"(import "a" "mem" (memory 2))"#,
            r#"(import "a" "mem" (memory 1 3))"#,
            r#"(import "a" "tab" (table 5 funcref))"#,
            r#"(import "a" "tab" (table 4 9 funcref))"#,
            r#"(import "a" "tab" (table 4 externref))"#,
            r#"(import "a" "g" (global i32))"#,
            r#"(import "a" "c" (global (mut i32)))"#,
            r#"(import "a" "c" (global i64))"#,
            r#"(import "a" "four" (func (result i64)))"#,
            r#"(import "a" "four" (func (param i32) (result i32)))"#,
            r#"(import "a" "four" (global i32))"#,
            r#"(import "a" "mem" (func))"#,
            r#"(import "a" "tab" (memory 1))"#,
            r#"(import "a" "g" (table 1 funcref))"#,
        ];
        for imp in incompatible {
            let r = link(&alloc::format!(r#"(module (import "a" "four" (func)) {imp})"#), Some(&a), 50, &mut store);
            // The first import is incompatible too (wrong result type), so
            // the reported index must be 0: imports are checked in order.
            assert_eq!(r.err(), Some(Error::IncompatibleImport { import: 0 }), "{imp}");
            let r = link(&alloc::format!("(module {imp})"), Some(&a), 50, &mut store);
            let e = r.err().unwrap_or_else(|| panic!("{imp}: linked"));
            assert_eq!(e, Error::IncompatibleImport { import: 0 }, "{imp}");
            assert_eq!(e.message(), "incompatible import type");
        }
        for imp in [r#"(import "a" "nope" (memory 1))"#, r#"(import "b" "mem" (memory 1))"#] {
            let r = link(&alloc::format!(r#"(module (import "a" "c" (global i32)) {imp})"#), Some(&a), 50, &mut store);
            assert_eq!(r.err(), Some(Error::Link { import: 1 }), "{imp}");
        }
        // A memory of a type declared without a maximum cannot satisfy an
        // import that wants one, however small the runtime cap is.
        let nomax = link("(module (memory (export \"mem\") 1))", None, 0, &mut store).unwrap();
        assert!(link(r#"(module (import "a" "mem" (memory 1)))"#, Some(&nomax), 9, &mut store).is_ok());
        assert_eq!(
            link(r#"(module (import "a" "mem" (memory 1 256)))"#, Some(&nomax), 9, &mut store).err(),
            Some(Error::IncompatibleImport { import: 0 })
        );
        // The address range must fit.
        assert_eq!(
            link("(module (func) (func))", None, u32::MAX - 1, &mut store).err(),
            Some(Error::Limit { msg: "function address range overflows" })
        );
        assert!(link("(module (func) (func))", None, u32::MAX - 2, &mut store).is_ok());
    }

    #[test]
    fn imported_function_address_must_lie_outside_the_importer_range() {
        // `f` is an import, `g` a defined function of the same type; `go`
        // stores `ref.func $f` in the table and calls through it. With an
        // import address that collides with the importer's own range, the
        // table entry would name `g` (or another slot) instead of `f`.
        const WAT: &str = r#"(module
            (type $t (func (result i32)))
            (import "x" "f" (func $f (type $t)))
            (func $g (result i32) (i32.const 7))
            (table 1 funcref)
            (elem declare func $f)
            (func (export "go") (result i32)
                (table.set 0 (i32.const 0) (ref.func $f))
                (call_indirect (type $t) (i32.const 0)))
        )"#;
        fn instantiate(addr: Option<u32>, base: u32) -> Result<Instance, Error> {
            Instance::instantiate(
                module(WAT),
                &mut |_, _, ty| {
                    let ExternType::Func(ft) = *ty else {
                        return None;
                    };
                    Some(Extern::Func { id: HostFuncId(0), ty: ft.clone(), addr })
                },
                &Limits::default(),
                base,
            )
        }
        struct Hundred {
            calls: u32,
        }
        impl Host for Hundred {
            fn call(
                &mut self,
                _: &mut HostCtx<'_>,
                func: HostFuncId,
                _: &[Value],
                results: &mut [Value],
            ) -> Result<HostOutcome, Trap> {
                assert_eq!(func, HostFuncId(0));
                self.calls += 1;
                results[0] = Value::I32(100);
                Ok(HostOutcome::Continue)
            }
        }

        // Three functions (`f`, `g`, `go`): the range at base 1000 is
        // 1000..1003, and every address in it is refused, the import's own
        // slot included (a foreign address is never the slot's).
        for base in [1000, 0] {
            for a in 0..3 {
                assert_eq!(
                    instantiate(Some(base + a), base).err(),
                    Some(Error::IncompatibleImport { import: 0 }),
                    "base {base}, addr {}",
                    base + a
                );
            }
        }
        // Just outside the range, far away, and no address at all: linked,
        // and `call_indirect` through `ref.func $f` reaches the host.
        for (addr, base) in [(Some(1003), 1000), (Some(999), 1000), (Some(5000), 1000), (None, 1000), (None, 0)] {
            let mut inst = instantiate(addr, base).unwrap_or_else(|e| panic!("base {base}, addr {addr:?}: {e}"));
            assert_eq!(inst.func_addr(0), Some(addr.unwrap_or(base)));
            let mut host = Hundred { calls: 0 };
            assert_eq!(one(&mut inst, &mut host, "go", &[]), Value::I32(100), "base {base}, addr {addr:?}");
            assert_eq!(host.calls, 1);
        }
    }

    #[test]
    fn two_indices_one_table() {
        let mut store = Store::default();
        let a = link(EXPORTER, None, 0, &mut store).unwrap();
        let mut b = link(
            r#"(module
                (import "a" "tab" (table $t1 4 funcref))
                (import "a" "tab" (table $t2 4 funcref))
                (func (export "copy") (param i32 i32 i32)
                    (table.copy $t1 $t2 (local.get 0) (local.get 1) (local.get 2)))
                (func (export "size2") (result i32) (table.size $t2))
                (func (export "grow1") (param i32) (result i32)
                    (table.grow $t1 (ref.null func) (local.get 0)))
            )"#,
            Some(&a),
            100,
            &mut store,
        )
        .unwrap();
        assert_eq!(b.num_tables(), 2);
        assert_eq!(call(&mut b, &mut store, "copy", &[Value::I32(1), Value::I32(0), Value::I32(3)]), ExecResult::Done(vec![]));
        // `[four, null, null, null]` copied onto itself one slot up.
        let four = a.export_func("four").unwrap();
        assert_eq!(a.table(0).unwrap().get(1), Ok(Value::FuncRef(Some(four))));
        assert_eq!(a.table(0).unwrap().get(3), Ok(Value::FuncRef(None)));
        // The element budget counts the shared table once per index.
        assert_eq!(b.table_elements(), 8);
        assert_eq!(one(&mut b, &mut store, "grow1", &[Value::I32(2)]), Value::I32(4));
        assert_eq!(one(&mut b, &mut store, "size2", &[]), Value::I32(6));
        assert_eq!(a.table(0).unwrap().size(), 6);
    }

    #[test]
    fn init_segments_keeps_what_was_applied() {
        let mut store = Store::default();
        let a = link(EXPORTER, None, 0, &mut store).unwrap();
        let module = module(
            r#"(module
                (import "a" "tab" (table 4 funcref))
                (import "a" "mem" (memory 1))
                (func $f (export "f") (result i32) (i32.const 9))
                (elem (i32.const 3) $f)
                (data (i32.const 10) "ok")
                (elem (i32.const 4) $f)
                (data (i32.const 20) "never")
            )"#,
        );
        let mut resolve = |_: &str, n: &str, _: &ExternType<'_>| match n {
            "tab" => Some(Extern::Table(a.table_ref(0).unwrap().clone())),
            "mem" => Some(Extern::Memory(a.memory_ref().clone())),
            _ => None,
        };
        let mut b = Instance::allocate(module.clone(), &mut resolve, &Limits::default(), 100).unwrap();
        // Nothing applied yet.
        assert_eq!(a.table(0).unwrap().get(3), Ok(Value::FuncRef(None)));
        assert_eq!(b.init_segments(), Err(Trap::TableOutOfBounds));
        // The first element segment was applied, the data segments were not
        // reached, and the instance is usable.
        assert_eq!(a.table(0).unwrap().get(3), Ok(Value::FuncRef(Some(100))));
        let mut buf = [0u8; 2];
        a.memory().read(10, &mut buf).unwrap();
        assert_eq!(buf, [0, 0]);
        assert_eq!(one(&mut b, &mut store, "f", &[]), Value::I32(9));
        // A second call is a no-op, and the one-shot form reports the trap.
        assert_eq!(b.init_segments(), Ok(()));
        assert_eq!(
            Instance::instantiate(module, &mut resolve, &Limits::default(), 100).err(),
            Some(Error::Trap(Trap::TableOutOfBounds))
        );
        // Through the exporter, the foreign function is callable.
        store.other = Some(b);
        let mut a = a;
        assert_eq!(one(&mut a, &mut store, "call", &[Value::I32(3)]), Value::I32(9));
    }

    /// A host that keeps a borrow of a shared object while the interpreter
    /// needs it gets a trap, not a panic.
    #[test]
    fn busy_borrows_trap_instead_of_panicking() {
        struct NoHost;
        impl Host for NoHost {
            fn call(&mut self, _: &mut HostCtx<'_>, _: HostFuncId, _: &[Value], _: &mut [Value]) -> Result<HostOutcome, Trap> {
                Err(Trap::Host)
            }
        }
        let mut inst = Instance::new(module(EXPORTER), &mut |_, _, _| Err(()), &Limits::default()).unwrap();
        let size = inst.export_func("size").unwrap();
        let held = inst.memory_ref().clone();
        let guard = held.borrow_mut();
        let mut e = Execution::new(&inst, size, &[]).unwrap();
        assert_eq!(e.run(&mut inst, &mut NoHost, 100), ExecResult::Trap(Trap::Host));
        drop(guard);
        assert_eq!(call(&mut inst, &mut NoHost, "size", &[]), ExecResult::Done(vec![Value::I32(1)]));
        let tab = inst.table_ref(0).unwrap().clone();
        let guard = tab.borrow_mut();
        assert_eq!(call(&mut inst, &mut NoHost, "call", &[Value::I32(0)]), ExecResult::Trap(Trap::Host));
        drop(guard);
        assert_eq!(one(&mut inst, &mut NoHost, "call", &[Value::I32(0)]), Value::I32(4));
        assert!(alloc::format!("{inst:?}").contains("func_base: 0"));
    }

    #[test]
    fn program_is_compiled_once_per_module() {
        let m = module(EXPORTER);
        assert!(m.program.is_compiled());
        let a = Instance::new(m.clone(), &mut |_, _, _| Err(()), &Limits::default()).unwrap();
        let b = Instance::new(m.clone(), &mut |_, _, _| Err(()), &Limits::default()).unwrap();
        assert!(Arc::ptr_eq(a.program(), b.program()));
        assert!(m.program.get().is_some_and(|p| Arc::ptr_eq(p, a.program())));
        // A clone of the module shares the program too.
        let c = Instance::new(Arc::new((*m).clone()), &mut |_, _, _| Err(()), &Limits::default()).unwrap();
        assert!(Arc::ptr_eq(a.program(), c.program()));
        // A module built without `Module::new` has no cache; its instances
        // compile their own, equal, programs, and it compares equal to the
        // compiled module (the cache is not part of the module's identity).
        let bare = Arc::new(Module { program: Default::default(), ..(*m).clone() });
        assert!(!bare.program.is_compiled());
        assert_eq!(*bare, *m);
        let d = Instance::new(bare.clone(), &mut |_, _, _| Err(()), &Limits::default()).unwrap();
        let e = Instance::new(bare, &mut |_, _, _| Err(()), &Limits::default()).unwrap();
        assert!(!Arc::ptr_eq(d.program(), e.program()));
        assert_eq!(**d.program(), **a.program());
        assert_eq!(alloc::format!("{:?}", m.program), "ProgramCache(compiled)");
        let mut inst = a;
        assert_eq!(one(&mut inst, &mut Store::default(), "four", &[]), Value::I32(4));
    }

    /// A module with every kind of state a snapshot covers.
    const STATEFUL: &str = r#"(module
        (type $t (func (result i32)))
        (memory 1 4)
        (table 4 8 funcref)
        (global $g (mut i32) (i32.const 10))
        (global $r (mut funcref) (ref.null func))
        (global $f (mut f64) (f64.const 1.5))
        (elem (i32.const 0) $one $two)
        (elem $passive funcref (ref.func $two) (ref.func $one))
        (elem declare funcref (ref.func $one))
        (data (i32.const 16) "init")
        (data $pd "passive")
        (func $one (type $t) (i32.const 1))
        (func $two (type $t) (i32.const 2))
        (func (export "mutate")
            (drop (memory.grow (i32.const 1)))
            (i32.store8 (i32.const 65536) (i32.const 0xAB))
            (i32.store8 (i32.const 16) (i32.const 0x55))
            (global.set $g (i32.const 42))
            (global.set $r (ref.func $two))
            (global.set $f (f64.const -0.0))
            (table.set (i32.const 3) (ref.func $one))
            (drop (table.grow (ref.null func) (i32.const 2)))
            (table.init $passive (i32.const 4) (i32.const 0) (i32.const 1))
            (data.drop $pd)
            (elem.drop $passive))
        (func (export "size") (result i32) (memory.size))
        (func (export "tsize") (result i32) (table.size))
        (func (export "load8") (param i32) (result i32) (i32.load8_u (local.get 0)))
        (func (export "g") (result i32) (global.get $g))
        (func (export "f") (result f64) (global.get $f))
        (func (export "call_r") (result i32)
            (table.set (i32.const 0) (global.get $r)) (call_indirect (type $t) (i32.const 0)))
        (func (export "call") (param i32) (result i32) (call_indirect (type $t) (local.get 0)))
        (func (export "init_pd") (memory.init $pd (i32.const 0) (i32.const 0) (i32.const 1)))
        (func (export "init_passive")
            (table.init $passive (i32.const 0) (i32.const 0) (i32.const 1))))"#;

    fn stateful(base: u32) -> Instance {
        Instance::instantiate(module(STATEFUL), &mut |_, _, _| None, &Limits::default(), base).unwrap()
    }

    fn restore(snap: &Snapshot, base: u32, limits: &Limits) -> Result<Instance, Error> {
        Instance::from_snapshot(module(STATEFUL), snap, &mut |_, _, _| None, limits, base)
    }

    #[test]
    fn snapshot_round_trip() {
        let mut host = Store::default();
        let mut a = stateful(0);
        let fresh = a.snapshot().unwrap();
        assert_eq!(fresh.memory_pages(), 1);
        assert_eq!(&fresh.memory()[16..20], b"init");
        assert_eq!(fresh.data_dropped, [true, false]);
        assert_eq!(fresh.elem_dropped, [true, false, true]);
        assert_eq!(call(&mut a, &mut host, "mutate", &[]), ExecResult::Done(vec![]));
        let snap = a.snapshot().unwrap();
        assert_eq!(snap.memory_pages(), 2);
        assert_eq!(snap.memory()[65536], 0xAB);
        assert_eq!(snap.num_globals(), 3);
        assert_eq!(snap.num_tables(), 1);
        // Function references are stored by index: $two is function 1.
        assert_eq!(snap.globals[1], Value::FuncRef(Some(1)));
        assert_eq!(snap.tables[0].len(), 6);
        assert_eq!(snap.tables[0][3], Value::FuncRef(Some(0)));
        assert_eq!(snap.tables[0][4], Value::FuncRef(Some(1)));
        assert_eq!(snap.data_dropped, [true, true]);
        assert_eq!(snap.elem_dropped, [true, true, true]);
        assert_eq!(Snapshot::from_bytes(&snap.to_bytes()).unwrap(), snap);

        // Restored at another function address range through the bytes.
        let bytes = snap.to_bytes();
        let decoded = Snapshot::from_bytes(&bytes).unwrap();
        let mut b = restore(&decoded, 1000, &Limits::default()).unwrap();
        assert_eq!(b.func_addr_base(), 1000);
        // The restored instance's own snapshot is the same snapshot.
        assert_eq!(b.snapshot().unwrap(), snap);
        assert_eq!(one(&mut b, &mut host, "size", &[]), Value::I32(2));
        assert_eq!(one(&mut b, &mut host, "tsize", &[]), Value::I32(6));
        assert_eq!(one(&mut b, &mut host, "load8", &[Value::I32(65536)]), Value::I32(0xAB));
        assert_eq!(one(&mut b, &mut host, "load8", &[Value::I32(16)]), Value::I32(0x55));
        assert_eq!(one(&mut b, &mut host, "g", &[]), Value::I32(42));
        assert_eq!(one(&mut b, &mut host, "f", &[]), Value::F64(-0.0));
        assert!(matches!(one(&mut b, &mut host, "f", &[]), Value::F64(f) if f.is_sign_negative()));
        // The relocated references call the right functions.
        assert_eq!(b.global(1), Some(Value::FuncRef(Some(1001))));
        assert_eq!(one(&mut b, &mut host, "call_r", &[]), Value::I32(2));
        assert_eq!(one(&mut b, &mut host, "call", &[Value::I32(3)]), Value::I32(1));
        assert_eq!(one(&mut b, &mut host, "call", &[Value::I32(4)]), Value::I32(2));
        assert_eq!(call(&mut b, &mut host, "call", &[Value::I32(5)]), ExecResult::Trap(Trap::UninitializedElement));
        // Dropped segments stay dropped, and nothing is re-applied.
        assert_eq!(call(&mut b, &mut host, "init_pd", &[]), ExecResult::Trap(Trap::OutOfBounds));
        assert_eq!(call(&mut b, &mut host, "init_passive", &[]), ExecResult::Trap(Trap::TableOutOfBounds));
        assert_eq!(b.init_segments(), Ok(()));
        assert_eq!(one(&mut b, &mut host, "load8", &[Value::I32(16)]), Value::I32(0x55));
        // What was run since (`call_r` overwrote table entry 0) shows.
        let after = b.snapshot().unwrap();
        assert_eq!(after.tables[0][0], Value::FuncRef(Some(1)));
        assert_eq!(after.memory(), snap.memory());

        // A fresh instance's snapshot restores to a fresh instance, whose
        // passive segments are still usable.
        let mut c = restore(&fresh, 7, &Limits::default()).unwrap();
        assert_eq!(c.snapshot().unwrap(), fresh);
        assert_eq!(call(&mut c, &mut host, "init_pd", &[]), ExecResult::Done(vec![]));
        assert_eq!(one(&mut c, &mut host, "load8", &[Value::I32(0)]), Value::I32(b'p' as i32));
        assert_eq!(call(&mut c, &mut host, "init_passive", &[]), ExecResult::Done(vec![]));
        assert_eq!(one(&mut c, &mut host, "call", &[Value::I32(0)]), Value::I32(2));
    }

    #[test]
    fn snapshot_requires_unshared_objects() {
        let inst = stateful(0);
        let shared =
            |r: &Result<Snapshot, Error>| matches!(r, Err(Error::Unsupported { offset: 0, msg }) if msg.contains("shared"));
        assert!(inst.snapshot().is_ok());
        let held = inst.memory_ref().clone();
        assert!(shared(&inst.snapshot()));
        drop(held);
        assert!(inst.snapshot().is_ok());
        let held = inst.table_ref(0).unwrap().clone();
        assert!(shared(&inst.snapshot()));
        drop(held);
        let held = inst.global_ref(2).unwrap().clone();
        assert!(shared(&inst.snapshot()));
        drop(held);
        // A host borrow is reported as a trap, not a panic.
        let guard = inst.memory_ref().borrow_mut();
        assert_eq!(inst.snapshot(), Err(Error::Trap(Trap::Host)));
        drop(guard);
        assert!(inst.snapshot().is_ok());

        // Exporting to another live instance shares the objects; the
        // importer cannot be snapshotted either, since it imports them.
        let mut store = Store::default();
        let exporter = Instance::new(module(EXPORTER), &mut |_, _, _| Err(()), &Limits::default()).unwrap();
        assert!(exporter.snapshot().is_ok());
        let importer = link(r#"(module (import "a" "mem" (memory 1)))"#, Some(&exporter), 100, &mut store).unwrap();
        assert!(shared(&exporter.snapshot()));
        assert!(matches!(
            importer.snapshot(),
            Err(Error::Unsupported { offset: 0, msg }) if msg.contains("imports")
        ));
        let snap = Snapshot {
            memory: alloc::vec![0; 65536],
            globals: Vec::new(),
            tables: Vec::new(),
            data_dropped: Vec::new(),
            elem_dropped: Vec::new(),
        };
        assert!(matches!(
            Instance::from_snapshot(
                importer.module().clone(),
                &snap,
                &mut |_, _, _| Some(Extern::Memory(exporter.memory_ref().clone())),
                &Limits::default(),
                200,
            ),
            Err(Error::Unsupported { offset: 0, .. })
        ));
        drop(importer);
        assert!(exporter.snapshot().is_ok());

        // A foreign function reference in a table cannot be captured.
        let lone = stateful(0);
        lone.table_ref(0).unwrap().borrow_mut().set(0, Value::FuncRef(Some(5000))).unwrap();
        assert!(matches!(
            lone.snapshot(),
            Err(Error::Unsupported { offset: 0, msg }) if msg.contains("another instance")
        ));
        lone.table_ref(0).unwrap().borrow_mut().set(0, Value::FuncRef(None)).unwrap();
        assert!(lone.snapshot().is_ok());
    }

    #[test]
    fn restore_validates_the_snapshot() {
        let good = stateful(0).snapshot().unwrap();
        assert!(restore(&good, 0, &Limits::default()).is_ok());
        let rejected = |snap: &Snapshot, limits: &Limits, what: &str| match restore(snap, 0, limits) {
            Err(Error::Snapshot { msg }) => assert!(msg.contains(what), "{msg} / {what}"),
            other => panic!("{what}: {other:?}"),
        };
        let with = |f: &dyn Fn(&mut Snapshot)| {
            let mut s = good.clone();
            f(&mut s);
            s
        };
        let limits = Limits::default();
        // Memory: below the minimum, above the maximum, above the limit,
        // not whole pages.
        rejected(&with(&|s| s.memory.clear()), &limits, "smaller");
        rejected(&with(&|s| s.memory = alloc::vec![0; 5 * 65536]), &limits, "exceeds");
        rejected(&with(&|s| s.memory = alloc::vec![0; 2 * 65536]), &Limits { max_memory_pages: 1, ..limits }, "exceeds");
        rejected(&with(&|s| s.memory.push(0)), &limits, "whole pages");
        // Globals: count and type, and a reference that names no function.
        rejected(&with(&|s| s.globals.truncate(2)), &limits, "global count");
        rejected(&with(&|s| s.globals[0] = Value::I64(1)), &limits, "wrong type");
        rejected(&with(&|s| s.globals[1] = Value::FuncRef(Some(99))), &limits, "names no function");
        // Tables: count, length under the minimum and over the maximum,
        // the element budget, entry types, unknown functions.
        rejected(&with(&|s| s.tables.clear()), &limits, "table count");
        rejected(&with(&|s| s.tables[0].truncate(3)), &limits, "table length");
        rejected(&with(&|s| s.tables[0].resize(9, Value::FuncRef(None))), &limits, "table length");
        rejected(
            &with(&|s| s.tables[0].resize(5, Value::FuncRef(None))),
            &Limits { max_table_elements: 4, ..limits },
            "element limit",
        );
        rejected(&with(&|s| s.tables[0][0] = Value::ExternRef(None)), &limits, "entry type");
        rejected(&with(&|s| s.tables[0][0] = Value::I32(0)), &limits, "entry type");
        rejected(&with(&|s| s.tables[0][0] = Value::FuncRef(Some(99))), &limits, "names no function");
        // Segment flags.
        rejected(&with(&|s| s.data_dropped.push(true)), &limits, "data segment count");
        rejected(&with(&|s| s.elem_dropped.truncate(2)), &limits, "element segment count");
        // A module without a memory takes no memory contents.
        let no_mem = module("(module)");
        let snap = Snapshot {
            memory: alloc::vec![0; 65536],
            globals: Vec::new(),
            tables: Vec::new(),
            data_dropped: Vec::new(),
            elem_dropped: Vec::new(),
        };
        assert!(matches!(
            Instance::from_snapshot(no_mem.clone(), &snap, &mut |_, _, _| None, &limits, 0),
            Err(Error::Snapshot { msg }) if msg.contains("without a memory")
        ));
        let empty = Snapshot { memory: Vec::new(), ..snap };
        let inst = Instance::from_snapshot(no_mem, &empty, &mut |_, _, _| None, &limits, 0).unwrap();
        assert_eq!(inst.snapshot().unwrap(), empty);
    }

    /// Blocks the first host call, then answers every call with its
    /// argument.
    #[derive(Default)]
    struct BlockOnce {
        calls: u32,
    }

    impl Host for BlockOnce {
        fn call(
            &mut self,
            _ctx: &mut HostCtx<'_>,
            _func: HostFuncId,
            args: &[Value],
            results: &mut [Value],
        ) -> Result<HostOutcome, Trap> {
            self.calls += 1;
            if self.calls == 1 {
                return Ok(HostOutcome::Block);
            }
            results[0] = args[0];
            Ok(HostOutcome::Continue)
        }
    }

    #[test]
    fn snapshot_mid_execution_restores_the_state_the_execution_left() {
        // `main` sets up state, then blocks in a host call, then returns
        // what `probe` computes from that state.
        let wat = r#"(module
            (type $t (func (result i32)))
            (import "env" "echo" (func $echo (param i32) (result i32)))
            (memory 1)
            (table 2 funcref)
            (global $g (mut i32) (i32.const 0))
            (elem declare func $seven)
            (func $seven (type $t) (i32.const 7))
            (func $probe (export "probe") (result i32)
                (i32.add
                    (i32.add (global.get $g) (i32.load (i32.const 100)))
                    (call_indirect (type $t) (i32.const 1))))
            (func (export "main") (result i32)
                (global.set $g (i32.const 5))
                (i32.store (i32.const 100) (i32.const 30))
                (table.set (i32.const 1) (ref.func $seven))
                (drop (call $echo (i32.const 9)))
                (call $probe)))"#;
        let m = module(wat);
        let mut resolve = |_: &str, _: &str, ty: &ExternType<'_>| match ty {
            ExternType::Func(ft) => Some(Extern::Func { id: HostFuncId(0), ty: (*ft).clone(), addr: None }),
            _ => None,
        };
        let mut a = Instance::instantiate(m.clone(), &mut resolve, &Limits::default(), 0).unwrap();
        let mut host = BlockOnce::default();
        let main = a.export_func("main").unwrap();
        let mut exec = Execution::new(&a, main, &[]).unwrap();
        assert_eq!(exec.run(&mut a, &mut host, 1 << 20), ExecResult::Blocked);
        assert_eq!(host.calls, 1);

        // Mid-execution: the interpreter holds no handles between runs.
        let snap = a.snapshot().unwrap();
        assert_eq!(snap.globals, [Value::I32(5)]);
        assert_eq!(snap.tables[0][1], Value::FuncRef(Some(1)));
        let bytes = snap.to_bytes();
        let mut b =
            Instance::from_snapshot(m, &Snapshot::from_bytes(&bytes).unwrap(), &mut resolve, &Limits::default(), 500).unwrap();
        let probe = b.export_func("probe").unwrap();
        let restored = Execution::new(&b, probe, &[]).unwrap().run(&mut b, &mut host, 1 << 20);
        assert_eq!(host.calls, 1);

        // The original resumes, re-issues the call, and finishes.
        let original = exec.run(&mut a, &mut host, 1 << 20);
        assert_eq!(host.calls, 2);
        assert_eq!(original, ExecResult::Done(vec![Value::I32(42)]));
        assert_eq!(restored, original);
        assert_eq!(b.snapshot().unwrap(), a.snapshot().unwrap());
    }
}
