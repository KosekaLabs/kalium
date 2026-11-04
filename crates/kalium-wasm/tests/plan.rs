//! The binding contract in `docs/prototype-plan.md` ("code that disagrees
//! with this document is wrong until the document is changed") declares the
//! public API of this crate. This test keeps the two from drifting apart the
//! way they did once (the plan kept `HostCtx { memory: &mut Memory }` after
//! the code had moved to `&MemoryRef`): every public item the kernel or the
//! spec runner links against must be spelled out in the plan's kalium-wasm
//! block, and the plan must not describe the old API.
//!
//! The strings are the plan's wording of the item, not a parse of the
//! crate: when the API changes, change the plan and then this list.

use std::fs;
use std::path::Path;

fn plan() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/prototype-plan.md");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The kalium-wasm section of the plan: from its heading to the next crate's.
fn kalium_wasm_block(plan: &str) -> &str {
    let start = plan
        .find("### kalium-wasm")
        .expect("the plan has a `### kalium-wasm` section");
    let rest = &plan[start..];
    let end = rest[4..].find("\n### ").map_or(rest.len(), |i| i + 4);
    &rest[..end]
}

#[test]
fn plan_declares_the_current_api() {
    let plan = plan();
    let block = kalium_wasm_block(&plan);
    // The exact spellings the plan uses, each of which names a public item
    // (or its contract) that exists in the crate as of this test.
    let required = [
        // The host interface and the shared-memory discipline.
        "pub struct HostCtx<'a> { pub memory: &'a MemoryRef }",
        "pub type MemoryRef = Rc<RefCell<Memory>>;",
        "pub type TableRef = Rc<RefCell<Table>>;",
        "pub type GlobalRef = Rc<global::Global>;",
        "fn call_foreign(&mut self, ctx: &mut HostCtx<'_>, addr: u32, ty: &FuncType, args: &[Value], results: &mut [Value]) -> Result<HostOutcome, Trap>;",
        "Default: `Err(Trap::Host)`",
        // Function addresses.
        "FuncRef(Option<u32> /* function address */)",
        "pub fn func_addr(&self, func: u32) -> Option<u32>;",
        "pub fn local_func(&self, addr: u32) -> Option<u32>;",
        // General linking.
        "pub enum ExternType<'a> { Func(&'a FuncType), Table(TableType), Memory(MemType), Global(GlobalType) }",
        "Func { id: HostFuncId, ty: FuncType, addr: Option<u32> },",
        "Table(TableRef), Memory(MemoryRef), Global(GlobalRef),",
        "pub fn instantiate(module: Arc<Module>, resolve: &mut dyn FnMut(&str, &str, &ExternType<'_>) -> Option<Extern>, limits: &Limits, func_base: u32) -> Result<Instance, Error>;",
        "pub fn allocate(module: Arc<Module>, resolve: &mut dyn FnMut(&str, &str, &ExternType<'_>) -> Option<Extern>, limits: &Limits, func_base: u32) -> Result<Instance, Error>;",
        "pub fn init_segments(&mut self) -> Result<(), Trap>;",
        "pub fn export_table(&self, name: &str) -> Option<TableRef>;",
        "pub fn export_memory(&self, name: &str) -> Option<MemoryRef>;",
        "pub fn export_global(&self, name: &str) -> Option<GlobalRef>;",
        "IncompatibleImport { import: u32 }",
        // The kernel's front end and what it reports for non-function imports.
        "pub fn new(module: Arc<Module>, resolve: &mut dyn FnMut(&str, &str, &FuncType) -> Result<HostFuncId, ()>, limits: &Limits) -> Result<Instance, Error>;",
        "as is any table, memory or global import",
        // The embedder's obligation the range check enforces.
        "An `addr` inside the importer's own range is `Error::IncompatibleImport`",
        // Fast start: the shared program and snapshots.
        "pub program: ProgramCache",
        "pub fn snapshot(&self) -> Result<Snapshot, Error>;",
        "pub fn from_snapshot(module: Arc<Module>, snapshot: &Snapshot, resolve: &mut dyn FnMut(&str, &str, &ExternType<'_>) -> Option<Extern>, limits: &Limits, func_base: u32) -> Result<Instance, Error>;",
        "pub fn to_bytes(&self) -> Vec<u8>;",
        "pub fn from_bytes(bytes: &[u8]) -> Result<Snapshot, Error>;",
        "pub fn memory_pages(&self) -> u32; pub fn memory(&self) -> &[u8];",
        "Snapshot { msg }",
        "`Rc::strong_count == 1`",
    ];
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|s| !block.contains(s))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/prototype-plan.md's kalium-wasm block does not declare:\n  {}",
        missing.join("\n  ")
    );
    // The pre-shared-objects API must be gone, not merely joined by the new one.
    for stale in ["pub memory: &'a mut Memory", "FuncRef(Option<u32>) }"] {
        assert!(
            !block.contains(stale),
            "docs/prototype-plan.md still describes the old API: `{stale}`"
        );
    }
}

#[test]
fn plan_items_exist_in_the_crate() {
    // What the plan spells out must compile against the crate: a type-level
    // check of the signatures the strings above name. Nothing is run.
    use kalium_wasm::{
        Error, Extern, ExternType, GlobalRef, Host, HostCtx, HostFuncId, HostOutcome, Instance,
        Limits, MemoryRef, Module, ProgramCache, Snapshot, TableRef, Trap, Value,
    };
    use std::sync::Arc;
    /// The signature the plan gives `Instance::instantiate` and `Instance::allocate`.
    type Instantiate = fn(
        Arc<Module>,
        &mut dyn FnMut(&str, &str, &ExternType<'_>) -> Option<Extern>,
        &Limits,
        u32,
    ) -> Result<Instance, Error>;
    let _: Instantiate = Instance::instantiate;
    let _: Instantiate = Instance::allocate;
    let _: fn(&mut Instance) -> Result<(), Trap> = Instance::init_segments;
    let _: fn(&Instance, &str) -> Option<TableRef> = Instance::export_table;
    let _: fn(&Instance, &str) -> Option<MemoryRef> = Instance::export_memory;
    let _: fn(&Instance, &str) -> Option<GlobalRef> = Instance::export_global;
    let _: fn(&Instance, u32) -> Option<u32> = Instance::func_addr;
    let _: fn(&Instance, u32) -> Option<u32> = Instance::local_func;
    let _: fn(&Instance) -> Result<Snapshot, Error> = Instance::snapshot;
    /// The signature the plan gives `Instance::from_snapshot`.
    type FromSnapshot = fn(
        Arc<Module>,
        &Snapshot,
        &mut dyn FnMut(&str, &str, &ExternType<'_>) -> Option<Extern>,
        &Limits,
        u32,
    ) -> Result<Instance, Error>;
    let _: FromSnapshot = Instance::from_snapshot;
    let _: fn(&Snapshot) -> Vec<u8> = Snapshot::to_bytes;
    let _: fn(&[u8]) -> Result<Snapshot, Error> = Snapshot::from_bytes;
    let _: fn(&Snapshot) -> u32 = Snapshot::memory_pages;
    let _: fn(&Snapshot) -> &[u8] = Snapshot::memory;
    let _ = Error::Snapshot { msg: "" };
    fn _program(m: &Module) -> &ProgramCache {
        &m.program
    }
    fn _call_foreign(
        host: &mut dyn Host,
        ctx: &mut HostCtx<'_>,
        addr: u32,
        ty: &kalium_wasm::FuncType,
        args: &[Value],
        results: &mut [Value],
    ) -> Result<HostOutcome, Trap> {
        host.call_foreign(ctx, addr, ty, args, results)
    }
    fn _memory<'a>(ctx: &HostCtx<'a>) -> &'a MemoryRef {
        ctx.memory
    }
    let _ = Error::IncompatibleImport { import: 0 };
    let _ = Extern::Func {
        id: HostFuncId(0),
        ty: kalium_wasm::FuncType::default(),
        addr: None,
    };
    let _ = Value::FuncRef(None);
}
