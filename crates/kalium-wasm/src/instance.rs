//! Instances, instantiation limits and the host interface.
//!
//! [`Instance::new`] resolves function imports through the kernel's
//! resolver, builds the memory, tables and globals, evaluates the element
//! segments and applies the active element and data segments in the order
//! the spec prescribes (elements first, then data, each with bounds checks
//! that fail instantiation with [`Error::Trap`]). The start function is only
//! recorded; the kernel runs it through [`crate::exec::Execution::start`].
//!
//! M1 supports function imports only: imported tables, memories and globals
//! are rejected with [`Error::Unsupported`].

use crate::error::{Error, Trap};
use crate::exec::Value;
use crate::instr::{ConstExpr, Instr};
use crate::memory::Memory;
use crate::module::{DataMode, ElemItems, ElemMode, ImportDesc, Module};
use crate::table::{Table, MAX_TABLE_ELEMENTS};
use crate::types::FuncType;
use alloc::sync::Arc;
use alloc::vec::Vec;
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
#[derive(Debug)]
pub struct HostCtx<'a> {
    pub memory: &'a mut Memory,
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
}

/// Resource limits imposed on an instance by the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Hard cap on linear memory, in 64 KiB pages.
    pub max_memory_pages: u32,
    /// Hard cap on the depth of the value stack (locals included).
    pub max_stack_values: usize,
    /// Hard cap on the depth of the call stack.
    pub max_call_depth: usize,
    /// Hard cap on the number of table elements of the instance, summed over
    /// all its tables (each element is a 16-byte [`Value`]). A module whose
    /// declared table minimums add up to more fails to instantiate with
    /// [`Error::Limit`]; `table.grow` past it returns `-1`.
    pub max_table_elements: u32,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_memory_pages: 256,
            max_stack_values: 64 * 1024,
            max_call_depth: 1024,
            max_table_elements: 65536,
        }
    }
}

/// An instantiated module: its memory, tables, globals and resolved
/// imports. Executions ([`crate::exec::Execution`]) borrow it while they
/// run; several may exist for one instance.
pub struct Instance {
    module: Arc<Module>,
    limits: Limits,
    /// Memory 0, or an empty, non-growable memory if the module has none.
    pub(crate) memory: Memory,
    pub(crate) tables: Vec<Table>,
    pub(crate) globals: Vec<Value>,
    /// Host function id of each imported function, by function index.
    host_funcs: Vec<HostFuncId>,
    /// Type index of every function (imports first), by function index.
    func_types: Vec<u32>,
    /// Evaluated element segments; emptied when dropped (or after an active
    /// segment has been applied).
    pub(crate) elems: Vec<Vec<Value>>,
    /// Which data segments have been dropped (or applied, for active ones).
    pub(crate) data_dropped: Vec<bool>,
}

impl fmt::Debug for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instance")
            .field("memory_pages", &self.memory.size_pages())
            .field("tables", &self.tables.len())
            .field("globals", &self.globals.len())
            .field("imported_funcs", &self.host_funcs.len())
            .finish_non_exhaustive()
    }
}

fn invalid(msg: &'static str) -> Error {
    Error::Invalid {
        msg,
        func: None,
        instr: None,
    }
}

/// Evaluate a constant expression (global initializer, segment offset or
/// element item) against the globals initialized so far.
fn eval_const(module: &Module, globals: &[Value], expr: &ConstExpr) -> Result<Value, Error> {
    let instr = expr
        .single()
        .ok_or_else(|| invalid("constant expression must be a single instruction"))?;
    Ok(match instr {
        Instr::I32Const(v) => Value::I32(v),
        Instr::I64Const(v) => Value::I64(v),
        Instr::F32Const(bits) => Value::F32(f32::from_bits(bits)),
        Instr::F64Const(bits) => Value::F64(f64::from_bits(bits)),
        Instr::RefNull(t) => Value::null(t),
        Instr::RefFunc(f) => {
            if (f as usize) >= module.num_funcs() {
                return Err(invalid("ref.func in constant expression: unknown function"));
            }
            Value::FuncRef(Some(f))
        }
        Instr::GlobalGet(g) => globals
            .get(g as usize)
            .copied()
            .ok_or_else(|| invalid("global.get in constant expression: unknown global"))?,
        _ => return Err(invalid("non-constant instruction in constant expression")),
    })
}

fn const_i32(module: &Module, globals: &[Value], expr: &ConstExpr) -> Result<u32, Error> {
    match eval_const(module, globals, expr)? {
        Value::I32(v) => Ok(v as u32),
        _ => Err(invalid("segment offset must be an i32")),
    }
}

impl Instance {
    /// Instantiate `module`, resolving each function import through
    /// `resolve(module_name, field_name, type)`; `Err(())` from `resolve` is
    /// a link error (`Error::Link`). Active segments are applied here; an
    /// out-of-bounds segment fails with `Error::Trap`. The start function
    /// is *not* run; see `Execution::start`.
    pub fn new(
        module: Arc<Module>,
        resolve: &mut dyn FnMut(&str, &str, &FuncType) -> Result<HostFuncId, ()>,
        limits: &Limits,
    ) -> Result<Instance, Error> {
        // Imports: functions only in M1.
        let mut host_funcs = Vec::new();
        let mut func_types = Vec::new();
        for (i, imp) in module.imports.iter().enumerate() {
            match &imp.desc {
                ImportDesc::Func(ty) => {
                    let ft = module
                        .get_type(*ty)
                        .ok_or_else(|| invalid("import type index out of range"))?;
                    let id = resolve(&imp.module, &imp.name, ft)
                        .map_err(|()| Error::Link { import: i as u32 })?;
                    host_funcs.push(id);
                    func_types.push(*ty);
                }
                ImportDesc::Table(_) => {
                    return Err(Error::Unsupported {
                        offset: 0,
                        msg: "imported tables are not supported",
                    })
                }
                ImportDesc::Memory(_) => {
                    return Err(Error::Unsupported {
                        offset: 0,
                        msg: "imported memories are not supported",
                    })
                }
                ImportDesc::Global(_) => {
                    return Err(Error::Unsupported {
                        offset: 0,
                        msg: "imported globals are not supported",
                    })
                }
            }
        }
        if module.funcs.len() != module.code.len() {
            return Err(invalid("function and code section lengths differ"));
        }
        for &ty in &module.funcs {
            if module.get_type(ty).is_none() {
                return Err(invalid("function type index out of range"));
            }
            func_types.push(ty);
        }

        // Globals, in order; initializers may read earlier globals.
        let mut globals: Vec<Value> = Vec::with_capacity(module.globals.len());
        for g in &module.globals {
            let v = eval_const(&module, &globals, &g.init)?;
            if v.ty() != g.ty.ty {
                return Err(invalid("global initializer has the wrong type"));
            }
            globals.push(v);
        }

        // Tables. Each one is bounded by the smaller of the per-table cap and
        // the instance's element budget, and the declared minimums together
        // must fit the budget before anything is allocated.
        let per_table = MAX_TABLE_ELEMENTS.min(limits.max_table_elements);
        let mut declared: u32 = 0;
        for t in &module.tables {
            declared = declared
                .checked_add(t.limits.min)
                .filter(|&n| n <= limits.max_table_elements)
                .ok_or(Error::Limit {
                    msg: "declared table sizes exceed the instance's table element limit",
                })?;
        }
        let mut tables = Vec::with_capacity(module.tables.len());
        for t in &module.tables {
            let table = Table::new(*t, per_table).ok_or(Error::Limit {
                msg: "table size exceeds limit",
            })?;
            tables.push(table);
        }

        // Memory 0 (an empty, non-growable one when the module has none).
        if module.memories.len() > 1 {
            return Err(Error::Unsupported {
                offset: 0,
                msg: "multiple memories are not supported",
            });
        }
        let memory = match module.memories.first() {
            Some(m) => {
                let max = m
                    .limits
                    .max
                    .map_or(limits.max_memory_pages, |x| x.min(limits.max_memory_pages));
                if m.limits.min > max {
                    return Err(Error::Limit {
                        msg: "memory size exceeds limit",
                    });
                }
                Memory::new(m.limits.min, max).ok_or(Error::OutOfMemory)?
            }
            None => Memory::new(0, 0).ok_or(Error::OutOfMemory)?,
        };

        // Element segments are evaluated eagerly into element instances.
        let mut elems = Vec::with_capacity(module.elements.len());
        for e in &module.elements {
            let items: Vec<Value> = match &e.items {
                ElemItems::Funcs(fs) => {
                    for &f in fs {
                        if (f as usize) >= func_types.len() {
                            return Err(invalid("element segment: unknown function"));
                        }
                    }
                    fs.iter().map(|&f| Value::FuncRef(Some(f))).collect()
                }
                ElemItems::Exprs(exprs) => {
                    let mut v = Vec::with_capacity(exprs.len());
                    for ex in exprs {
                        let val = eval_const(&module, &globals, ex)?;
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

        let mut inst = Instance {
            limits: *limits,
            memory,
            tables,
            globals,
            host_funcs,
            func_types,
            elems,
            data_dropped: alloc::vec![false; module.data.len()],
            module,
        };

        // Active element segments: `table.init` then `elem.drop`, in order.
        // Declarative segments are dropped immediately.
        for (i, e) in inst.module.elements.iter().enumerate() {
            match &e.mode {
                ElemMode::Active { table, offset } => {
                    let dst = const_i32(&inst.module, &inst.globals, offset)?;
                    let seg = &inst.elems[i];
                    let n = seg.len() as u32;
                    inst.tables
                        .get_mut(*table as usize)
                        .ok_or_else(|| invalid("element segment: unknown table"))?
                        .init(dst, seg, 0, n)
                        .map_err(Error::Trap)?;
                    inst.elems[i] = Vec::new();
                }
                ElemMode::Declarative => inst.elems[i] = Vec::new(),
                ElemMode::Passive => {}
            }
        }

        // Active data segments: `memory.init` then `data.drop`, in order.
        for (i, d) in inst.module.data.iter().enumerate() {
            if let DataMode::Active { memory, offset } = &d.mode {
                if *memory != 0 || inst.module.memories.is_empty() {
                    return Err(invalid("data segment: unknown memory"));
                }
                let dst = const_i32(&inst.module, &inst.globals, offset)?;
                inst.memory.write(dst, &d.bytes).map_err(Error::Trap)?;
                inst.data_dropped[i] = true;
            }
        }

        Ok(inst)
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

    /// The module's start function, if it declares one. The kernel runs it
    /// first through `Execution::start`.
    pub fn start_function(&self) -> Option<u32> {
        self.module.start
    }

    /// Linear memory 0 (empty if the module declares none).
    pub fn memory(&self) -> &Memory {
        &self.memory
    }

    /// Mutable access to linear memory 0.
    pub fn memory_mut(&mut self) -> &mut Memory {
        &mut self.memory
    }

    /// Table `idx`, if it exists.
    pub fn table(&self, idx: u32) -> Option<&Table> {
        self.tables.get(idx as usize)
    }

    /// Number of table elements currently in use, summed over all tables
    /// (never more than [`Limits::max_table_elements`]).
    pub fn table_elements(&self) -> u32 {
        self.tables
            .iter()
            .fold(0u32, |n, t| n.saturating_add(t.size()))
    }

    /// `table.grow` on table `idx`: grow by `delta` entries set to `init`.
    /// Returns the old size, or `None` (which Wasm sees as `-1`, the spec's
    /// failed grow) when the table's own bound or the instance's element
    /// budget ([`Limits::max_table_elements`]) would be exceeded.
    /// `Trap::Invalid` names a table that does not exist.
    pub(crate) fn grow_table(
        &mut self,
        idx: u32,
        delta: u32,
        init: Value,
    ) -> Result<Option<u32>, Trap> {
        let in_use = self.table_elements();
        let table = self.tables.get_mut(idx as usize).ok_or(Trap::Invalid)?;
        match in_use.checked_add(delta) {
            Some(total) if total <= self.limits.max_table_elements => Ok(table.grow(delta, init)),
            _ => Ok(None),
        }
    }

    /// The current value of global `idx`, if it exists.
    pub fn global(&self, idx: u32) -> Option<Value> {
        self.globals.get(idx as usize).copied()
    }

    /// Set global `idx`. Fails if the index is unknown or the value has the
    /// wrong type (mutability is not checked: this is the host's door).
    pub fn set_global(&mut self, idx: u32, value: Value) -> Result<(), Error> {
        let slot = self
            .globals
            .get_mut(idx as usize)
            .ok_or_else(|| invalid("unknown global"))?;
        if slot.ty() != value.ty() {
            return Err(Error::ArgumentMismatch);
        }
        *slot = value;
        Ok(())
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

    /// The type index of function `func` (function index space).
    pub fn func_type_index(&self, func: u32) -> Option<u32> {
        self.func_types.get(func as usize).copied()
    }

    /// The type of function `func` (function index space).
    pub fn func_type(&self, func: u32) -> Option<&FuncType> {
        self.module.get_type(self.func_type_index(func)?)
    }
}
