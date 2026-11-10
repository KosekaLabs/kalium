//! Runs the execution part of the official WebAssembly spec test suite
//! (release 2.0, vendored under `tests/spec/`, see `tests/spec/README.md`)
//! through the interpreter.
//!
//! For every `.wast` script, in order:
//!
//! - `(module ...)` must decode, validate, instantiate and run its start
//!   function; it becomes the current module and may be named with `$id`;
//! - `(register "name" $id?)` makes a module's exports importable by later
//!   modules: functions as cross-instance calls through the `Host` trait
//!   (exactly as the kernel would route them), tables, memories and
//!   globals as the shared objects themselves, so that an importer and the
//!   exporter see one table, one memory, one global;
//! - `(invoke ...)` must complete without trapping;
//! - `(assert_return (invoke ...) results)` and `(assert_return (get ...))`
//!   must produce exactly the expected values (NaN patterns included);
//! - `(assert_trap (invoke ...) msg)` must trap with the spec's message, and
//!   `(assert_trap (module ...) msg)` must fail instantiation with it;
//! - `(assert_exhaustion ...)` must trap with the call-depth or value-stack
//!   limit;
//! - `(assert_unlinkable (module ...) msg)` must fail to link with the
//!   spec's message ("unknown import" or "incompatible import type");
//! - `(assert_malformed ...)` and `(assert_invalid ...)` must be rejected
//!   (`tests/spec_validate.rs` also checks the messages).
//!
//! The `spectest` host module is provided in full: the `print*` host
//! functions, `table` (10 20 funcref), `memory` (1 2) and the four
//! `global_*` globals, created by the host and shared with every module
//! that imports them (as the kernel will share memory objects with
//! instances). A `call_indirect` through a table entry another instance
//! stored (a *foreign* function address, see `kalium_wasm::instance`) is
//! served through `Host::call_foreign`, which finds the owning instance by
//! its address range.
//!
//! Every directive is executed: there is no skip mechanism, and the test
//! asserts that the number of directives equals the number of recorded
//! outcomes (passes and failures), so nothing can fall through silently.

use kalium_wasm::global::Global;
use kalium_wasm::{
    Error, ExecResult, Execution, ExportDesc, Extern, ExternType, FuncType, GlobalRef, GlobalType, Host, HostCtx, HostFuncId,
    HostOutcome, Instance, Limits, MAX_TABLE_ELEMENTS, MemType, Memory, MemoryRef, Module, RefType, ResizableLimits, Table,
    TableRef, TableType, Trap, ValType, Value,
};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wast::core::{AbstractHeapType, HeapType, NanPattern, WastArgCore, WastRetCore};
use wast::lexer::Lexer;
use wast::parser::{self, ParseBuffer};
use wast::token::{Id, Index, Span};
use wast::{QuoteWat, Wast, WastArg, WastDirective, WastExecute, WastInvoke, WastRet, Wat};

/// Generous limits: the suite grows memories to 803 pages and recurses until
/// `assert_exhaustion` fires.
const LIMITS: Limits =
    Limits { max_memory_pages: 65536, max_stack_values: 1 << 20, max_call_depth: 10_000, max_table_elements: 1 << 20 };

/// Size of each instance's function address range: instance `i` of the
/// store owns addresses `i * FUNC_ADDR_SPACE ..`, so an address maps back to
/// its instance by division. No spec module has a million functions.
const FUNC_ADDR_SPACE: u32 = 1 << 20;

// ---------------------------------------------------------------------------
// The store: instances, names and the host
// ---------------------------------------------------------------------------

/// A resolved host function.
#[derive(Clone, Copy, Debug)]
enum HostFn {
    /// One of the `spectest.print*` functions.
    Print,
    /// A function exported by another instance in the store.
    Cross { instance: usize, func: u32 },
}

/// The host-provided objects of the `spectest` module.
struct SpecTest {
    table: TableRef,
    memory: MemoryRef,
    globals: Vec<(&'static str, GlobalRef)>,
}

fn spectest_type(name: &str) -> Option<FuncType> {
    use ValType::*;
    let (params, results): (Vec<ValType>, Vec<ValType>) = match name {
        "print" => (vec![], vec![]),
        "print_i32" => (vec![I32], vec![]),
        "print_i64" => (vec![I64], vec![]),
        "print_f32" => (vec![F32], vec![]),
        "print_f64" => (vec![F64], vec![]),
        "print_i32_f32" => (vec![I32, F32], vec![]),
        "print_f64_f64" => (vec![F64, F64], vec![]),
        _ => return None,
    };
    Some(FuncType::new(params, results))
}

impl SpecTest {
    fn new() -> SpecTest {
        let table = Table::new(
            TableType { elem: RefType::FuncRef, limits: ResizableLimits { min: 10, max: Some(20) } },
            MAX_TABLE_ELEMENTS,
        )
        .expect("spectest table")
        .into_shared();
        let memory = Memory::with_type(MemType { limits: ResizableLimits { min: 1, max: Some(2) } }, LIMITS.max_memory_pages)
            .expect("spectest memory")
            .into_shared();
        let global = |name, value: Value| {
            let ty = GlobalType { ty: value.ty(), mutable: false };
            (name, Global::new(ty, value).expect("typed").into_shared())
        };
        SpecTest {
            table,
            memory,
            globals: vec![
                global("global_i32", Value::I32(666)),
                global("global_i64", Value::I64(666)),
                global("global_f32", Value::F32(666.6)),
                global("global_f64", Value::F64(666.6)),
            ],
        }
    }

    /// Resolve `spectest.name`; `None` for an unknown name.
    fn lookup(&self, name: &str, host_funcs: &mut Vec<HostFn>) -> Option<Extern> {
        if name == "table" {
            return Some(Extern::Table(self.table.clone()));
        }
        if name == "memory" {
            return Some(Extern::Memory(self.memory.clone()));
        }
        if let Some((_, g)) = self.globals.iter().find(|(n, _)| *n == name) {
            return Some(Extern::Global(g.clone()));
        }
        let ty = spectest_type(name)?;
        let id = HostFuncId(host_funcs.len() as u32);
        host_funcs.push(HostFn::Print);
        Some(Extern::Func { id, ty, addr: None })
    }
}

struct Store {
    /// Every instance created in this script, in creation order (the index
    /// is the instance's function address range). `None` while that
    /// instance is running (it is lent to the interpreter).
    instances: Vec<Option<Instance>>,
    /// `$id` of a module directive -> instance.
    by_id: HashMap<String, usize>,
    /// `(register "name")` -> instance.
    registered: HashMap<String, usize>,
    current: Option<usize>,
    host_funcs: Vec<HostFn>,
    spectest: SpecTest,
}

impl Store {
    fn new() -> Store {
        Store {
            instances: Vec::new(),
            by_id: HashMap::new(),
            registered: HashMap::new(),
            current: None,
            host_funcs: Vec::new(),
            spectest: SpecTest::new(),
        }
    }

    fn instance_of(&self, id: Option<Id<'_>>) -> Result<usize, String> {
        match id {
            Some(id) => self.by_id.get(id.name()).copied().ok_or_else(|| format!("unknown module ${}", id.name())),
            None => self.current.ok_or_else(|| "no current module".to_owned()),
        }
    }

    /// Decode, validate, instantiate and run the start function. The
    /// instance is kept in the store even when a segment or the start
    /// function traps: it exists, and its active segments may already have
    /// put its functions into shared tables (linking.wast relies on that).
    fn instantiate(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        let module = Arc::new(Module::new(bytes)?);
        let idx = self.instances.len();
        let base =
            u32::try_from(idx).ok().and_then(|i| i.checked_mul(FUNC_ADDR_SPACE)).expect("fewer than 4096 instances per script");
        let instance = {
            let instances = &self.instances;
            let registered = &self.registered;
            let host_funcs = &mut self.host_funcs;
            let spectest = &self.spectest;
            let mut resolve = |m: &str, n: &str, _ty: &ExternType<'_>| -> Option<Extern> {
                if m == "spectest" {
                    return spectest.lookup(n, host_funcs);
                }
                let target_idx = *registered.get(m)?;
                let target = instances[target_idx].as_ref()?;
                Some(match target.module().export(n)?.desc {
                    ExportDesc::Func(func) => {
                        let id = HostFuncId(host_funcs.len() as u32);
                        host_funcs.push(HostFn::Cross { instance: target_idx, func });
                        Extern::Func { id, ty: target.func_type(func)?.clone(), addr: target.func_addr(func) }
                    }
                    ExportDesc::Table(t) => Extern::Table(target.table_ref(t)?.clone()),
                    ExportDesc::Memory(_) => Extern::Memory(target.memory_ref().clone()),
                    ExportDesc::Global(g) => Extern::Global(target.global_ref(g)?.clone()),
                })
            };
            Instance::allocate(module, &mut resolve, &LIMITS, base)?
        };
        self.instances.push(Some(instance));
        let inst = self.instances[idx].as_mut().expect("just stored");
        inst.init_segments().map_err(Error::Trap)?;

        // The start function.
        let start = Execution::start(inst)?;
        if let Some(mut start) = start {
            let mut inst = self.instances[idx].take().expect("just stored");
            let r = start.run(&mut inst, self, u64::MAX);
            self.instances[idx] = Some(inst);
            match r {
                ExecResult::Done(_) => {}
                ExecResult::Trap(t) => return Err(Error::Trap(t)),
                ExecResult::OutOfFuel => return Err(Error::Trap(Trap::OutOfFuel)),
                ExecResult::Blocked | ExecResult::Exited(_) => return Err(Error::Trap(Trap::Host)),
            }
        }
        Ok(idx)
    }

    /// Invoke an export of an instance.
    fn invoke(&mut self, idx: usize, name: &str, args: &[Value]) -> Result<ExecResult, String> {
        let mut inst = self.instances[idx].take().ok_or_else(|| "instance is already running".to_owned())?;
        let result = (|| {
            let func = inst.export_func(name).ok_or_else(|| format!("no exported function \"{name}\""))?;
            let mut exec = Execution::new(&inst, func, args).map_err(|e| format!("cannot call \"{name}\": {e}"))?;
            Ok(exec.run(&mut inst, self, u64::MAX))
        })();
        self.instances[idx] = Some(inst);
        result
    }

    /// Read an exported global.
    fn get_global(&self, idx: usize, name: &str) -> Result<Value, String> {
        let inst = self.instances[idx].as_ref().ok_or_else(|| "instance is already running".to_owned())?;
        inst.export_global(name).map(|g| g.get()).ok_or_else(|| format!("no exported global \"{name}\""))
    }

    /// Run function `func` of instance `instance` to completion on behalf of
    /// a host call, filling `results`. The instance is lent to the nested
    /// interpreter for the duration of the call; an instance that is
    /// already running (a cross call back into the caller) cannot be
    /// re-entered and reports `Trap::Host`.
    fn run_in(&mut self, instance: usize, func: u32, args: &[Value], results: &mut [Value]) -> Result<HostOutcome, Trap> {
        let mut target = self.instances.get_mut(instance).and_then(Option::take).ok_or(Trap::Host)?;
        let r = Execution::new(&target, func, args).map(|mut exec| exec.run(&mut target, self, u64::MAX));
        self.instances[instance] = Some(target);
        match r {
            Ok(ExecResult::Done(vals)) => {
                if vals.len() != results.len() {
                    return Err(Trap::Host);
                }
                results.copy_from_slice(&vals);
                Ok(HostOutcome::Continue)
            }
            Ok(ExecResult::Trap(t)) => Err(t),
            Ok(ExecResult::Exited(_) | ExecResult::Blocked | ExecResult::OutOfFuel) => Err(Trap::Host),
            Err(_) => Err(Trap::Host),
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
        match *self.host_funcs.get(func.0 as usize).ok_or(Trap::Host)? {
            HostFn::Print => Ok(HostOutcome::Continue),
            HostFn::Cross { instance, func } => self.run_in(instance, func, args, results),
        }
    }

    /// `call_indirect` through a function of another instance: the address
    /// range names the instance, the offset its function.
    fn call_foreign(
        &mut self,
        _ctx: &mut HostCtx<'_>,
        addr: u32,
        ty: &FuncType,
        args: &[Value],
        results: &mut [Value],
    ) -> Result<HostOutcome, Trap> {
        let instance = (addr / FUNC_ADDR_SPACE) as usize;
        let func = addr % FUNC_ADDR_SPACE;
        let target = self.instances.get(instance).and_then(Option::as_ref).ok_or(Trap::Host)?;
        if target.func_type(func) != Some(ty) {
            return Err(Trap::IndirectCallTypeMismatch);
        }
        self.run_in(instance, func, args, results)
    }
}

// ---------------------------------------------------------------------------
// Values and expectations
// ---------------------------------------------------------------------------

fn heap_is_func(h: &HeapType<'_>) -> Option<bool> {
    match h {
        HeapType::Abstract { ty: AbstractHeapType::Func, .. } => Some(true),
        HeapType::Abstract { ty: AbstractHeapType::Extern, .. } => Some(false),
        _ => None,
    }
}

fn arg_value(arg: &WastArg<'_>) -> Result<Value, String> {
    let WastArg::Core(core) = arg else {
        return Err("component-model arguments are not supported".to_owned());
    };
    Ok(match core {
        WastArgCore::I32(v) => Value::I32(*v),
        WastArgCore::I64(v) => Value::I64(*v),
        WastArgCore::F32(f) => Value::F32(f32::from_bits(f.bits)),
        WastArgCore::F64(f) => Value::F64(f64::from_bits(f.bits)),
        WastArgCore::RefNull(h) => match heap_is_func(h) {
            Some(true) => Value::FuncRef(None),
            Some(false) => Value::ExternRef(None),
            None => return Err(format!("unsupported heap type {h:?}")),
        },
        WastArgCore::RefExtern(n) | WastArgCore::RefHost(n) => Value::ExternRef(Some(*n)),
        WastArgCore::V128(_) => return Err("v128 arguments are not supported".to_owned()),
    })
}

/// Whether `got` matches the expected result pattern.
fn ret_matches(expected: &WastRetCore<'_>, got: Value) -> Result<bool, String> {
    Ok(match expected {
        WastRetCore::I32(v) => got == Value::I32(*v),
        WastRetCore::I64(v) => got == Value::I64(*v),
        WastRetCore::F32(p) => match got {
            Value::F32(f) => {
                let bits = f.to_bits();
                match p {
                    NanPattern::CanonicalNan => bits & 0x7fff_ffff == 0x7fc0_0000,
                    NanPattern::ArithmeticNan => bits & 0x7fc0_0000 == 0x7fc0_0000,
                    NanPattern::Value(v) => bits == v.bits,
                }
            }
            _ => false,
        },
        WastRetCore::F64(p) => match got {
            Value::F64(f) => {
                let bits = f.to_bits();
                match p {
                    NanPattern::CanonicalNan => bits & 0x7fff_ffff_ffff_ffff == 0x7ff8_0000_0000_0000,
                    NanPattern::ArithmeticNan => bits & 0x7ff8_0000_0000_0000 == 0x7ff8_0000_0000_0000,
                    NanPattern::Value(v) => bits == v.bits,
                }
            }
            _ => false,
        },
        WastRetCore::RefNull(None) => got.is_null_ref(),
        WastRetCore::RefNull(Some(h)) => match heap_is_func(h) {
            Some(true) => got == Value::FuncRef(None),
            Some(false) => got == Value::ExternRef(None),
            None => return Err(format!("unsupported heap type {h:?}")),
        },
        WastRetCore::RefExtern(None) => matches!(got, Value::ExternRef(Some(_))),
        WastRetCore::RefExtern(Some(n)) | WastRetCore::RefHost(n) => got == Value::ExternRef(Some(*n)),
        WastRetCore::RefFunc(None) | WastRetCore::RefFunc(Some(Index::Id(_))) => {
            matches!(got, Value::FuncRef(Some(_)))
        }
        WastRetCore::RefFunc(Some(Index::Num(n, _))) => got == Value::FuncRef(Some(*n)),
        WastRetCore::Either(alts) => {
            for alt in alts {
                if ret_matches(alt, got)? {
                    return Ok(true);
                }
            }
            false
        }
        other => return Err(format!("unsupported result pattern {other:?}")),
    })
}

fn describe(values: &[Value]) -> String {
    let mut s = String::from("[");
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        match v {
            Value::F32(f) => {
                let _ = write!(s, "f32:{:#010x}", f.to_bits());
            }
            Value::F64(f) => {
                let _ = write!(s, "f64:{:#018x}", f.to_bits());
            }
            other => {
                let _ = write!(s, "{other:?}");
            }
        }
    }
    s.push(']');
    s
}

/// Whether our message for a trap or link error matches the script's (the
/// reference interpreter's messages are sometimes longer, sometimes
/// shorter than ours).
fn message_matches(expected: &str, ours: &str) -> bool {
    ours.starts_with(expected) || expected.starts_with(ours)
}

fn trap_message_matches(expected: &str, trap: Trap) -> bool {
    message_matches(expected, trap.message())
}

// ---------------------------------------------------------------------------
// The per-file runner
// ---------------------------------------------------------------------------

/// Outcome counters. Every directive ends in exactly one of these or in a
/// failure; `directives` counts them all, so the two sides must balance.
#[derive(Default, Clone, Copy)]
struct Stats {
    directives: usize,
    modules: usize,
    definitions: usize,
    registers: usize,
    invocations: usize,
    returns: usize,
    traps: usize,
    exhaustions: usize,
    unlinkable: usize,
    rejected: usize,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.directives += o.directives;
        self.modules += o.modules;
        self.definitions += o.definitions;
        self.registers += o.registers;
        self.invocations += o.invocations;
        self.returns += o.returns;
        self.traps += o.traps;
        self.exhaustions += o.exhaustions;
        self.unlinkable += o.unlinkable;
        self.rejected += o.rejected;
    }

    /// Directives that ended in a recorded pass.
    fn passes(&self) -> usize {
        self.modules
            + self.definitions
            + self.registers
            + self.invocations
            + self.returns
            + self.traps
            + self.exhaustions
            + self.unlinkable
            + self.rejected
    }
}

struct Runner<'a> {
    file: &'a str,
    text: &'a str,
    store: Store,
    stats: Stats,
    failures: Vec<String>,
}

/// `names.wast` deliberately uses confusable unicode in export names, which
/// the `wast` lexer refuses by default.
fn parse_buffer(text: &str) -> wast::parser::Result<ParseBuffer<'_>> {
    let mut lexer = Lexer::new(text);
    lexer.allow_confusing_unicode(true);
    ParseBuffer::new_with_lexer(lexer)
}

fn module_id<'a>(m: &QuoteWat<'a>) -> Option<Id<'a>> {
    match m {
        QuoteWat::Wat(Wat::Module(m)) => m.id,
        _ => None,
    }
}

fn wat_id<'a>(m: &Wat<'a>) -> Option<Id<'a>> {
    match m {
        Wat::Module(m) => m.id,
        Wat::Component(_) => None,
    }
}

impl Runner<'_> {
    fn line(&self, span: Span) -> usize {
        span.linecol_in(self.text).0 + 1
    }

    fn fail(&mut self, span: Span, msg: String) {
        self.failures.push(format!("{}:{}: {msg}", self.file, self.line(span)));
    }

    /// A `module` directive: instantiate and make current.
    fn module(&mut self, span: Span, bytes: &[u8], id: Option<Id<'_>>) {
        let idx = match self.store.instantiate(bytes) {
            Ok(idx) => {
                self.stats.modules += 1;
                idx
            }
            Err(e) => {
                self.fail(span, format!("module failed to instantiate: {e}"));
                return;
            }
        };
        if let Some(id) = id {
            self.store.by_id.insert(id.name().to_owned(), idx);
        }
        self.store.current = Some(idx);
    }

    fn args(&self, invoke: &WastInvoke<'_>) -> Result<Vec<Value>, String> {
        invoke.args.iter().map(arg_value).collect()
    }

    /// Run an invocation.
    fn exec_invoke(&mut self, invoke: &WastInvoke<'_>) -> Result<ExecResult, String> {
        let idx = self.store.instance_of(invoke.module)?;
        let args = self.args(invoke)?;
        self.store.invoke(idx, invoke.name, &args)
    }

    fn directive(&mut self, directive: WastDirective<'_>) {
        // A thread is a container; its directives are counted one by one.
        if !matches!(directive, WastDirective::Thread(_)) {
            self.stats.directives += 1;
        }
        match directive {
            WastDirective::Module(mut m) => {
                let span = m.span();
                let id = module_id(&m);
                match m.encode() {
                    Ok(bytes) => self.module(span, &bytes, id),
                    Err(e) => self.fail(span, format!("wast could not encode module: {e}")),
                }
            }
            WastDirective::ModuleDefinition(mut m) => {
                // Define without instantiating: it must at least validate.
                let span = m.span();
                match m.encode() {
                    Ok(bytes) => match Module::new(&bytes) {
                        Ok(_) => self.stats.definitions += 1,
                        Err(e) => self.fail(span, format!("module definition rejected: {e}")),
                    },
                    Err(e) => self.fail(span, format!("wast could not encode module: {e}")),
                }
            }
            WastDirective::ModuleInstance { span, .. } => {
                self.fail(span, "module instance directives are not supported".to_owned());
            }
            WastDirective::Register { span, name, module } => match self.store.instance_of(module) {
                Ok(idx) => {
                    self.store.registered.insert(name.to_owned(), idx);
                    self.stats.registers += 1;
                }
                Err(e) => self.fail(span, format!("register: {e}")),
            },
            WastDirective::Invoke(invoke) => {
                let span = invoke.span;
                match self.exec_invoke(&invoke) {
                    Ok(ExecResult::Done(_)) => self.stats.invocations += 1,
                    Ok(other) => self.fail(span, format!("invoke \"{}\": {other:?}", invoke.name)),
                    Err(e) => self.fail(span, format!("invoke \"{}\": {e}", invoke.name)),
                }
            }
            WastDirective::AssertReturn { span, exec, results } => self.assert_return(span, exec, &results),
            WastDirective::AssertTrap { span, exec, message } => self.assert_trap(span, exec, message),
            WastDirective::AssertExhaustion { span, call, message } => match self.exec_invoke(&call) {
                Ok(ExecResult::Trap(Trap::CallDepth | Trap::StackOverflow)) => self.stats.exhaustions += 1,
                Ok(other) => self.fail(span, format!("assert_exhaustion \"{message}\" on \"{}\": got {other:?}", call.name)),
                Err(e) => self.fail(span, format!("assert_exhaustion: {e}")),
            },
            WastDirective::AssertUnlinkable { span, mut module, message } => match module.encode() {
                Ok(bytes) => match self.store.instantiate(&bytes) {
                    Ok(_) => self.fail(span, format!("assert_unlinkable \"{message}\": module instantiated")),
                    Err(e @ (Error::Link { .. } | Error::IncompatibleImport { .. })) => {
                        if message_matches(message, e.message()) {
                            self.stats.unlinkable += 1;
                        } else {
                            self.fail(
                                span,
                                format!("assert_unlinkable \"{message}\": linking failed with \"{}\"", e.message()),
                            );
                        }
                    }
                    Err(e) => self.fail(span, format!("assert_unlinkable \"{message}\": wrong error: {e}")),
                },
                Err(e) => self.fail(span, format!("wast could not encode module: {e}")),
            },
            WastDirective::AssertMalformed { span, mut module, .. } | WastDirective::AssertInvalid { span, mut module, .. } => {
                match module.encode() {
                    Ok(bytes) => match Module::new(&bytes) {
                        Ok(_) => self.fail(span, "malformed/invalid module accepted".to_owned()),
                        Err(_) => self.stats.rejected += 1,
                    },
                    // A quoted module the text parser itself refuses is
                    // malformed, as the directive says.
                    Err(_) => self.stats.rejected += 1,
                }
            }
            WastDirective::Thread(thread) => {
                for d in thread.directives {
                    self.directive(d);
                }
            }
            WastDirective::AssertMalformedCustom { .. }
            | WastDirective::AssertInvalidCustom { .. }
            | WastDirective::AssertException { .. }
            | WastDirective::AssertSuspension { .. }
            | WastDirective::Wait { .. } => {
                let span = directive.span();
                self.fail(span, "unsupported directive".to_owned());
            }
        }
    }

    fn assert_return(&mut self, span: Span, exec: WastExecute<'_>, results: &[WastRet<'_>]) {
        let (got, what) = match exec {
            WastExecute::Invoke(invoke) => match self.exec_invoke(&invoke) {
                Ok(ExecResult::Done(v)) => (v, format!("invoke \"{}\"", invoke.name)),
                Ok(other) => return self.fail(span, format!("assert_return on \"{}\": got {other:?}", invoke.name)),
                Err(e) => return self.fail(span, format!("assert_return: {e}")),
            },
            WastExecute::Get { module, global, .. } => {
                let idx = match self.store.instance_of(module) {
                    Ok(s) => s,
                    Err(e) => return self.fail(span, format!("get: {e}")),
                };
                match self.store.get_global(idx, global) {
                    Ok(v) => (vec![v], format!("get \"{global}\"")),
                    Err(e) => return self.fail(span, format!("get: {e}")),
                }
            }
            WastExecute::Wat(_) => {
                return self.fail(span, "assert_return on a module".to_owned());
            }
        };
        if got.len() != results.len() {
            return self.fail(span, format!("{what}: expected {} results, got {}", results.len(), describe(&got)));
        }
        for (i, (r, g)) in results.iter().zip(&got).enumerate() {
            let WastRet::Core(expected) = r else {
                return self.fail(span, format!("{what}: unsupported result {r:?}"));
            };
            match ret_matches(expected, *g) {
                Ok(true) => {}
                Ok(false) => {
                    return self.fail(span, format!("{what}: result {i} expected {expected:?}, got {}", describe(&got)));
                }
                Err(e) => return self.fail(span, format!("{what}: {e}")),
            }
        }
        self.stats.returns += 1;
    }

    fn assert_trap(&mut self, span: Span, exec: WastExecute<'_>, message: &str) {
        match exec {
            WastExecute::Invoke(invoke) => match self.exec_invoke(&invoke) {
                Ok(ExecResult::Trap(t)) => {
                    if trap_message_matches(message, t) {
                        self.stats.traps += 1;
                    } else {
                        self.fail(
                            span,
                            format!("assert_trap \"{message}\" on \"{}\": trapped with \"{}\"", invoke.name, t.message()),
                        );
                    }
                }
                Ok(other) => self.fail(span, format!("assert_trap \"{message}\" on \"{}\": got {other:?}", invoke.name)),
                Err(e) => self.fail(span, format!("assert_trap: {e}")),
            },
            WastExecute::Wat(mut module) => {
                let id = wat_id(&module);
                let bytes = match module.encode() {
                    Ok(b) => b,
                    Err(e) => return self.fail(span, format!("wast could not encode module: {e}")),
                };
                match self.store.instantiate(&bytes) {
                    Ok(idx) => {
                        // The reference interpreter keeps a module that
                        // unexpectedly instantiated; so do we, for the
                        // directives that follow.
                        if let Some(id) = id {
                            self.store.by_id.insert(id.name().to_owned(), idx);
                        }
                        self.store.current = Some(idx);
                        self.fail(span, format!("assert_trap \"{message}\": module instantiated"));
                    }
                    Err(Error::Trap(t)) => {
                        if trap_message_matches(message, t) {
                            self.stats.traps += 1;
                        } else {
                            self.fail(
                                span,
                                format!("assert_trap \"{message}\": instantiation trapped with \"{}\"", t.message()),
                            );
                        }
                    }
                    Err(e) => self.fail(span, format!("assert_trap \"{message}\": instantiation failed with {e}")),
                }
            }
            WastExecute::Get { global, .. } => {
                self.fail(span, format!("assert_trap on get \"{global}\""));
            }
        }
    }
}

fn spec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/spec")
}

fn spec_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(spec_dir())
        .expect("tests/spec exists")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "wast"))
        .collect();
    files.sort();
    files
}

struct FileReport {
    file: String,
    stats: Stats,
    failures: Vec<String>,
}

fn run_file(path: &Path) -> FileReport {
    let file = path.file_name().and_then(|f| f.to_str()).unwrap_or("?").to_owned();
    let text = std::fs::read_to_string(path).expect("readable spec file");
    let buf = parse_buffer(&text).unwrap_or_else(|e| panic!("{file}: lexing failed: {e}"));
    let wast: Wast = parser::parse(&buf).unwrap_or_else(|e| panic!("{file}: parsing failed: {e}"));

    let mut runner = Runner { file: &file, text: &text, store: Store::new(), stats: Stats::default(), failures: Vec::new() };
    for directive in wast.directives {
        runner.directive(directive);
    }
    let (stats, failures) = (runner.stats, runner.failures);
    FileReport { file, stats, failures }
}

#[test]
fn spec_suite_executes() {
    let files = spec_files();
    assert!(files.len() >= 80, "expected the vendored spec suite, found {} files", files.len());

    let mut total = Stats::default();
    let mut report = String::new();
    let mut failures: Vec<String> = Vec::new();
    let mut unbalanced: Vec<String> = Vec::new();
    let mut passed_files = 0usize;
    for path in &files {
        let r = run_file(path);
        total.add(&r.stats);
        let s = &r.stats;
        let status = if r.failures.is_empty() {
            passed_files += 1;
            "ok"
        } else {
            "FAIL"
        };
        let _ = writeln!(
            report,
            "{status:4} {:32} {:4} directives {:4} modules {:5} returns {:4} traps {:3} unlinkable",
            r.file, s.directives, s.modules, s.returns, s.traps, s.unlinkable
        );
        // Every directive must have ended in a recorded outcome: nothing is
        // skipped, silently or otherwise.
        if s.directives != s.passes() + r.failures.len() {
            unbalanced.push(format!(
                "{}: {} directives, {} passes, {} failures",
                r.file,
                s.directives,
                s.passes(),
                r.failures.len()
            ));
        }
        failures.extend(r.failures);
    }
    let _ = writeln!(
        report,
        "\nspec execution: {} files ({passed_files} complete), {} directives, {} modules \
         instantiated, {} module definitions, {} registers, {} invocations, \
         {} assert_return, {} assert_trap, {} assert_exhaustion, {} assert_unlinkable, \
         {} malformed/invalid rejected, 0 skipped",
        files.len(),
        total.directives,
        total.modules,
        total.definitions,
        total.registers,
        total.invocations,
        total.returns,
        total.traps,
        total.exhaustions,
        total.unlinkable,
        total.rejected,
    );
    if !failures.is_empty() {
        let _ = writeln!(report, "\n{} failures:", failures.len());
        for f in failures.iter().take(200) {
            let _ = writeln!(report, "  {f}");
        }
        if failures.len() > 200 {
            let _ = writeln!(report, "  ... and {} more", failures.len() - 200);
        }
    }
    if !unbalanced.is_empty() {
        let _ = writeln!(report, "\nfiles with unaccounted directives:");
        for u in &unbalanced {
            let _ = writeln!(report, "  {u}");
        }
    }
    println!("{report}");
    assert!(failures.is_empty() && unbalanced.is_empty(), "spec execution failures:\n{report}");
    assert_eq!(passed_files, files.len(), "every file must run completely");
    // Sanity: the suite really was exercised.
    assert!(total.returns > 15_000, "too few assert_return: {}", total.returns);
    assert!(total.traps > 500, "too few assert_trap: {}", total.traps);
    assert!(total.exhaustions >= 5, "too few assert_exhaustion: {}", total.exhaustions);
    assert!(total.unlinkable >= 80, "too few assert_unlinkable: {}", total.unlinkable);
}
