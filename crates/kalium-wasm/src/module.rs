//! The decoded module: every section as plain data, plus accessors over the
//! index spaces (imports first, then definitions).

use crate::code::Program;
use crate::decode;
use crate::error::Error;
use crate::instr::{ConstExpr, FuncBody};
use crate::types::{FuncType, GlobalType, MemType, RefType, TableType};
use crate::validate;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

/// What an import provides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportDesc {
    /// A function with the given type index.
    Func(u32),
    Table(TableType),
    Memory(MemType),
    Global(GlobalType),
}

/// An import: `module.name` of the given kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub desc: ImportDesc,
}

/// What an export refers to (an index into the respective index space,
/// imports included).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportDesc {
    Func(u32),
    Table(u32),
    Memory(u32),
    Global(u32),
}

/// An export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    pub desc: ExportDesc,
}

/// A global defined by the module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Global {
    pub ty: GlobalType,
    pub init: ConstExpr,
}

/// How an element segment is used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ElemMode {
    /// Available to `table.init` until dropped.
    Passive,
    /// Only forward-declares `ref.func` targets; never instantiated.
    Declarative,
    /// Copied into `table` at `offset` on instantiation.
    Active { table: u32, offset: ConstExpr },
}

/// The items of an element segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ElemItems {
    /// Function indices (`funcref` only).
    Funcs(Vec<u32>),
    /// Constant expressions evaluating to references.
    Exprs(Vec<ConstExpr>),
}

/// An element segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    pub ty: RefType,
    pub items: ElemItems,
    pub mode: ElemMode,
}

impl Element {
    /// Number of items in the segment.
    pub fn len(&self) -> usize {
        match &self.items {
            ElemItems::Funcs(v) => v.len(),
            ElemItems::Exprs(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How a data segment is used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataMode {
    /// Available to `memory.init` until dropped.
    Passive,
    /// Copied into `memory` at `offset` on instantiation.
    Active { memory: u32, offset: ConstExpr },
}

/// A data segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Data {
    pub mode: DataMode,
    pub bytes: Vec<u8>,
}

/// The compiled form of a module (the interpreter's private code format),
/// built once and shared by every instance of the module.
///
/// [`Module::new`] fills it, so that instantiating a decoded module never
/// compiles: the instance takes a reference to the shared program. A
/// `Module` built any other way (a struct literal, [`crate::decode::decode`])
/// has an empty cache, `ProgramCache::default()`, and each of its instances
/// compiles its own copy. The cache is derived data, not part of the
/// module's identity: it does not take part in `PartialEq`.
#[derive(Clone, Default)]
pub struct ProgramCache(Option<Arc<Program>>);

impl ProgramCache {
    /// The cached program, if the module has been compiled.
    pub(crate) fn get(&self) -> Option<&Arc<Program>> {
        self.0.as_ref()
    }

    /// Whether the cache is filled.
    pub fn is_compiled(&self) -> bool {
        self.0.is_some()
    }
}

impl fmt::Debug for ProgramCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_some() {
            "ProgramCache(compiled)"
        } else {
            "ProgramCache(empty)"
        })
    }
}

impl PartialEq for ProgramCache {
    /// Always equal: the cache is derived from the other fields.
    fn eq(&self, _other: &ProgramCache) -> bool {
        true
    }
}

/// A decoded (and, when built through [`Module::new`], validated) module.
///
/// All fields are public so the validator and interpreter can walk them
/// directly; the accessor methods handle the index spaces, which include
/// imports before definitions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Module {
    /// Type section.
    pub types: Vec<FuncType>,
    /// Import section.
    pub imports: Vec<Import>,
    /// Function section: the type index of each *defined* function, in
    /// order. `funcs[i]` describes function index `num_imported_funcs() + i`.
    pub funcs: Vec<u32>,
    /// Table section (defined tables only).
    pub tables: Vec<TableType>,
    /// Memory section (defined memories only).
    pub memories: Vec<MemType>,
    /// Global section (defined globals only).
    pub globals: Vec<Global>,
    /// Export section.
    pub exports: Vec<Export>,
    /// Start section.
    pub start: Option<u32>,
    /// Element section.
    pub elements: Vec<Element>,
    /// Data section.
    pub data: Vec<Data>,
    /// Data count section, if present.
    pub data_count: Option<u32>,
    /// Code section: one body per entry of `funcs`.
    pub code: Vec<FuncBody>,
    /// The compiled code, shared by every instance of this module (see
    /// [`ProgramCache`]); filled by [`Module::new`].
    pub program: ProgramCache,
}

impl Module {
    /// Decode, validate and compile. Never panics on malformed input.
    pub fn new(bytes: &[u8]) -> Result<Module, Error> {
        let mut module = decode::decode(bytes)?;
        validate::validate(&module)?;
        module.program = ProgramCache(Some(module.compile()));
        Ok(module)
    }

    /// Compile the module's functions (see [`crate::code`]); never fails.
    /// [`Module::new`] stores the result in [`Module::program`];
    /// instantiation uses that copy when there is one.
    pub(crate) fn compile(&self) -> Arc<Program> {
        let imported = self.imports.iter().filter_map(|i| match i.desc {
            ImportDesc::Func(ty) => Some(ty),
            _ => None,
        });
        let num_imports = self.num_imported_funcs();
        let arity: Vec<(u32, u32)> = imported
            .chain(self.funcs.iter().copied())
            .map(|ty| {
                self.get_type(ty).map_or((0, 0), |ft| {
                    (ft.params.len() as u32, ft.results.len() as u32)
                })
            })
            .collect();
        Arc::new(crate::code::compile_module(self, &arity, num_imports))
    }

    /// The compiled code: the cached program, or a fresh compilation for a
    /// module that was not built through [`Module::new`].
    pub(crate) fn program(&self) -> Arc<Program> {
        match self.program.get() {
            Some(p) => p.clone(),
            None => self.compile(),
        }
    }

    /// The imports, in order.
    pub fn imports(&self) -> &[Import] {
        &self.imports
    }

    /// The exports, in order.
    pub fn exports(&self) -> &[Export] {
        &self.exports
    }

    /// The function type at type index `idx`.
    ///
    /// # Panics
    ///
    /// Panics if `idx` is not a valid type index. Validation guarantees that
    /// every type index stored in a module is valid, so this is only a
    /// programming error on the caller's side; use [`Module::get_type`] for
    /// a fallible lookup.
    pub fn func_type(&self, idx: u32) -> &FuncType {
        match self.types.get(idx as usize) {
            Some(t) => t,
            None => panic!("type index {idx} out of range"),
        }
    }

    /// The function type at type index `idx`, if valid.
    pub fn get_type(&self, idx: u32) -> Option<&FuncType> {
        self.types.get(idx as usize)
    }

    /// Number of imported functions (they occupy the first function
    /// indices).
    pub fn num_imported_funcs(&self) -> usize {
        self.imports
            .iter()
            .filter(|i| matches!(i.desc, ImportDesc::Func(_)))
            .count()
    }

    /// Number of imported tables.
    pub fn num_imported_tables(&self) -> usize {
        self.imports
            .iter()
            .filter(|i| matches!(i.desc, ImportDesc::Table(_)))
            .count()
    }

    /// Number of imported memories.
    pub fn num_imported_memories(&self) -> usize {
        self.imports
            .iter()
            .filter(|i| matches!(i.desc, ImportDesc::Memory(_)))
            .count()
    }

    /// Number of imported globals.
    pub fn num_imported_globals(&self) -> usize {
        self.imports
            .iter()
            .filter(|i| matches!(i.desc, ImportDesc::Global(_)))
            .count()
    }

    /// Total size of the function index space (imports + definitions).
    pub fn num_funcs(&self) -> usize {
        self.num_imported_funcs() + self.funcs.len()
    }

    /// Total size of the table index space.
    pub fn num_tables(&self) -> usize {
        self.num_imported_tables() + self.tables.len()
    }

    /// Total size of the memory index space.
    pub fn num_memories(&self) -> usize {
        self.num_imported_memories() + self.memories.len()
    }

    /// Total size of the global index space.
    pub fn num_globals(&self) -> usize {
        self.num_imported_globals() + self.globals.len()
    }

    /// The type index of function `func_idx` (function index space), if
    /// the index is valid.
    pub fn func_type_index(&self, func_idx: u32) -> Option<u32> {
        let mut remaining = func_idx as usize;
        for imp in &self.imports {
            if let ImportDesc::Func(ty) = imp.desc {
                if remaining == 0 {
                    return Some(ty);
                }
                remaining -= 1;
            }
        }
        self.funcs.get(remaining).copied()
    }

    /// The type of function `func_idx` (function index space), if the index
    /// and its type index are valid.
    pub fn func_type_of(&self, func_idx: u32) -> Option<&FuncType> {
        self.get_type(self.func_type_index(func_idx)?)
    }

    /// The body of function `func_idx` (function index space); `None` for
    /// imports and invalid indices.
    pub fn func_body(&self, func_idx: u32) -> Option<&FuncBody> {
        let defined = (func_idx as usize).checked_sub(self.num_imported_funcs())?;
        self.code.get(defined)
    }

    /// The type of table `idx` (table index space).
    pub fn table_type(&self, idx: u32) -> Option<TableType> {
        let mut remaining = idx as usize;
        for imp in &self.imports {
            if let ImportDesc::Table(t) = imp.desc {
                if remaining == 0 {
                    return Some(t);
                }
                remaining -= 1;
            }
        }
        self.tables.get(remaining).copied()
    }

    /// The type of memory `idx` (memory index space).
    pub fn memory_type(&self, idx: u32) -> Option<MemType> {
        let mut remaining = idx as usize;
        for imp in &self.imports {
            if let ImportDesc::Memory(m) = imp.desc {
                if remaining == 0 {
                    return Some(m);
                }
                remaining -= 1;
            }
        }
        self.memories.get(remaining).copied()
    }

    /// The type of global `idx` (global index space).
    pub fn global_type(&self, idx: u32) -> Option<GlobalType> {
        let mut remaining = idx as usize;
        for imp in &self.imports {
            if let ImportDesc::Global(g) = imp.desc {
                if remaining == 0 {
                    return Some(g);
                }
                remaining -= 1;
            }
        }
        self.globals.get(remaining).map(|g| g.ty)
    }

    /// The export named `name`, if any.
    pub fn export(&self, name: &str) -> Option<&Export> {
        self.exports.iter().find(|e| e.name == name)
    }

    /// The function index exported under `name`, if any.
    pub fn export_func(&self, name: &str) -> Option<u32> {
        match self.export(name)?.desc {
            ExportDesc::Func(idx) => Some(idx),
            _ => None,
        }
    }
}
