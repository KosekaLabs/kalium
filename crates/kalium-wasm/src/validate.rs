//! Module validation (step E of the plan).
//!
//! `Module::new` calls [`validate`] after decoding. The decoder guarantees
//! binary well-formedness only; everything in the spec's "valid" category is
//! checked here:
//!
//! - index spaces: every type, function, table, memory, global, local, label,
//!   element- and data-segment index is in range;
//! - limits: table and memory limits are sane, memory is at most 65536 pages,
//!   at most one memory;
//! - imports/exports: import type indices, export indices, duplicate names;
//! - constant expressions (global initializers, segment offsets and element
//!   items): a single `*.const` / `ref.null` / `ref.func` / `global.get` of an
//!   imported immutable global, of the expected type;
//! - the start function has type `[] -> []`;
//! - every function body type-checks with the WebAssembly 2.0 appendix
//!   algorithm: an operand stack that becomes polymorphic after
//!   `unreachable`/`br`/`br_table`/`return`, a control stack of frames with
//!   label types (multi-value block types included), `br_table` arity checks,
//!   `select` typing, reference types, `ref.func` declaration checks,
//!   alignment checks on loads and stores, and the existence of the memory,
//!   tables and segments that bulk operations use.
//!
//! Every error is `Error::Invalid { msg, func, instr }` with spec-test-suite
//! wording; `func` and `instr` locate the failure (function index in the
//! function index space, instruction index in [`FuncBody::instrs`]) when it
//! is inside a function body. Index arithmetic goes through `get`/checked
//! operations, so hostile input never panics.
//!
//! Constant expressions follow the 2.0 rules: `global.get` may only refer to
//! an imported immutable global (the extended-const and 3.0 relaxations are
//! not needed by Rust-emitted modules).

use crate::error::Error;
use crate::instr::{BlockType, ConstExpr, FuncBody, Instr, MemArg};
use crate::module::{DataMode, ElemItems, ElemMode, ExportDesc, ImportDesc, Module};
use crate::types::{FuncType, GlobalType, MemType, RefType, ResizableLimits, TableType, ValType};
use alloc::vec::Vec;

/// Validate a decoded module. Never panics.
pub fn validate(module: &Module) -> Result<(), Error> {
    let ctx = Context::new(module)?;
    ctx.check_limits()?;
    ctx.check_globals()?;
    ctx.check_exports()?;
    ctx.check_start()?;
    ctx.check_elements()?;
    ctx.check_data()?;
    let mut fv = FuncValidator::new(&ctx);
    let first_defined = ctx.num_imported_funcs;
    for (i, body) in module.code.iter().enumerate() {
        let func_idx = u32::try_from(first_defined + i).map_err(|_| invalid("unknown function"))?;
        fv.validate_body(func_idx, body)?;
    }
    Ok(())
}

fn invalid(msg: &'static str) -> Error {
    Error::Invalid {
        msg,
        func: None,
        instr: None,
    }
}

/// The validation context `C` of the spec: the index spaces of the module
/// (imports first) and the set of declared function references.
struct Context<'m> {
    module: &'m Module,
    types: &'m [FuncType],
    /// Type index of every function (imports first).
    funcs: Vec<u32>,
    num_imported_funcs: usize,
    tables: Vec<TableType>,
    memories: Vec<MemType>,
    globals: Vec<GlobalType>,
    num_imported_globals: usize,
    /// Element type of every element segment.
    elems: Vec<RefType>,
    /// Number of data segments.
    datas: usize,
    /// `C.refs`: functions referenced outside function bodies (exports,
    /// element segments, global initializers), one flag per function.
    refs: Vec<bool>,
}

impl<'m> Context<'m> {
    fn new(module: &'m Module) -> Result<Context<'m>, Error> {
        let types = module.types.as_slice();
        let mut funcs = Vec::new();
        let mut tables = Vec::new();
        let mut memories = Vec::new();
        let mut globals = Vec::new();
        for imp in &module.imports {
            match &imp.desc {
                ImportDesc::Func(ty) => {
                    if (*ty as usize) >= types.len() {
                        return Err(invalid("unknown type"));
                    }
                    funcs.push(*ty);
                }
                ImportDesc::Table(t) => tables.push(*t),
                ImportDesc::Memory(m) => memories.push(*m),
                ImportDesc::Global(g) => globals.push(*g),
            }
        }
        let num_imported_funcs = funcs.len();
        let num_imported_globals = globals.len();
        for (i, ty) in module.funcs.iter().enumerate() {
            if (*ty as usize) >= types.len() {
                return Err(Error::Invalid {
                    msg: "unknown type",
                    func: u32::try_from(num_imported_funcs + i).ok(),
                    instr: None,
                });
            }
            funcs.push(*ty);
        }
        tables.extend(module.tables.iter().copied());
        memories.extend(module.memories.iter().copied());
        globals.extend(module.globals.iter().map(|g| g.ty));
        let elems: Vec<RefType> = module.elements.iter().map(|e| e.ty).collect();

        let mut refs = alloc::vec![false; funcs.len()];
        let mut declare = |idx: u32| {
            if let Some(slot) = refs.get_mut(idx as usize) {
                *slot = true;
            }
        };
        for e in &module.exports {
            if let ExportDesc::Func(idx) = e.desc {
                declare(idx);
            }
        }
        for g in &module.globals {
            for instr in &g.init.instrs {
                if let Instr::RefFunc(idx) = instr {
                    declare(*idx);
                }
            }
        }
        for seg in &module.elements {
            match &seg.items {
                ElemItems::Funcs(idxs) => idxs.iter().for_each(|i| declare(*i)),
                ElemItems::Exprs(exprs) => {
                    for e in exprs {
                        for instr in &e.instrs {
                            if let Instr::RefFunc(idx) = instr {
                                declare(*idx);
                            }
                        }
                    }
                }
            }
        }

        Ok(Context {
            module,
            types,
            funcs,
            num_imported_funcs,
            tables,
            memories,
            globals,
            num_imported_globals,
            elems,
            datas: module.data.len(),
            refs,
        })
    }

    fn func_type(&self, func_idx: u32) -> Option<&'m FuncType> {
        let ty = *self.funcs.get(func_idx as usize)?;
        self.types.get(ty as usize)
    }

    // ---- module-level checks ---------------------------------------------

    fn check_limits(&self) -> Result<(), Error> {
        for t in &self.tables {
            check_table_limits(&t.limits)?;
        }
        if self.memories.len() > 1 {
            return Err(invalid("multiple memories"));
        }
        for m in &self.memories {
            check_memory_limits(&m.limits)?;
        }
        Ok(())
    }

    fn check_globals(&self) -> Result<(), Error> {
        for g in &self.module.globals {
            self.const_expr(&g.init, g.ty.ty)?;
        }
        Ok(())
    }

    fn check_exports(&self) -> Result<(), Error> {
        for e in &self.module.exports {
            let ok = match e.desc {
                ExportDesc::Func(i) => (i as usize) < self.funcs.len(),
                ExportDesc::Table(i) => (i as usize) < self.tables.len(),
                ExportDesc::Memory(i) => (i as usize) < self.memories.len(),
                ExportDesc::Global(i) => (i as usize) < self.globals.len(),
            };
            if !ok {
                return Err(invalid(match e.desc {
                    ExportDesc::Func(_) => "unknown function",
                    ExportDesc::Table(_) => "unknown table",
                    ExportDesc::Memory(_) => "unknown memory",
                    ExportDesc::Global(_) => "unknown global",
                }));
            }
        }
        let mut names: Vec<&str> = self
            .module
            .exports
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        names.sort_unstable();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return Err(invalid("duplicate export name"));
        }
        Ok(())
    }

    fn check_start(&self) -> Result<(), Error> {
        if let Some(start) = self.module.start {
            let ty = self
                .func_type(start)
                .ok_or_else(|| invalid("unknown function"))?;
            if !ty.params.is_empty() || !ty.results.is_empty() {
                return Err(invalid("start function"));
            }
        }
        Ok(())
    }

    fn check_elements(&self) -> Result<(), Error> {
        for seg in &self.module.elements {
            if let ElemMode::Active { table, offset } = &seg.mode {
                let tt = self
                    .tables
                    .get(*table as usize)
                    .ok_or_else(|| invalid("unknown table"))?;
                if tt.elem != seg.ty {
                    return Err(invalid("type mismatch"));
                }
                self.const_expr(offset, ValType::I32)?;
            }
            match &seg.items {
                ElemItems::Funcs(idxs) => {
                    if seg.ty != RefType::FuncRef {
                        return Err(invalid("type mismatch"));
                    }
                    for idx in idxs {
                        if (*idx as usize) >= self.funcs.len() {
                            return Err(invalid("unknown function"));
                        }
                    }
                }
                ElemItems::Exprs(exprs) => {
                    for e in exprs {
                        self.const_expr(e, seg.ty.into())?;
                    }
                }
            }
        }
        Ok(())
    }

    fn check_data(&self) -> Result<(), Error> {
        for seg in &self.module.data {
            if let DataMode::Active { memory, offset } = &seg.mode {
                if (*memory as usize) >= self.memories.len() {
                    return Err(invalid("unknown memory"));
                }
                self.const_expr(offset, ValType::I32)?;
            }
        }
        Ok(())
    }

    /// A constant expression must consist of constant instructions only and
    /// evaluate to exactly one value of type `expected`.
    fn const_expr(&self, expr: &ConstExpr, expected: ValType) -> Result<(), Error> {
        let mut stack: Vec<ValType> = Vec::with_capacity(1);
        for instr in &expr.instrs {
            let t = match *instr {
                Instr::I32Const(_) => ValType::I32,
                Instr::I64Const(_) => ValType::I64,
                Instr::F32Const(_) => ValType::F32,
                Instr::F64Const(_) => ValType::F64,
                Instr::RefNull(r) => r.into(),
                Instr::RefFunc(idx) => {
                    if (idx as usize) >= self.funcs.len() {
                        return Err(invalid("unknown function"));
                    }
                    ValType::FuncRef
                }
                Instr::GlobalGet(idx) => {
                    // Only imported globals are in scope for constant
                    // expressions (2.0 rules), and they must be immutable.
                    let idx = idx as usize;
                    let g = self
                        .globals
                        .get(idx)
                        .ok_or_else(|| invalid("unknown global"))?;
                    if idx >= self.num_imported_globals {
                        return Err(invalid("unknown global"));
                    }
                    if g.mutable {
                        return Err(invalid("constant expression required"));
                    }
                    g.ty
                }
                _ => return Err(invalid("constant expression required")),
            };
            stack.push(t);
        }
        if stack.as_slice() != [expected] {
            return Err(invalid("type mismatch"));
        }
        Ok(())
    }
}

fn check_table_limits(l: &ResizableLimits) -> Result<(), Error> {
    if let Some(max) = l.max {
        if max < l.min {
            return Err(invalid("size minimum must not be greater than maximum"));
        }
    }
    Ok(())
}

fn check_memory_limits(l: &ResizableLimits) -> Result<(), Error> {
    if l.min > crate::types::MAX_PAGES || l.max.is_some_and(|m| m > crate::types::MAX_PAGES) {
        return Err(invalid("memory size must be at most 65536 pages (4GiB)"));
    }
    check_table_limits(l)
}

// ---------------------------------------------------------------------------
// Function bodies
// ---------------------------------------------------------------------------

/// What opened a control frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameKind {
    Func,
    Block,
    Loop,
    If,
    Else,
}

/// The type of a control frame, as a view onto the module's types so frames
/// stay `Copy` and allocation-free.
#[derive(Clone, Copy, Debug)]
enum Sig {
    Empty,
    Val(ValType),
    Type(u32),
}

impl Sig {
    fn params<'a>(&'a self, types: &'a [FuncType]) -> &'a [ValType] {
        match self {
            Sig::Empty | Sig::Val(_) => &[],
            // Checked when the frame is pushed; an out-of-range index would
            // only be a bug, and an empty slice is the harmless answer.
            Sig::Type(i) => types.get(*i as usize).map_or(&[], |t| t.params.as_slice()),
        }
    }

    fn results<'a>(&'a self, types: &'a [FuncType]) -> &'a [ValType] {
        match self {
            Sig::Empty => &[],
            Sig::Val(t) => core::slice::from_ref(t),
            Sig::Type(i) => types.get(*i as usize).map_or(&[], |t| t.results.as_slice()),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    kind: FrameKind,
    sig: Sig,
    /// Height of the operand stack when the frame was entered.
    height: usize,
    /// Set after `unreachable`/`br`/`br_table`/`return`: the rest of the
    /// frame is polymorphic.
    unreachable: bool,
}

impl Frame {
    /// The types a branch to this frame's label carries.
    fn label_types<'a>(&'a self, types: &'a [FuncType]) -> &'a [ValType] {
        if self.kind == FrameKind::Loop {
            self.sig.params(types)
        } else {
            self.sig.results(types)
        }
    }
}

/// Type-checks function bodies against a [`Context`]. Reused across the
/// functions of a module so the stacks are allocated once.
struct FuncValidator<'c, 'm> {
    ctx: &'c Context<'m>,
    /// Operand stack; `None` is the polymorphic "unknown" type.
    vals: Vec<Option<ValType>>,
    ctrls: Vec<Frame>,
    func: u32,
    pc: u32,
    params: &'m [ValType],
    locals: &'m [ValType],
}

/// Natural alignment exponents of the memory instructions.
const ALIGN_8: u32 = 0;
const ALIGN_16: u32 = 1;
const ALIGN_32: u32 = 2;
const ALIGN_64: u32 = 3;

impl<'c, 'm> FuncValidator<'c, 'm> {
    fn new(ctx: &'c Context<'m>) -> Self {
        FuncValidator {
            ctx,
            vals: Vec::new(),
            ctrls: Vec::new(),
            func: 0,
            pc: 0,
            params: &[],
            locals: &[],
        }
    }

    fn err(&self, msg: &'static str) -> Error {
        Error::Invalid {
            msg,
            func: Some(self.func),
            instr: Some(self.pc),
        }
    }

    fn validate_body(&mut self, func_idx: u32, body: &'m FuncBody) -> Result<(), Error> {
        self.func = func_idx;
        self.pc = 0;
        self.vals.clear();
        self.ctrls.clear();
        let type_idx = *self
            .ctx
            .funcs
            .get(func_idx as usize)
            .ok_or_else(|| self.err("unknown function"))?;
        let ty = self
            .ctx
            .types
            .get(type_idx as usize)
            .ok_or_else(|| self.err("unknown type"))?;
        self.params = &ty.params;
        self.locals = &body.locals;
        self.ctrls.push(Frame {
            kind: FrameKind::Func,
            sig: Sig::Type(type_idx),
            height: 0,
            unreachable: false,
        });

        for (pc, instr) in body.instrs.iter().enumerate() {
            self.pc = u32::try_from(pc).map_err(|_| self.err("function body too large"))?;
            if self.ctrls.is_empty() {
                // Instructions after the function's own `end`; the decoder
                // never produces this.
                return Err(self.err("operators remaining after end of function"));
            }
            self.instr(*instr, body)?;
        }
        if !self.ctrls.is_empty() {
            // The decoder always terminates a body with `end`.
            return Err(self.err("unexpected end of function"));
        }
        Ok(())
    }

    // ---- operand stack ----------------------------------------------------

    fn top_frame(&self) -> Result<&Frame, Error> {
        self.ctrls.last().ok_or_else(|| self.err("type mismatch"))
    }

    fn push(&mut self, t: ValType) {
        self.vals.push(Some(t));
    }

    fn push_opt(&mut self, t: Option<ValType>) {
        self.vals.push(t);
    }

    fn push_vals(&mut self, types: &[ValType]) {
        self.vals.extend(types.iter().map(|t| Some(*t)));
    }

    /// Pop a value of any type; `None` when the stack is polymorphic.
    fn pop_any(&mut self) -> Result<Option<ValType>, Error> {
        let frame = *self.top_frame()?;
        if self.vals.len() == frame.height {
            if frame.unreachable {
                return Ok(None);
            }
            return Err(self.err("type mismatch"));
        }
        self.vals.pop().ok_or_else(|| self.err("type mismatch"))
    }

    fn pop(&mut self, expect: ValType) -> Result<(), Error> {
        match self.pop_any()? {
            Some(t) if t != expect => Err(self.err("type mismatch")),
            _ => Ok(()),
        }
    }

    fn pop_vals(&mut self, types: &[ValType]) -> Result<(), Error> {
        for t in types.iter().rev() {
            self.pop(*t)?;
        }
        Ok(())
    }

    /// Check that `types` are on top of the stack without popping them,
    /// with the same polymorphic-stack rules as `pop_vals`. Used by
    /// `br_table` (the spec's `push_vals(pop_vals(...))`).
    fn peek_vals(&self, types: &[ValType]) -> Result<(), Error> {
        let frame = self.top_frame()?;
        let avail = self.vals.len() - frame.height;
        for (i, t) in types.iter().rev().enumerate() {
            if i < avail {
                match self.vals.get(self.vals.len() - 1 - i) {
                    Some(Some(actual)) if actual != t => return Err(self.err("type mismatch")),
                    _ => {}
                }
            } else if !frame.unreachable {
                return Err(self.err("type mismatch"));
            }
        }
        Ok(())
    }

    // ---- control stack ----------------------------------------------------

    fn push_ctrl(&mut self, kind: FrameKind, sig: Sig) {
        let height = self.vals.len();
        self.ctrls.push(Frame {
            kind,
            sig,
            height,
            unreachable: false,
        });
        self.push_vals(sig.params(self.ctx.types));
    }

    fn pop_ctrl(&mut self) -> Result<Frame, Error> {
        let frame = *self.top_frame()?;
        self.pop_vals(frame.sig.results(self.ctx.types))?;
        if self.vals.len() != frame.height {
            return Err(self.err("type mismatch"));
        }
        self.ctrls.pop();
        Ok(frame)
    }

    fn unreachable(&mut self) -> Result<(), Error> {
        let err = self.err("type mismatch");
        let frame = self.ctrls.last_mut().ok_or(err)?;
        self.vals.truncate(frame.height);
        frame.unreachable = true;
        Ok(())
    }

    /// The frame `depth` labels out from the innermost one.
    fn label(&self, depth: u32) -> Result<Frame, Error> {
        let idx = self
            .ctrls
            .len()
            .checked_sub(1)
            .and_then(|top| top.checked_sub(depth as usize))
            .ok_or_else(|| self.err("unknown label"))?;
        self.ctrls
            .get(idx)
            .copied()
            .ok_or_else(|| self.err("unknown label"))
    }

    fn block_sig(&self, bt: BlockType) -> Result<Sig, Error> {
        Ok(match bt {
            BlockType::Empty => Sig::Empty,
            BlockType::Val(t) => Sig::Val(t),
            BlockType::Type(i) => {
                if (i as usize) >= self.ctx.types.len() {
                    return Err(self.err("unknown type"));
                }
                Sig::Type(i)
            }
        })
    }

    // ---- context lookups --------------------------------------------------

    fn local(&self, idx: u32) -> Result<ValType, Error> {
        let idx = idx as usize;
        if let Some(t) = self.params.get(idx) {
            return Ok(*t);
        }
        idx.checked_sub(self.params.len())
            .and_then(|i| self.locals.get(i))
            .copied()
            .ok_or_else(|| self.err("unknown local"))
    }

    fn global(&self, idx: u32) -> Result<GlobalType, Error> {
        self.ctx
            .globals
            .get(idx as usize)
            .copied()
            .ok_or_else(|| self.err("unknown global"))
    }

    fn table(&self, idx: u32) -> Result<TableType, Error> {
        self.ctx
            .tables
            .get(idx as usize)
            .copied()
            .ok_or_else(|| self.err("unknown table"))
    }

    fn memory(&self) -> Result<(), Error> {
        if self.ctx.memories.is_empty() {
            return Err(self.err("unknown memory"));
        }
        Ok(())
    }

    fn func_type(&self, idx: u32) -> Result<&'m FuncType, Error> {
        self.ctx
            .func_type(idx)
            .ok_or_else(|| self.err("unknown function"))
    }

    fn elem(&self, idx: u32) -> Result<RefType, Error> {
        self.ctx
            .elems
            .get(idx as usize)
            .copied()
            .ok_or_else(|| self.err("unknown elem segment"))
    }

    fn data(&self, idx: u32) -> Result<(), Error> {
        if (idx as usize) >= self.ctx.datas {
            return Err(self.err("unknown data segment"));
        }
        Ok(())
    }

    // ---- instruction shapes -----------------------------------------------

    fn unop(&mut self, t: ValType) -> Result<(), Error> {
        self.pop(t)?;
        self.push(t);
        Ok(())
    }

    fn binop(&mut self, t: ValType) -> Result<(), Error> {
        self.pop(t)?;
        self.pop(t)?;
        self.push(t);
        Ok(())
    }

    fn testop(&mut self, t: ValType) -> Result<(), Error> {
        self.pop(t)?;
        self.push(ValType::I32);
        Ok(())
    }

    fn relop(&mut self, t: ValType) -> Result<(), Error> {
        self.pop(t)?;
        self.pop(t)?;
        self.push(ValType::I32);
        Ok(())
    }

    fn cvtop(&mut self, from: ValType, to: ValType) -> Result<(), Error> {
        self.pop(from)?;
        self.push(to);
        Ok(())
    }

    fn check_align(&self, m: MemArg, natural: u32) -> Result<(), Error> {
        self.memory()?;
        if m.align > natural {
            return Err(self.err("alignment must not be larger than natural"));
        }
        Ok(())
    }

    fn load(&mut self, t: ValType, natural: u32, m: MemArg) -> Result<(), Error> {
        self.check_align(m, natural)?;
        self.pop(ValType::I32)?;
        self.push(t);
        Ok(())
    }

    fn store(&mut self, t: ValType, natural: u32, m: MemArg) -> Result<(), Error> {
        self.check_align(m, natural)?;
        self.pop(t)?;
        self.pop(ValType::I32)?;
        Ok(())
    }

    fn call_type(&mut self, ty: &FuncType) -> Result<(), Error> {
        self.pop_vals(&ty.params)?;
        self.push_vals(&ty.results);
        Ok(())
    }

    // ---- the instruction switch -------------------------------------------

    fn instr(&mut self, instr: Instr, body: &FuncBody) -> Result<(), Error> {
        use Instr::*;
        use ValType::{F32, F64, I32, I64};
        match instr {
            // ---- control ----
            Unreachable => self.unreachable()?,
            Nop => {}
            Block { ty, .. } => {
                let sig = self.block_sig(ty)?;
                self.pop_vals(sig.params(self.ctx.types))?;
                self.push_ctrl(FrameKind::Block, sig);
            }
            Loop { ty, .. } => {
                let sig = self.block_sig(ty)?;
                self.pop_vals(sig.params(self.ctx.types))?;
                self.push_ctrl(FrameKind::Loop, sig);
            }
            If { ty, .. } => {
                let sig = self.block_sig(ty)?;
                self.pop(I32)?;
                self.pop_vals(sig.params(self.ctx.types))?;
                self.push_ctrl(FrameKind::If, sig);
            }
            Else { .. } => {
                let frame = self.pop_ctrl()?;
                if frame.kind != FrameKind::If {
                    return Err(self.err("unexpected else"));
                }
                self.push_ctrl(FrameKind::Else, frame.sig);
            }
            End => {
                let frame = self.pop_ctrl()?;
                if frame.kind == FrameKind::If {
                    // An `if` without `else` has an implicit empty else arm,
                    // which is only well-typed when params == results.
                    let types = self.ctx.types;
                    if frame.sig.params(types) != frame.sig.results(types) {
                        return Err(self.err("type mismatch"));
                    }
                }
                self.push_vals(frame.sig.results(self.ctx.types));
            }
            Br(depth) => {
                let frame = self.label(depth)?;
                self.pop_vals(frame.label_types(self.ctx.types))?;
                self.unreachable()?;
            }
            BrIf(depth) => {
                let frame = self.label(depth)?;
                self.pop(I32)?;
                let types = frame.label_types(self.ctx.types);
                self.pop_vals(types)?;
                self.push_vals(types);
            }
            BrTable(idx) => {
                let table = body
                    .br_table(idx)
                    .ok_or_else(|| self.err("unknown label"))?;
                self.pop(I32)?;
                let default = self.label(table.default)?;
                let arity = default.label_types(self.ctx.types).len();
                for target in &table.targets {
                    let frame = self.label(*target)?;
                    let types = frame.label_types(self.ctx.types);
                    if types.len() != arity {
                        return Err(self.err("type mismatch"));
                    }
                    self.peek_vals(types)?;
                }
                self.pop_vals(default.label_types(self.ctx.types))?;
                self.unreachable()?;
            }
            Return => {
                let func = *self
                    .ctrls
                    .first()
                    .ok_or_else(|| self.err("type mismatch"))?;
                self.pop_vals(func.sig.results(self.ctx.types))?;
                self.unreachable()?;
            }
            Call(idx) => {
                let ty = self.func_type(idx)?;
                self.call_type(ty)?;
            }
            CallIndirect { ty, table } => {
                let tt = self.table(table)?;
                if tt.elem != RefType::FuncRef {
                    return Err(self.err("type mismatch"));
                }
                let ft = self
                    .ctx
                    .types
                    .get(ty as usize)
                    .ok_or_else(|| self.err("unknown type"))?;
                self.pop(I32)?;
                self.call_type(ft)?;
            }

            // ---- parametric ----
            Drop => {
                self.pop_any()?;
            }
            Select => {
                self.pop(I32)?;
                let t1 = self.pop_any()?;
                let t2 = self.pop_any()?;
                if t1.is_some_and(ValType::is_ref) || t2.is_some_and(ValType::is_ref) {
                    return Err(self.err("type mismatch"));
                }
                if let (Some(a), Some(b)) = (t1, t2) {
                    if a != b {
                        return Err(self.err("type mismatch"));
                    }
                }
                self.push_opt(t1.or(t2));
            }
            SelectT(t) => {
                self.pop(I32)?;
                self.pop(t)?;
                self.pop(t)?;
                self.push(t);
            }

            // ---- variable ----
            LocalGet(idx) => {
                let t = self.local(idx)?;
                self.push(t);
            }
            LocalSet(idx) => {
                let t = self.local(idx)?;
                self.pop(t)?;
            }
            LocalTee(idx) => {
                let t = self.local(idx)?;
                self.pop(t)?;
                self.push(t);
            }
            GlobalGet(idx) => {
                let g = self.global(idx)?;
                self.push(g.ty);
            }
            GlobalSet(idx) => {
                let g = self.global(idx)?;
                if !g.mutable {
                    return Err(self.err("global is immutable"));
                }
                self.pop(g.ty)?;
            }

            // ---- reference ----
            RefNull(r) => self.push(r.into()),
            RefIsNull => {
                if let Some(t) = self.pop_any()? {
                    if !t.is_ref() {
                        return Err(self.err("type mismatch"));
                    }
                }
                self.push(I32);
            }
            RefFunc(idx) => {
                self.func_type(idx)?;
                if !self.ctx.refs.get(idx as usize).copied().unwrap_or(false) {
                    return Err(self.err("undeclared function reference"));
                }
                self.push(ValType::FuncRef);
            }

            // ---- table ----
            TableGet(t) => {
                let tt = self.table(t)?;
                self.pop(I32)?;
                self.push(tt.elem.into());
            }
            TableSet(t) => {
                let tt = self.table(t)?;
                self.pop(tt.elem.into())?;
                self.pop(I32)?;
            }
            TableInit { elem, table } => {
                let tt = self.table(table)?;
                let et = self.elem(elem)?;
                if tt.elem != et {
                    return Err(self.err("type mismatch"));
                }
                self.pop(I32)?;
                self.pop(I32)?;
                self.pop(I32)?;
            }
            ElemDrop(elem) => {
                self.elem(elem)?;
            }
            TableCopy { dst, src } => {
                let d = self.table(dst)?;
                let s = self.table(src)?;
                if d.elem != s.elem {
                    return Err(self.err("type mismatch"));
                }
                self.pop(I32)?;
                self.pop(I32)?;
                self.pop(I32)?;
            }
            TableGrow(t) => {
                let tt = self.table(t)?;
                self.pop(I32)?;
                self.pop(tt.elem.into())?;
                self.push(I32);
            }
            TableSize(t) => {
                self.table(t)?;
                self.push(I32);
            }
            TableFill(t) => {
                let tt = self.table(t)?;
                self.pop(I32)?;
                self.pop(tt.elem.into())?;
                self.pop(I32)?;
            }

            // ---- memory ----
            I32Load(m) => self.load(I32, ALIGN_32, m)?,
            I64Load(m) => self.load(I64, ALIGN_64, m)?,
            F32Load(m) => self.load(F32, ALIGN_32, m)?,
            F64Load(m) => self.load(F64, ALIGN_64, m)?,
            I32Load8S(m) | I32Load8U(m) => self.load(I32, ALIGN_8, m)?,
            I32Load16S(m) | I32Load16U(m) => self.load(I32, ALIGN_16, m)?,
            I64Load8S(m) | I64Load8U(m) => self.load(I64, ALIGN_8, m)?,
            I64Load16S(m) | I64Load16U(m) => self.load(I64, ALIGN_16, m)?,
            I64Load32S(m) | I64Load32U(m) => self.load(I64, ALIGN_32, m)?,
            I32Store(m) => self.store(I32, ALIGN_32, m)?,
            I64Store(m) => self.store(I64, ALIGN_64, m)?,
            F32Store(m) => self.store(F32, ALIGN_32, m)?,
            F64Store(m) => self.store(F64, ALIGN_64, m)?,
            I32Store8(m) => self.store(I32, ALIGN_8, m)?,
            I32Store16(m) => self.store(I32, ALIGN_16, m)?,
            I64Store8(m) => self.store(I64, ALIGN_8, m)?,
            I64Store16(m) => self.store(I64, ALIGN_16, m)?,
            I64Store32(m) => self.store(I64, ALIGN_32, m)?,
            MemorySize => {
                self.memory()?;
                self.push(I32);
            }
            MemoryGrow => {
                self.memory()?;
                self.pop(I32)?;
                self.push(I32);
            }
            MemoryInit(d) => {
                self.memory()?;
                self.data(d)?;
                self.pop(I32)?;
                self.pop(I32)?;
                self.pop(I32)?;
            }
            DataDrop(d) => self.data(d)?,
            MemoryCopy | MemoryFill => {
                self.memory()?;
                self.pop(I32)?;
                self.pop(I32)?;
                self.pop(I32)?;
            }

            // ---- constants ----
            I32Const(_) => self.push(I32),
            I64Const(_) => self.push(I64),
            F32Const(_) => self.push(F32),
            F64Const(_) => self.push(F64),

            // ---- i32 ----
            I32Eqz => self.testop(I32)?,
            I32Eq | I32Ne | I32LtS | I32LtU | I32GtS | I32GtU | I32LeS | I32LeU | I32GeS
            | I32GeU => self.relop(I32)?,
            I32Clz | I32Ctz | I32Popcnt => self.unop(I32)?,
            I32Add | I32Sub | I32Mul | I32DivS | I32DivU | I32RemS | I32RemU | I32And | I32Or
            | I32Xor | I32Shl | I32ShrS | I32ShrU | I32Rotl | I32Rotr => self.binop(I32)?,

            // ---- i64 ----
            I64Eqz => self.testop(I64)?,
            I64Eq | I64Ne | I64LtS | I64LtU | I64GtS | I64GtU | I64LeS | I64LeU | I64GeS
            | I64GeU => self.relop(I64)?,
            I64Clz | I64Ctz | I64Popcnt => self.unop(I64)?,
            I64Add | I64Sub | I64Mul | I64DivS | I64DivU | I64RemS | I64RemU | I64And | I64Or
            | I64Xor | I64Shl | I64ShrS | I64ShrU | I64Rotl | I64Rotr => self.binop(I64)?,

            // ---- f32 ----
            F32Eq | F32Ne | F32Lt | F32Gt | F32Le | F32Ge => self.relop(F32)?,
            F32Abs | F32Neg | F32Ceil | F32Floor | F32Trunc | F32Nearest | F32Sqrt => {
                self.unop(F32)?
            }
            F32Add | F32Sub | F32Mul | F32Div | F32Min | F32Max | F32Copysign => self.binop(F32)?,

            // ---- f64 ----
            F64Eq | F64Ne | F64Lt | F64Gt | F64Le | F64Ge => self.relop(F64)?,
            F64Abs | F64Neg | F64Ceil | F64Floor | F64Trunc | F64Nearest | F64Sqrt => {
                self.unop(F64)?
            }
            F64Add | F64Sub | F64Mul | F64Div | F64Min | F64Max | F64Copysign => self.binop(F64)?,

            // ---- conversions ----
            I32WrapI64 => self.cvtop(I64, I32)?,
            I32TruncF32S | I32TruncF32U | I32TruncSatF32S | I32TruncSatF32U | I32ReinterpretF32 => {
                self.cvtop(F32, I32)?
            }
            I32TruncF64S | I32TruncF64U | I32TruncSatF64S | I32TruncSatF64U => {
                self.cvtop(F64, I32)?
            }
            I64ExtendI32S | I64ExtendI32U => self.cvtop(I32, I64)?,
            I64TruncF32S | I64TruncF32U | I64TruncSatF32S | I64TruncSatF32U => {
                self.cvtop(F32, I64)?
            }
            I64TruncF64S | I64TruncF64U | I64TruncSatF64S | I64TruncSatF64U | I64ReinterpretF64 => {
                self.cvtop(F64, I64)?
            }
            F32ConvertI32S | F32ConvertI32U | F32ReinterpretI32 => self.cvtop(I32, F32)?,
            F32ConvertI64S | F32ConvertI64U => self.cvtop(I64, F32)?,
            F32DemoteF64 => self.cvtop(F64, F32)?,
            F64ConvertI32S | F64ConvertI32U => self.cvtop(I32, F64)?,
            F64ConvertI64S | F64ConvertI64U | F64ReinterpretI64 => self.cvtop(I64, F64)?,
            F64PromoteF32 => self.cvtop(F32, F64)?,

            // ---- sign extension ----
            I32Extend8S | I32Extend16S => self.unop(I32)?,
            I64Extend8S | I64Extend16S | I64Extend32S => self.unop(I64)?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instr::BrTable;
    use crate::module::{Export, Global};
    use alloc::string::String;
    use alloc::vec;

    fn func_module(ty: FuncType, locals: Vec<ValType>, instrs: Vec<Instr>) -> Module {
        Module {
            types: vec![ty],
            funcs: vec![0],
            code: vec![FuncBody {
                locals,
                instrs,
                br_tables: Vec::new(),
            }],
            ..Module::default()
        }
    }

    fn msg(r: Result<(), Error>) -> Option<&'static str> {
        r.err().map(|e| e.message())
    }

    #[test]
    fn empty_module_is_valid() {
        assert_eq!(validate(&Module::default()), Ok(()));
    }

    #[test]
    fn body_errors_carry_location() {
        let m = func_module(
            FuncType::default(),
            vec![],
            vec![
                Instr::I32Const(1),
                Instr::I64Const(2),
                Instr::I32Add,
                Instr::End,
            ],
        );
        assert_eq!(
            validate(&m),
            Err(Error::Invalid {
                msg: "type mismatch",
                func: Some(0),
                instr: Some(2),
            })
        );
    }

    #[test]
    fn imported_funcs_shift_indices() {
        let m = Module {
            types: vec![FuncType::default()],
            imports: vec![crate::module::Import {
                module: String::from("m"),
                name: String::from("f"),
                desc: ImportDesc::Func(0),
            }],
            funcs: vec![0],
            code: vec![FuncBody {
                locals: vec![],
                instrs: vec![Instr::LocalGet(0), Instr::End],
                br_tables: Vec::new(),
            }],
            ..Module::default()
        };
        assert_eq!(
            validate(&m),
            Err(Error::Invalid {
                msg: "unknown local",
                func: Some(1),
                instr: Some(0),
            })
        );
    }

    #[test]
    fn missing_br_table_entry_is_not_a_panic() {
        // A `BrTable` index with no side-table entry can only come from a
        // hand-built module; it must be an error, not a panic.
        let m = func_module(
            FuncType::default(),
            vec![],
            vec![Instr::BrTable(7), Instr::End],
        );
        assert_eq!(msg(validate(&m)), Some("unknown label"));
        let mut m = func_module(
            FuncType::default(),
            vec![],
            vec![Instr::I32Const(0), Instr::BrTable(0), Instr::End],
        );
        m.code[0].br_tables.push(BrTable {
            targets: vec![0],
            default: 0,
        });
        assert_eq!(validate(&m), Ok(()));
    }

    #[test]
    fn stray_end_and_missing_end_are_errors() {
        let m = func_module(FuncType::default(), vec![], vec![Instr::End, Instr::Nop]);
        assert_eq!(
            msg(validate(&m)),
            Some("operators remaining after end of function")
        );
        let m = func_module(FuncType::default(), vec![], vec![Instr::Nop]);
        assert_eq!(msg(validate(&m)), Some("unexpected end of function"));
        let m = func_module(
            FuncType::default(),
            vec![],
            vec![Instr::Else { end: 1 }, Instr::End],
        );
        assert_eq!(msg(validate(&m)), Some("unexpected else"));
    }

    #[test]
    fn hand_built_module_level_errors() {
        let m = Module {
            funcs: vec![3],
            code: vec![FuncBody::default()],
            ..Module::default()
        };
        assert_eq!(
            validate(&m),
            Err(Error::Invalid {
                msg: "unknown type",
                func: Some(0),
                instr: None
            })
        );
        let m = Module {
            exports: vec![Export {
                name: String::from("g"),
                desc: ExportDesc::Global(0),
            }],
            ..Module::default()
        };
        assert_eq!(msg(validate(&m)), Some("unknown global"));
        let m = Module {
            globals: vec![Global {
                ty: GlobalType {
                    ty: ValType::I32,
                    mutable: false,
                },
                init: ConstExpr { instrs: vec![] },
            }],
            ..Module::default()
        };
        assert_eq!(msg(validate(&m)), Some("type mismatch"));
    }
}
