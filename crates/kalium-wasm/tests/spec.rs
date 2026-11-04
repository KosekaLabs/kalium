//! Runs the execution part of the official WebAssembly spec test suite
//! (release 2.0, vendored under `tests/spec/`, see `tests/spec/README.md`)
//! through the interpreter.
//!
//! For every `.wast` script, in order:
//!
//! - `(module ...)` must decode, validate, instantiate and run its start
//!   function; it becomes the current module and may be named with `$id`;
//! - `(register "name" $id?)` makes a module's function exports importable
//!   by later modules (cross-instance calls go through the `Host` trait,
//!   exactly as the kernel would route them);
//! - `(invoke ...)` must complete without trapping;
//! - `(assert_return (invoke ...) results)` and `(assert_return (get ...))`
//!   must produce exactly the expected values (NaN patterns included);
//! - `(assert_trap (invoke ...) msg)` must trap with the spec's message, and
//!   `(assert_trap (module ...) msg)` must fail instantiation with it;
//! - `(assert_exhaustion ...)` must trap with the call-depth or value-stack
//!   limit;
//! - `(assert_unlinkable (module ...))` must fail to instantiate;
//! - `(assert_malformed ...)` and `(assert_invalid ...)` must be rejected
//!   (`tests/spec_validate.rs` also checks the messages).
//!
//! The `spectest` host module provides the `print*` functions. Its table,
//! memory and global exports cannot be provided because M1 supports function
//! imports only: a module importing a table, memory or global is *skipped*,
//! as are the directives that target it, any module importing from it, and
//! any invocation of an instance whose table or memory the skipped module
//! would have shared (its state would differ from what the script expects).
//! Every skip is counted and the files in which skips may happen are listed
//! explicitly in [`FILES_WITH_SKIPS`]; a skip anywhere else fails the test,
//! so a regression cannot hide behind a skip.

use kalium_wasm::{
    Error, ExecResult, Execution, ExportDesc, FuncType, Host, HostCtx, HostFuncId, HostOutcome,
    Instance, Limits, Module, Trap, ValType, Value,
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

/// Files in which modules are skipped because they import a table, memory or
/// global (from `spectest` or from a registered module), with the reason.
/// Everything else must run completely.
const FILES_WITH_SKIPS: &[(&str, &str)] = &[
    ("data.wast", "imports spectest memory/globals"),
    (
        "elem.wast",
        "imports spectest table/globals and shares tables",
    ),
    ("global.wast", "imports spectest globals"),
    ("imports.wast", "imports spectest table/memory/globals"),
    (
        "linking.wast",
        "shares tables, memories and globals between modules",
    ),
    (
        "memory_grow.wast",
        "imports a grown memory from a registered module",
    ),
    ("table.wast", "imports spectest table"),
    (
        "table_grow.wast",
        "imports a grown table from a registered module",
    ),
];

/// Generous limits: the suite grows memories to 803 pages and recurses until
/// `assert_exhaustion` fires.
const LIMITS: Limits = Limits {
    max_memory_pages: 65536,
    max_stack_values: 1 << 20,
    max_call_depth: 10_000,
};

// ---------------------------------------------------------------------------
// The store: instances, names and the host
// ---------------------------------------------------------------------------

/// What a module directive produced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    /// A live instance (index into `Store::instances`).
    Live(usize),
    /// Skipped: it needs a non-function import.
    Skipped,
}

/// A resolved host function.
#[derive(Clone, Copy, Debug)]
enum HostFn {
    /// One of the `spectest.print*` functions.
    Print,
    /// A function exported by another instance in the store.
    Cross { instance: usize, func: u32 },
}

#[derive(Default)]
struct Store {
    /// `None` while that instance is running (it is lent to the interpreter).
    instances: Vec<Option<Instance>>,
    /// Per instance: the instances it imports functions from.
    deps: Vec<Vec<usize>>,
    /// Per instance: whether a skipped module would have shared (and
    /// modified) its table or memory, so its state is not what the script
    /// expects any more.
    tainted: Vec<bool>,
    /// `$id` of a module directive -> what it produced.
    by_id: HashMap<String, Slot>,
    /// `(register "name")` -> what it refers to.
    registered: HashMap<String, Slot>,
    current: Option<Slot>,
    host_funcs: Vec<HostFn>,
}

/// Why a module could not be instantiated.
enum NotInstantiated {
    /// Needs a non-function import: skipped, not a failure. `shares_with`
    /// are the live instances whose table/memory/global it would have
    /// imported.
    Skipped {
        why: String,
        shares_with: Vec<usize>,
    },
    /// A real error (decode, validate, link, trap, ...).
    Failed(Error),
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

impl Store {
    fn slot_of(&self, id: Option<Id<'_>>) -> Result<Slot, String> {
        match id {
            Some(id) => self
                .by_id
                .get(id.name())
                .copied()
                .ok_or_else(|| format!("unknown module ${}", id.name())),
            None => self.current.ok_or_else(|| "no current module".to_owned()),
        }
    }

    /// Decode, validate, instantiate and run the start function.
    fn instantiate(&mut self, bytes: &[u8]) -> Result<usize, NotInstantiated> {
        let module = Module::new(bytes).map_err(NotInstantiated::Failed)?;
        let module = Arc::new(module);

        // Non-function imports cannot be provided in M1.
        let mut shares_with = Vec::new();
        let mut why = None;
        for imp in module.imports() {
            let kind = match imp.desc {
                kalium_wasm::ImportDesc::Func(_) => continue,
                kalium_wasm::ImportDesc::Table(_) => "table",
                kalium_wasm::ImportDesc::Memory(_) => "memory",
                kalium_wasm::ImportDesc::Global(_) => "global",
            };
            if let Some(Slot::Live(idx)) = self.registered.get(imp.module.as_str()) {
                shares_with.push(*idx);
            }
            why.get_or_insert_with(|| {
                format!("imports {kind} \"{}\".\"{}\"", imp.module, imp.name)
            });
        }
        if let Some(why) = why {
            return Err(NotInstantiated::Skipped { why, shares_with });
        }

        let mut depends_on_skipped = None;
        let mut deps = Vec::new();
        let instance = {
            let instances = &self.instances;
            let registered = &self.registered;
            let host_funcs = &mut self.host_funcs;
            let mut resolve = |m: &str, n: &str, ty: &FuncType| -> Result<HostFuncId, ()> {
                let hf = if m == "spectest" {
                    if spectest_type(n).as_ref() != Some(ty) {
                        return Err(());
                    }
                    HostFn::Print
                } else {
                    match registered.get(m) {
                        Some(Slot::Live(idx)) => {
                            let target = instances[*idx].as_ref().ok_or(())?;
                            let func = target.export_func(n).ok_or(())?;
                            if target.module().func_type_of(func) != Some(ty) {
                                return Err(());
                            }
                            deps.push(*idx);
                            HostFn::Cross {
                                instance: *idx,
                                func,
                            }
                        }
                        Some(Slot::Skipped) => {
                            depends_on_skipped =
                                Some(format!("imports from skipped module \"{m}\""));
                            return Err(());
                        }
                        None => return Err(()),
                    }
                };
                let id = HostFuncId(host_funcs.len() as u32);
                host_funcs.push(hf);
                Ok(id)
            };
            Instance::new(module, &mut resolve, &LIMITS)
        };
        let mut instance = match instance {
            Ok(i) => i,
            Err(Error::Link { .. }) if depends_on_skipped.is_some() => {
                return Err(NotInstantiated::Skipped {
                    why: depends_on_skipped.unwrap_or_default(),
                    shares_with: Vec::new(),
                })
            }
            Err(e) => return Err(NotInstantiated::Failed(e)),
        };

        // The start function.
        if let Some(mut start) = Execution::start(&instance).map_err(NotInstantiated::Failed)? {
            match start.run(&mut instance, self, u64::MAX) {
                ExecResult::Done(_) => {}
                ExecResult::Trap(t) => return Err(NotInstantiated::Failed(Error::Trap(t))),
                other => {
                    return Err(NotInstantiated::Failed(Error::Trap(match other {
                        ExecResult::OutOfFuel => Trap::OutOfFuel,
                        _ => Trap::Host,
                    })))
                }
            }
        }

        let idx = self.instances.len();
        self.instances.push(Some(instance));
        self.deps.push(deps);
        self.tainted.push(false);
        Ok(idx)
    }

    /// Whether the instance, or an instance it imports functions from, has
    /// state a skipped module would have modified.
    fn is_tainted(&self, idx: usize) -> bool {
        let mut seen = vec![false; self.instances.len()];
        let mut todo = vec![idx];
        while let Some(i) = todo.pop() {
            if seen[i] {
                continue;
            }
            seen[i] = true;
            if self.tainted[i] {
                return true;
            }
            todo.extend(self.deps[i].iter().copied());
        }
        false
    }

    /// Invoke an export of the given (or current) module.
    fn invoke(&mut self, slot: Slot, name: &str, args: &[Value]) -> Result<ExecResult, String> {
        let idx = match slot {
            Slot::Live(idx) => idx,
            Slot::Skipped => return Err("module was skipped".to_owned()),
        };
        let mut inst = self.instances[idx]
            .take()
            .ok_or_else(|| "instance is already running".to_owned())?;
        let result = (|| {
            let func = inst
                .export_func(name)
                .ok_or_else(|| format!("no exported function \"{name}\""))?;
            let mut exec = Execution::new(&inst, func, args)
                .map_err(|e| format!("cannot call \"{name}\": {e}"))?;
            Ok(exec.run(&mut inst, self, u64::MAX))
        })();
        self.instances[idx] = Some(inst);
        result
    }

    /// Read an exported global.
    fn get_global(&self, slot: Slot, name: &str) -> Result<Value, String> {
        let idx = match slot {
            Slot::Live(idx) => idx,
            Slot::Skipped => return Err("module was skipped".to_owned()),
        };
        let inst = self.instances[idx]
            .as_ref()
            .ok_or_else(|| "instance is already running".to_owned())?;
        match inst.module().export(name).map(|e| e.desc) {
            Some(ExportDesc::Global(g)) => inst
                .global(g)
                .ok_or_else(|| format!("global {g} out of range")),
            _ => Err(format!("no exported global \"{name}\"")),
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
            HostFn::Cross { instance, func } => {
                // Nested execution on the other instance; it is lent to the
                // interpreter for the duration of the call.
                let mut target = self
                    .instances
                    .get_mut(instance)
                    .and_then(Option::take)
                    .ok_or(Trap::Host)?;
                let r = Execution::new(&target, func, args)
                    .map(|mut exec| exec.run(&mut target, self, u64::MAX));
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
                    Ok(ExecResult::Exited(_) | ExecResult::Blocked | ExecResult::OutOfFuel) => {
                        Err(Trap::Host)
                    }
                    Err(_) => Err(Trap::Host),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Values and expectations
// ---------------------------------------------------------------------------

fn heap_is_func(h: &HeapType<'_>) -> Option<bool> {
    match h {
        HeapType::Abstract {
            ty: AbstractHeapType::Func,
            ..
        } => Some(true),
        HeapType::Abstract {
            ty: AbstractHeapType::Extern,
            ..
        } => Some(false),
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
                    NanPattern::CanonicalNan => {
                        bits & 0x7fff_ffff_ffff_ffff == 0x7ff8_0000_0000_0000
                    }
                    NanPattern::ArithmeticNan => {
                        bits & 0x7ff8_0000_0000_0000 == 0x7ff8_0000_0000_0000
                    }
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
        WastRetCore::RefExtern(Some(n)) | WastRetCore::RefHost(n) => {
            got == Value::ExternRef(Some(*n))
        }
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

fn trap_message_matches(expected: &str, trap: Trap) -> bool {
    let ours = trap.message();
    ours.starts_with(expected) || expected.starts_with(ours)
}

// ---------------------------------------------------------------------------
// The per-file runner
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct Stats {
    modules: usize,
    invocations: usize,
    returns: usize,
    traps: usize,
    exhaustions: usize,
    unlinkable: usize,
    rejected: usize,
    skipped_modules: usize,
    skipped_directives: usize,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.modules += o.modules;
        self.invocations += o.invocations;
        self.returns += o.returns;
        self.traps += o.traps;
        self.exhaustions += o.exhaustions;
        self.unlinkable += o.unlinkable;
        self.rejected += o.rejected;
        self.skipped_modules += o.skipped_modules;
        self.skipped_directives += o.skipped_directives;
    }
}

struct Runner<'a> {
    file: &'a str,
    text: &'a str,
    store: Store,
    stats: Stats,
    failures: Vec<String>,
    skips: Vec<String>,
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
        self.failures
            .push(format!("{}:{}: {msg}", self.file, self.line(span)));
    }

    fn skip(&mut self, span: Span, msg: String) {
        self.stats.skipped_directives += 1;
        self.skips
            .push(format!("{}:{}: {msg}", self.file, self.line(span)));
    }

    /// A `module` directive: instantiate and make current.
    fn module(&mut self, span: Span, bytes: &[u8], id: Option<Id<'_>>) {
        let slot = match self.store.instantiate(bytes) {
            Ok(idx) => {
                self.stats.modules += 1;
                Slot::Live(idx)
            }
            Err(NotInstantiated::Skipped { why, shares_with }) => {
                self.stats.skipped_modules += 1;
                self.skips.push(format!(
                    "{}:{}: module skipped: {why}",
                    self.file,
                    self.line(span)
                ));
                for idx in shares_with {
                    self.store.tainted[idx] = true;
                }
                Slot::Skipped
            }
            Err(NotInstantiated::Failed(e)) => {
                self.fail(span, format!("module failed to instantiate: {e}"));
                return;
            }
        };
        if let Some(id) = id {
            self.store.by_id.insert(id.name().to_owned(), slot);
        }
        self.store.current = Some(slot);
    }

    fn args(&self, invoke: &WastInvoke<'_>) -> Result<Vec<Value>, String> {
        invoke.args.iter().map(arg_value).collect()
    }

    /// Run an invocation; `Ok(None)` when the target module was skipped or
    /// its state depends on a skipped module.
    fn exec_invoke(&mut self, invoke: &WastInvoke<'_>) -> Result<Option<ExecResult>, String> {
        let slot = self.store.slot_of(invoke.module)?;
        match slot {
            Slot::Skipped => return Ok(None),
            Slot::Live(idx) if self.store.is_tainted(idx) => return Ok(None),
            Slot::Live(_) => {}
        }
        let args = self.args(invoke)?;
        self.store.invoke(slot, invoke.name, &args).map(Some)
    }

    fn directive(&mut self, directive: WastDirective<'_>) {
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
                    Ok(bytes) => {
                        if let Err(e) = Module::new(&bytes) {
                            self.fail(span, format!("module definition rejected: {e}"));
                        }
                    }
                    Err(e) => self.fail(span, format!("wast could not encode module: {e}")),
                }
            }
            WastDirective::ModuleInstance { span, .. } => {
                self.fail(
                    span,
                    "module instance directives are not supported".to_owned(),
                );
            }
            WastDirective::Register { span, name, module } => match self.store.slot_of(module) {
                Ok(slot) => {
                    self.store.registered.insert(name.to_owned(), slot);
                }
                Err(e) => self.fail(span, format!("register: {e}")),
            },
            WastDirective::Invoke(invoke) => {
                let span = invoke.span;
                match self.exec_invoke(&invoke) {
                    Ok(Some(ExecResult::Done(_))) => self.stats.invocations += 1,
                    Ok(Some(other)) => {
                        self.fail(span, format!("invoke \"{}\": {other:?}", invoke.name))
                    }
                    Ok(None) => self.skip(span, format!("invoke \"{}\"", invoke.name)),
                    Err(e) => self.fail(span, format!("invoke \"{}\": {e}", invoke.name)),
                }
            }
            WastDirective::AssertReturn {
                span,
                exec,
                results,
            } => self.assert_return(span, exec, &results),
            WastDirective::AssertTrap {
                span,
                exec,
                message,
            } => self.assert_trap(span, exec, message),
            WastDirective::AssertExhaustion {
                span,
                call,
                message,
            } => match self.exec_invoke(&call) {
                Ok(Some(ExecResult::Trap(Trap::CallDepth | Trap::StackOverflow))) => {
                    self.stats.exhaustions += 1
                }
                Ok(Some(other)) => self.fail(
                    span,
                    format!(
                        "assert_exhaustion \"{message}\" on \"{}\": got {other:?}",
                        call.name
                    ),
                ),
                Ok(None) => self.skip(span, format!("assert_exhaustion \"{}\"", call.name)),
                Err(e) => self.fail(span, format!("assert_exhaustion: {e}")),
            },
            WastDirective::AssertUnlinkable {
                span,
                mut module,
                message,
            } => match module.encode() {
                Ok(bytes) => match self.store.instantiate(&bytes) {
                    Ok(_) => self.fail(
                        span,
                        format!("assert_unlinkable \"{message}\": module instantiated"),
                    ),
                    Err(NotInstantiated::Failed(Error::Link { .. })) => self.stats.unlinkable += 1,
                    Err(NotInstantiated::Skipped { why, .. }) => {
                        // Rejected for the right reason, by a different check.
                        self.stats.unlinkable += 1;
                        self.skips.push(format!(
                            "{}:{}: assert_unlinkable module rejected as unsupported ({why})",
                            self.file,
                            self.line(span)
                        ));
                    }
                    Err(NotInstantiated::Failed(e)) => self.fail(
                        span,
                        format!("assert_unlinkable \"{message}\": wrong error: {e}"),
                    ),
                },
                Err(e) => self.fail(span, format!("wast could not encode module: {e}")),
            },
            WastDirective::AssertMalformed {
                span, mut module, ..
            }
            | WastDirective::AssertInvalid {
                span, mut module, ..
            } => {
                if let Ok(bytes) = module.encode() {
                    match Module::new(&bytes) {
                        Ok(_) => self.fail(span, "malformed/invalid module accepted".to_owned()),
                        Err(_) => self.stats.rejected += 1,
                    }
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
                Ok(Some(ExecResult::Done(v))) => (v, format!("invoke \"{}\"", invoke.name)),
                Ok(Some(other)) => {
                    return self.fail(
                        span,
                        format!("assert_return on \"{}\": got {other:?}", invoke.name),
                    )
                }
                Ok(None) => return self.skip(span, format!("assert_return \"{}\"", invoke.name)),
                Err(e) => return self.fail(span, format!("assert_return: {e}")),
            },
            WastExecute::Get { module, global, .. } => {
                let slot = match self.store.slot_of(module) {
                    Ok(s) => s,
                    Err(e) => return self.fail(span, format!("get: {e}")),
                };
                if slot == Slot::Skipped
                    || matches!(slot, Slot::Live(idx) if self.store.is_tainted(idx))
                {
                    return self.skip(span, format!("get \"{global}\""));
                }
                match self.store.get_global(slot, global) {
                    Ok(v) => (vec![v], format!("get \"{global}\"")),
                    Err(e) => return self.fail(span, format!("get: {e}")),
                }
            }
            WastExecute::Wat(_) => {
                return self.fail(span, "assert_return on a module".to_owned());
            }
        };
        if got.len() != results.len() {
            return self.fail(
                span,
                format!(
                    "{what}: expected {} results, got {}",
                    results.len(),
                    describe(&got)
                ),
            );
        }
        for (i, (r, g)) in results.iter().zip(&got).enumerate() {
            let WastRet::Core(expected) = r else {
                return self.fail(span, format!("{what}: unsupported result {r:?}"));
            };
            match ret_matches(expected, *g) {
                Ok(true) => {}
                Ok(false) => {
                    return self.fail(
                        span,
                        format!(
                            "{what}: result {i} expected {expected:?}, got {}",
                            describe(&got)
                        ),
                    )
                }
                Err(e) => return self.fail(span, format!("{what}: {e}")),
            }
        }
        self.stats.returns += 1;
    }

    fn assert_trap(&mut self, span: Span, exec: WastExecute<'_>, message: &str) {
        match exec {
            WastExecute::Invoke(invoke) => match self.exec_invoke(&invoke) {
                Ok(Some(ExecResult::Trap(t))) => {
                    if trap_message_matches(message, t) {
                        self.stats.traps += 1;
                    } else {
                        self.fail(
                            span,
                            format!(
                                "assert_trap \"{message}\" on \"{}\": trapped with \"{}\"",
                                invoke.name,
                                t.message()
                            ),
                        );
                    }
                }
                Ok(Some(other)) => self.fail(
                    span,
                    format!(
                        "assert_trap \"{message}\" on \"{}\": got {other:?}",
                        invoke.name
                    ),
                ),
                Ok(None) => self.skip(span, format!("assert_trap \"{}\"", invoke.name)),
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
                            self.store
                                .by_id
                                .insert(id.name().to_owned(), Slot::Live(idx));
                        }
                        self.store.current = Some(Slot::Live(idx));
                        self.fail(
                            span,
                            format!("assert_trap \"{message}\": module instantiated"),
                        );
                    }
                    Err(NotInstantiated::Failed(Error::Trap(t))) => {
                        if trap_message_matches(message, t) {
                            self.stats.traps += 1;
                        } else {
                            self.fail(
                                span,
                                format!(
                                    "assert_trap \"{message}\": instantiation trapped with \"{}\"",
                                    t.message()
                                ),
                            );
                        }
                    }
                    Err(NotInstantiated::Failed(e)) => self.fail(
                        span,
                        format!("assert_trap \"{message}\": instantiation failed with {e}"),
                    ),
                    Err(NotInstantiated::Skipped { why, shares_with }) => {
                        self.stats.skipped_modules += 1;
                        self.skips.push(format!(
                            "{}:{}: assert_trap module skipped: {why}",
                            self.file,
                            self.line(span)
                        ));
                        // It would have applied its segments before trapping.
                        for idx in shares_with {
                            self.store.tainted[idx] = true;
                        }
                    }
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
    skips: Vec<String>,
}

fn run_file(path: &Path) -> FileReport {
    let file = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("?")
        .to_owned();
    let text = std::fs::read_to_string(path).expect("readable spec file");
    let buf = parse_buffer(&text).unwrap_or_else(|e| panic!("{file}: lexing failed: {e}"));
    let wast: Wast = parser::parse(&buf).unwrap_or_else(|e| panic!("{file}: parsing failed: {e}"));

    let mut runner = Runner {
        file: &file,
        text: &text,
        store: Store::default(),
        stats: Stats::default(),
        failures: Vec::new(),
        skips: Vec::new(),
    };
    for directive in wast.directives {
        runner.directive(directive);
    }
    let (stats, failures, skips) = (runner.stats, runner.failures, runner.skips);
    FileReport {
        file,
        stats,
        failures,
        skips,
    }
}

#[test]
fn spec_suite_executes() {
    let files = spec_files();
    assert!(
        files.len() >= 80,
        "expected the vendored spec suite, found {} files",
        files.len()
    );

    let mut total = Stats::default();
    let mut report = String::new();
    let mut failures: Vec<String> = Vec::new();
    let mut unexpected_skips: Vec<String> = Vec::new();
    let mut passed_files = 0usize;
    for path in &files {
        let r = run_file(path);
        total.add(&r.stats);
        let s = &r.stats;
        let status = if !r.failures.is_empty() {
            "FAIL"
        } else if r.stats.skipped_modules > 0 {
            "partial"
        } else {
            passed_files += 1;
            "ok"
        };
        let _ = writeln!(
            report,
            "{status:8} {:32} {:4} modules {:5} returns {:4} traps {:3} skipped modules {:4} skipped directives",
            r.file, s.modules, s.returns, s.traps, s.skipped_modules, s.skipped_directives
        );
        let allowed = FILES_WITH_SKIPS.iter().any(|(f, _)| *f == r.file);
        if !allowed && (s.skipped_modules > 0 || s.skipped_directives > 0) {
            unexpected_skips.extend(r.skips.iter().cloned());
        }
        failures.extend(r.failures);
    }
    let _ = writeln!(
        report,
        "\nspec execution: {} files ({passed_files} complete), {} modules instantiated, \
         {} invocations, {} assert_return, {} assert_trap, {} assert_exhaustion, \
         {} assert_unlinkable, {} malformed/invalid rejected, \
         {} modules skipped, {} directives skipped",
        files.len(),
        total.modules,
        total.invocations,
        total.returns,
        total.traps,
        total.exhaustions,
        total.unlinkable,
        total.rejected,
        total.skipped_modules,
        total.skipped_directives
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
    if !unexpected_skips.is_empty() {
        let _ = writeln!(
            report,
            "\n{} skips in files not listed in FILES_WITH_SKIPS:",
            unexpected_skips.len()
        );
        for s in unexpected_skips.iter().take(50) {
            let _ = writeln!(report, "  {s}");
        }
    }
    println!("{report}");
    assert!(
        failures.is_empty() && unexpected_skips.is_empty(),
        "spec execution failures:\n{report}"
    );
    // Sanity: the suite really was exercised.
    assert!(
        total.returns > 15_000,
        "too few assert_return: {}",
        total.returns
    );
    assert!(total.traps > 500, "too few assert_trap: {}", total.traps);
    assert!(
        total.exhaustions >= 5,
        "too few assert_exhaustion: {}",
        total.exhaustions
    );
}

/// Every entry of `FILES_WITH_SKIPS` must name a vendored file that really
/// has skips, so the list cannot rot.
#[test]
fn files_with_skips_are_accurate() {
    for (file, _) in FILES_WITH_SKIPS {
        let path = spec_dir().join(file);
        assert!(path.exists(), "{file} is not vendored");
        let r = run_file(&path);
        assert!(
            r.stats.skipped_modules > 0,
            "{file} is listed in FILES_WITH_SKIPS but skips nothing"
        );
    }
}
