//! Runtime values and the resumable stack-machine interpreter.
//!
//! # Design
//!
//! [`Execution`] owns the value stack, the call frames and the label stack.
//! Wasm calls never recurse on the Rust stack: a call pushes a frame and the
//! loop continues in the callee, so call depth is bounded by
//! [`Limits::max_call_depth`] (`Trap::CallDepth`) and the value stack by
//! [`Limits::max_stack_values`] (`Trap::StackOverflow`).
//!
//! Locals live on the value stack. When a function is entered its arguments
//! are already on top of the stack and become the first locals; the declared
//! locals are pushed after them and the frame's operand stack grows above
//! that. Returning moves the results down to the frame base.
//!
//! A label records the continuation pc, the number of values a branch
//! carries and the stack height to unwind to. Structured instructions carry
//! their matching `end`/`else` indices (see [`crate::instr`]), so branching
//! is a truncate-and-jump without scanning. Branching to a `loop` label
//! jumps to the `loop` instruction itself, which pushes its label again.
//!
//! Fuel is one unit per instruction, checked before each instruction, so
//! [`Execution::run`] returns `OutOfFuel` promptly and can be resumed.
//!
//! Host calls see their arguments as a slice of the stack; on
//! [`HostOutcome::Block`] nothing is popped, anything the instruction popped
//! before the call (the table index of `call_indirect`) is pushed back, and
//! the pc is rewound to the call instruction so that resuming re-issues the
//! call with the stack exactly as it was.
//!
//! The interpreter assumes a validated module but never trusts it: any
//! state a valid module cannot produce (type confusion on the stack, an
//! index out of range, ...) ends the execution with [`Trap::Invalid`]
//! instead of panicking.

use crate::error::{Error, Trap};
use crate::instance::{Host, HostCtx, HostOutcome, Instance};
use crate::instr::{BlockType, FuncBody, Instr, MemArg};
use crate::module::Module;
use crate::types::{RefType, ValType};
use alloc::vec::Vec;
use core::fmt;

/// A runtime value. References are indices (function index for `FuncRef`,
/// host-defined for `ExternRef`), `None` being null.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    FuncRef(Option<u32>),
    ExternRef(Option<u32>),
}

impl Value {
    /// The type of this value.
    pub fn ty(&self) -> ValType {
        match self {
            Value::I32(_) => ValType::I32,
            Value::I64(_) => ValType::I64,
            Value::F32(_) => ValType::F32,
            Value::F64(_) => ValType::F64,
            Value::FuncRef(_) => ValType::FuncRef,
            Value::ExternRef(_) => ValType::ExternRef,
        }
    }

    /// The default (zero / null) value of a type, as locals are initialized.
    pub fn default_for(ty: ValType) -> Value {
        match ty {
            ValType::I32 => Value::I32(0),
            ValType::I64 => Value::I64(0),
            ValType::F32 => Value::F32(0.0),
            ValType::F64 => Value::F64(0.0),
            ValType::FuncRef => Value::FuncRef(None),
            ValType::ExternRef => Value::ExternRef(None),
        }
    }

    /// The null reference of a reference type.
    pub fn null(ty: RefType) -> Value {
        match ty {
            RefType::FuncRef => Value::FuncRef(None),
            RefType::ExternRef => Value::ExternRef(None),
        }
    }

    /// Whether this is a null reference (false for non-reference values).
    pub fn is_null_ref(&self) -> bool {
        matches!(self, Value::FuncRef(None) | Value::ExternRef(None))
    }

    pub fn as_i32(&self) -> Option<i32> {
        match self {
            Value::I32(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::I64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_f32(&self) -> Option<f32> {
        match self {
            Value::F32(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::F64(v) => Some(*v),
            _ => None,
        }
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Value {
        Value::I32(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Value {
        Value::I64(v)
    }
}

impl From<f32> for Value {
    fn from(v: f32) -> Value {
        Value::F32(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Value {
        Value::F64(v)
    }
}

/// Values the typed stack helpers pop and push.
trait StackVal: Copy {
    fn from_value(v: Value) -> Option<Self>;
    fn into_value(self) -> Value;
}

impl StackVal for i32 {
    fn from_value(v: Value) -> Option<i32> {
        v.as_i32()
    }
    fn into_value(self) -> Value {
        Value::I32(self)
    }
}

impl StackVal for i64 {
    fn from_value(v: Value) -> Option<i64> {
        v.as_i64()
    }
    fn into_value(self) -> Value {
        Value::I64(self)
    }
}

impl StackVal for f32 {
    fn from_value(v: Value) -> Option<f32> {
        v.as_f32()
    }
    fn into_value(self) -> Value {
        Value::F32(self)
    }
}

impl StackVal for f64 {
    fn from_value(v: Value) -> Option<f64> {
        v.as_f64()
    }
    fn into_value(self) -> Value {
        Value::F64(self)
    }
}

impl StackVal for bool {
    fn from_value(v: Value) -> Option<bool> {
        v.as_i32().map(|x| x != 0)
    }
    fn into_value(self) -> Value {
        Value::I32(self as i32)
    }
}

impl StackVal for Value {
    fn from_value(v: Value) -> Option<Value> {
        Some(v)
    }
    fn into_value(self) -> Value {
        self
    }
}

/// How a call to [`Execution::run`] ended.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecResult {
    /// The function returned these results.
    Done(Vec<Value>),
    /// The fuel budget was spent; call `run` again to continue.
    OutOfFuel,
    /// A host call returned `HostOutcome::Block`; call `run` again to retry
    /// it once the host is ready.
    Blocked,
    /// A host call returned `HostOutcome::Exit(code)`.
    Exited(i32),
    /// The execution trapped and cannot be resumed.
    Trap(Trap),
}

/// A call frame.
#[derive(Clone, Copy, Debug)]
struct Frame {
    /// Function index.
    func: u32,
    /// Saved pc: the resume point while another frame is active, or the
    /// point to resume at after `OutOfFuel`/`Blocked`.
    pc: u32,
    /// Stack index of local 0.
    base: usize,
    /// Index into the label stack of this frame's first label.
    label_base: usize,
    /// Number of results.
    arity: u32,
}

/// A control label.
#[derive(Clone, Copy, Debug)]
struct Label {
    /// pc to continue at when branching to this label.
    cont: u32,
    /// Number of values a branch carries to the continuation.
    arity: u32,
    /// Stack height to unwind to before pushing the carried values.
    height: usize,
}

#[derive(Clone, Debug)]
enum State {
    Running,
    Finished(ExecResult),
}

/// Where control goes after a branch.
enum Jump {
    To(u32),
    Return,
}

/// How `run_inner` stopped.
enum Flow {
    Done,
    OutOfFuel,
    Blocked,
    Exited(i32),
}

/// A resumable execution of one function: value stack, call frames and
/// program counter. See the module documentation.
pub struct Execution {
    stack: Vec<Value>,
    frames: Vec<Frame>,
    labels: Vec<Label>,
    state: State,
    max_stack: usize,
    max_depth: usize,
    /// Reused buffer for host call results.
    host_results: Vec<Value>,
    /// Set when the entry function is an import: the whole execution is one
    /// host call with the arguments on the stack (and no frame).
    entry_import: Option<u32>,
}

impl fmt::Debug for Execution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Execution")
            .field("stack_len", &self.stack.len())
            .field("call_depth", &self.frames.len())
            .field("labels", &self.labels.len())
            .field("finished", &matches!(self.state, State::Finished(_)))
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Numeric helpers with spec semantics
// ---------------------------------------------------------------------------

const F32_QUIET_BIT: u32 = 0x0040_0000;
const F64_QUIET_BIT: u64 = 0x0008_0000_0000_0000;

/// A NaN with the quiet bit set (payload and sign preserved).
fn quiet_f32(x: f32) -> f32 {
    f32::from_bits(x.to_bits() | F32_QUIET_BIT)
}

fn quiet_f64(x: f64) -> f64 {
    f64::from_bits(x.to_bits() | F64_QUIET_BIT)
}

fn f32_abs(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & 0x7fff_ffff)
}

fn f64_abs(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & 0x7fff_ffff_ffff_ffff)
}

fn f32_neg(x: f32) -> f32 {
    f32::from_bits(x.to_bits() ^ 0x8000_0000)
}

fn f64_neg(x: f64) -> f64 {
    f64::from_bits(x.to_bits() ^ 0x8000_0000_0000_0000)
}

fn f32_copysign(a: f32, b: f32) -> f32 {
    f32::from_bits((a.to_bits() & 0x7fff_ffff) | (b.to_bits() & 0x8000_0000))
}

fn f64_copysign(a: f64, b: f64) -> f64 {
    f64::from_bits((a.to_bits() & 0x7fff_ffff_ffff_ffff) | (b.to_bits() & 0x8000_0000_0000_0000))
}

/// `fmin`: NaN if either operand is NaN, `-0` for `min(-0, +0)`.
fn f32_min(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        quiet_f32(a)
    } else if b.is_nan() {
        quiet_f32(b)
    } else if a == b {
        if a.is_sign_negative() {
            a
        } else {
            b
        }
    } else if a < b {
        a
    } else {
        b
    }
}

fn f32_max(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        quiet_f32(a)
    } else if b.is_nan() {
        quiet_f32(b)
    } else if a == b {
        if a.is_sign_positive() {
            a
        } else {
            b
        }
    } else if a > b {
        a
    } else {
        b
    }
}

fn f64_min(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        quiet_f64(a)
    } else if b.is_nan() {
        quiet_f64(b)
    } else if a == b {
        if a.is_sign_negative() {
            a
        } else {
            b
        }
    } else if a < b {
        a
    } else {
        b
    }
}

fn f64_max(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        quiet_f64(a)
    } else if b.is_nan() {
        quiet_f64(b)
    } else if a == b {
        if a.is_sign_positive() {
            a
        } else {
            b
        }
    } else if a > b {
        a
    } else {
        b
    }
}

/// Apply a rounding function, quieting NaN inputs.
fn round_f32(x: f32, f: fn(f32) -> f32) -> f32 {
    if x.is_nan() {
        quiet_f32(x)
    } else {
        f(x)
    }
}

fn round_f64(x: f64, f: fn(f64) -> f64) -> f64 {
    if x.is_nan() {
        quiet_f64(x)
    } else {
        f(x)
    }
}

fn f32_sqrt(x: f32) -> f32 {
    if x.is_nan() {
        quiet_f32(x)
    } else if x < 0.0 {
        f32::NAN
    } else {
        libm::sqrtf(x)
    }
}

fn f64_sqrt(x: f64) -> f64 {
    if x.is_nan() {
        quiet_f64(x)
    } else if x < 0.0 {
        f64::NAN
    } else {
        libm::sqrt(x)
    }
}

fn f32_demote(x: f64) -> f32 {
    if x.is_nan() {
        f32::NAN
    } else {
        x as f32
    }
}

fn f64_promote(x: f32) -> f64 {
    if x.is_nan() {
        f64::NAN
    } else {
        x as f64
    }
}

/// Trapping float-to-integer truncation. `lo`/`hi` bound the truncated
/// value: valid iff `lo < trunc(x) < hi`, both bounds exactly representable.
macro_rules! trunc_trap {
    ($name:ident, $from:ty, $to:ty, $trunc:path, $lo:expr, $hi:expr) => {
        fn $name(x: $from) -> Result<$to, Trap> {
            if x.is_nan() {
                return Err(Trap::InvalidConversion);
            }
            let t = $trunc(x);
            if t > $lo && t < $hi {
                Ok(t as $to)
            } else {
                Err(Trap::IntegerOverflow)
            }
        }
    };
}

trunc_trap!(
    i32_trunc_f32_s,
    f32,
    i32,
    libm::truncf,
    -2147483904.0,
    2147483648.0
);
trunc_trap!(i32_trunc_f32_u, f32, u32, libm::truncf, -1.0, 4294967296.0);
trunc_trap!(
    i32_trunc_f64_s,
    f64,
    i32,
    libm::trunc,
    -2147483649.0,
    2147483648.0
);
trunc_trap!(i32_trunc_f64_u, f64, u32, libm::trunc, -1.0, 4294967296.0);
trunc_trap!(
    i64_trunc_f32_s,
    f32,
    i64,
    libm::truncf,
    -9223373136366403584.0,
    9223372036854775808.0
);
trunc_trap!(
    i64_trunc_f32_u,
    f32,
    u64,
    libm::truncf,
    -1.0,
    18446744073709551616.0
);
trunc_trap!(
    i64_trunc_f64_s,
    f64,
    i64,
    libm::trunc,
    -9223372036854777856.0,
    9223372036854775808.0
);
trunc_trap!(
    i64_trunc_f64_u,
    f64,
    u64,
    libm::trunc,
    -1.0,
    18446744073709551616.0
);

fn effective_address(addr: u32, m: MemArg) -> Result<u32, Trap> {
    u32::try_from(addr as u64 + m.offset as u64).map_err(|_| Trap::OutOfBounds)
}

fn block_arity(module: &Module, ty: BlockType) -> Result<(usize, usize), Trap> {
    Ok(match ty {
        BlockType::Empty => (0, 0),
        BlockType::Val(_) => (0, 1),
        BlockType::Type(idx) => {
            let t = module.types.get(idx as usize).ok_or(Trap::Invalid)?;
            (t.params.len(), t.results.len())
        }
    })
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

impl Execution {
    /// Prepare a call of function `func` (function index space) with `args`.
    /// Fails with `Error::NoSuchFunction` (unknown index) or
    /// `Error::ArgumentMismatch`.
    ///
    /// `func` may be an imported function (a module may export one, or use
    /// one as its start function): the execution then consists of that one
    /// host call, with the same `Block`/`Exit`/trap behaviour as a call from
    /// Wasm code.
    pub fn new(inst: &Instance, func: u32, args: &[Value]) -> Result<Execution, Error> {
        let ty = inst.func_type(func).ok_or(Error::NoSuchFunction { func })?;
        if args.len() != ty.params.len() || args.iter().zip(&ty.params).any(|(a, p)| a.ty() != *p) {
            return Err(Error::ArgumentMismatch);
        }
        let limits = inst.limits();
        let mut exec = Execution {
            stack: Vec::new(),
            frames: Vec::new(),
            labels: Vec::new(),
            state: State::Running,
            max_stack: limits.max_stack_values,
            max_depth: limits.max_call_depth,
            host_results: Vec::new(),
            entry_import: None,
        };
        let setup = (|| {
            for &a in args {
                exec.push(a)?;
            }
            if inst.is_imported_func(func) {
                exec.entry_import = Some(func);
                Ok(())
            } else {
                exec.push_frame(inst.module(), inst, func)
            }
        })();
        match setup {
            Ok(()) => Ok(exec),
            Err(Trap::CallDepth) => Err(Error::Limit {
                msg: "call depth limit too small",
            }),
            Err(Trap::StackOverflow) => Err(Error::Limit {
                msg: "value stack limit too small",
            }),
            Err(_) => Err(Error::NoSuchFunction { func }),
        }
    }

    /// Prepare a run of the module's start function, if it has one.
    pub fn start(inst: &Instance) -> Result<Option<Execution>, Error> {
        match inst.start_function() {
            None => Ok(None),
            Some(f) => Execution::new(inst, f, &[]).map(Some),
        }
    }

    /// Whether the execution has ended (`run` will return its final result
    /// again without executing anything).
    pub fn is_finished(&self) -> bool {
        matches!(self.state, State::Finished(_))
    }

    /// Current depth of the call stack.
    pub fn call_depth(&self) -> usize {
        self.frames.len()
    }

    /// Current number of values on the stack (locals included).
    pub fn stack_len(&self) -> usize {
        self.stack.len()
    }

    /// Run for at most `fuel` instructions (1 unit per instruction). Returns
    /// promptly with `OutOfFuel` when the budget is spent; the execution can
    /// be resumed with another call. After `Done`, `Exited` or `Trap`, every
    /// further call returns that same result.
    pub fn run(&mut self, inst: &mut Instance, host: &mut dyn Host, fuel: u64) -> ExecResult {
        if let State::Finished(r) = &self.state {
            return r.clone();
        }
        let module = inst.module().clone();
        let flow = match self.entry_import {
            Some(f) => self.run_entry_import(&module, inst, host, f, fuel),
            None => self.run_inner(&module, inst, host, fuel),
        };
        let result = match flow {
            Ok(Flow::OutOfFuel) => return ExecResult::OutOfFuel,
            Ok(Flow::Blocked) => return ExecResult::Blocked,
            Ok(Flow::Done) => ExecResult::Done(core::mem::take(&mut self.stack)),
            Ok(Flow::Exited(code)) => ExecResult::Exited(code),
            Err(trap) => ExecResult::Trap(trap),
        };
        self.stack = Vec::new();
        self.frames = Vec::new();
        self.labels = Vec::new();
        self.state = State::Finished(result.clone());
        result
    }

    /// An execution whose entry function is an import: one host call with
    /// the arguments on the stack.
    fn run_entry_import(
        &mut self,
        module: &Module,
        inst: &mut Instance,
        host: &mut dyn Host,
        func: u32,
        fuel: u64,
    ) -> Result<Flow, Trap> {
        if fuel == 0 {
            return Ok(Flow::OutOfFuel);
        }
        Ok(match self.host_call(module, inst, host, func)? {
            HostOutcome::Continue => Flow::Done,
            HostOutcome::Block => Flow::Blocked,
            HostOutcome::Exit(code) => Flow::Exited(code),
        })
    }

    // ---- stack helpers --------------------------------------------------

    fn push(&mut self, v: Value) -> Result<(), Trap> {
        if self.stack.len() >= self.max_stack {
            return Err(Trap::StackOverflow);
        }
        self.stack.push(v);
        Ok(())
    }

    fn pop(&mut self) -> Result<Value, Trap> {
        self.stack.pop().ok_or(Trap::Invalid)
    }

    fn pop_as<T: StackVal>(&mut self) -> Result<T, Trap> {
        T::from_value(self.pop()?).ok_or(Trap::Invalid)
    }

    fn un<A: StackVal, R: StackVal>(&mut self, f: impl FnOnce(A) -> R) -> Result<(), Trap> {
        let a = self.pop_as::<A>()?;
        self.push(f(a).into_value())
    }

    fn un_try<A: StackVal, R: StackVal>(
        &mut self,
        f: impl FnOnce(A) -> Result<R, Trap>,
    ) -> Result<(), Trap> {
        let a = self.pop_as::<A>()?;
        self.push(f(a)?.into_value())
    }

    fn bin<A: StackVal, B: StackVal, R: StackVal>(
        &mut self,
        f: impl FnOnce(A, B) -> R,
    ) -> Result<(), Trap> {
        let b = self.pop_as::<B>()?;
        let a = self.pop_as::<A>()?;
        self.push(f(a, b).into_value())
    }

    fn bin_try<A: StackVal, B: StackVal, R: StackVal>(
        &mut self,
        f: impl FnOnce(A, B) -> Result<R, Trap>,
    ) -> Result<(), Trap> {
        let b = self.pop_as::<B>()?;
        let a = self.pop_as::<A>()?;
        self.push(f(a, b)?.into_value())
    }

    fn load<const N: usize>(&mut self, inst: &Instance, m: MemArg) -> Result<[u8; N], Trap> {
        let addr = self.pop_as::<i32>()? as u32;
        let off = effective_address(addr, m)?;
        let mut buf = [0u8; N];
        inst.memory.read(off, &mut buf)?;
        Ok(buf)
    }

    /// Store `bytes` at the address on the stack (the value must already have
    /// been popped).
    fn store(&mut self, inst: &mut Instance, m: MemArg, bytes: &[u8]) -> Result<(), Trap> {
        let addr = self.pop_as::<i32>()? as u32;
        let off = effective_address(addr, m)?;
        inst.memory.write(off, bytes)
    }

    // ---- frames and labels ----------------------------------------------

    fn body_of<'m>(module: &'m Module, inst: &Instance, func: u32) -> Result<&'m FuncBody, Trap> {
        let defined = (func as usize)
            .checked_sub(inst.num_imported_funcs())
            .ok_or(Trap::Invalid)?;
        module.code.get(defined).ok_or(Trap::Invalid)
    }

    /// Enter defined function `func`; its arguments are the top of the stack.
    fn push_frame(&mut self, module: &Module, inst: &Instance, func: u32) -> Result<(), Trap> {
        if self.frames.len() >= self.max_depth {
            return Err(Trap::CallDepth);
        }
        let ty = inst.func_type(func).ok_or(Trap::Invalid)?;
        let body = Self::body_of(module, inst, func)?;
        let base = self
            .stack
            .len()
            .checked_sub(ty.params.len())
            .ok_or(Trap::Invalid)?;
        let arity = ty.results.len() as u32;
        for &l in &body.locals {
            self.push(Value::default_for(l))?;
        }
        self.frames.push(Frame {
            func,
            pc: 0,
            base,
            label_base: self.labels.len(),
            arity,
        });
        Ok(())
    }

    /// Leave the current frame, moving its results to the frame base.
    /// Returns the index of the caller's frame, or `None` if this was the
    /// outermost one.
    fn do_return(&mut self) -> Result<Option<usize>, Trap> {
        let frame = self.frames.pop().ok_or(Trap::Invalid)?;
        let n = frame.arity as usize;
        let len = self.stack.len();
        if len < n || frame.base > len - n {
            return Err(Trap::Invalid);
        }
        self.stack.copy_within(len - n..len, frame.base);
        self.stack.truncate(frame.base + n);
        self.labels.truncate(frame.label_base);
        Ok(self.frames.len().checked_sub(1))
    }

    fn push_label(&mut self, cont: u32, arity: usize, nparams: usize) -> Result<(), Trap> {
        if self.labels.len() >= self.max_stack {
            return Err(Trap::StackOverflow);
        }
        let height = self.stack.len().checked_sub(nparams).ok_or(Trap::Invalid)?;
        self.labels.push(Label {
            cont,
            arity: arity as u32,
            height,
        });
        Ok(())
    }

    /// Branch to the label `depth` levels out in frame `fi`.
    fn branch(&mut self, fi: usize, depth: u32) -> Result<Jump, Trap> {
        let label_base = self.frames.get(fi).ok_or(Trap::Invalid)?.label_base;
        let in_frame = self.labels.len().saturating_sub(label_base);
        if depth as usize >= in_frame {
            return Ok(Jump::Return);
        }
        let li = self.labels.len() - 1 - depth as usize;
        let label = *self.labels.get(li).ok_or(Trap::Invalid)?;
        let n = label.arity as usize;
        let len = self.stack.len();
        if len < n || label.height > len - n {
            return Err(Trap::Invalid);
        }
        if label.height != len - n {
            self.stack.copy_within(len - n..len, label.height);
            self.stack.truncate(label.height + n);
        }
        self.labels.truncate(li);
        Ok(Jump::To(label.cont))
    }

    /// Call imported function `func` through the host. The arguments stay
    /// on the stack until the host returns `Continue`.
    fn host_call(
        &mut self,
        module: &Module,
        inst: &mut Instance,
        host: &mut dyn Host,
        func: u32,
    ) -> Result<HostOutcome, Trap> {
        let id = inst.host_func(func).ok_or(Trap::Invalid)?;
        let ty = inst
            .func_type_index(func)
            .and_then(|t| module.types.get(t as usize))
            .ok_or(Trap::Invalid)?;
        let start = self
            .stack
            .len()
            .checked_sub(ty.params.len())
            .ok_or(Trap::Invalid)?;
        let mut results = core::mem::take(&mut self.host_results);
        results.clear();
        results.extend(ty.results.iter().map(|&t| Value::default_for(t)));
        let outcome = {
            let mut ctx = HostCtx {
                memory: &mut inst.memory,
            };
            host.call(&mut ctx, id, &self.stack[start..], &mut results)
        };
        let flow = match outcome {
            Ok(HostOutcome::Continue) => {
                let well_typed = results.len() == ty.results.len()
                    && results.iter().zip(&ty.results).all(|(v, &t)| v.ty() == t);
                if !well_typed {
                    self.host_results = results;
                    return Err(Trap::Host);
                }
                self.stack.truncate(start);
                for &v in &results {
                    if let Err(t) = self.push(v) {
                        self.host_results = results;
                        return Err(t);
                    }
                }
                HostOutcome::Continue
            }
            Ok(other) => other,
            Err(trap) => {
                self.host_results = results;
                return Err(trap);
            }
        };
        self.host_results = results;
        Ok(flow)
    }

    // ---- the interpreter loop -------------------------------------------

    #[allow(clippy::cognitive_complexity)]
    fn run_inner(
        &mut self,
        module: &Module,
        inst: &mut Instance,
        host: &mut dyn Host,
        mut fuel: u64,
    ) -> Result<Flow, Trap> {
        let mut fi = self.frames.len().checked_sub(1).ok_or(Trap::Invalid)?;
        let mut body = Self::body_of(module, inst, self.frames[fi].func)?;
        let mut pc = self.frames[fi].pc as usize;
        let mut base = self.frames[fi].base;

        // Switch to the frame `fi` after a return.
        macro_rules! ret {
            () => {
                match self.do_return()? {
                    Some(caller) => {
                        fi = caller;
                        let frame = *self.frames.get(fi).ok_or(Trap::Invalid)?;
                        body = Self::body_of(module, inst, frame.func)?;
                        pc = frame.pc as usize;
                        base = frame.base;
                    }
                    None => return Ok(Flow::Done),
                }
            };
        }
        macro_rules! jump {
            ($j:expr) => {
                match $j {
                    Jump::To(target) => pc = target as usize,
                    Jump::Return => ret!(),
                }
            };
        }
        // `$restore` is what the instruction popped before the call (the
        // table index of `call_indirect`): it goes back on the stack when the
        // host blocks, so that the re-issued instruction finds the stack as
        // it was and calls the same function with the same arguments.
        macro_rules! call {
            ($f:expr) => {
                call!($f, None)
            };
            ($f:expr, $restore:expr) => {{
                let f: u32 = $f;
                if inst.is_imported_func(f) {
                    match self.host_call(module, inst, host, f)? {
                        HostOutcome::Continue => {}
                        HostOutcome::Block => {
                            // Rewind to the call so that resuming re-issues it.
                            let restore: Option<Value> = $restore;
                            if let Some(v) = restore {
                                self.push(v)?;
                            }
                            self.frames[fi].pc = (pc - 1) as u32;
                            return Ok(Flow::Blocked);
                        }
                        HostOutcome::Exit(code) => return Ok(Flow::Exited(code)),
                    }
                } else {
                    self.frames[fi].pc = pc as u32;
                    self.push_frame(module, inst, f)?;
                    fi += 1;
                    body = Self::body_of(module, inst, f)?;
                    pc = 0;
                    base = self.frames[fi].base;
                }
            }};
        }
        macro_rules! local {
            ($idx:expr) => {
                self.stack
                    .get_mut(base + $idx as usize)
                    .ok_or(Trap::Invalid)?
            };
        }
        macro_rules! table {
            ($idx:expr) => {
                inst.tables.get_mut($idx as usize).ok_or(Trap::Invalid)?
            };
        }

        loop {
            if fuel == 0 {
                self.frames[fi].pc = pc as u32;
                return Ok(Flow::OutOfFuel);
            }
            fuel -= 1;
            let instr = *body.instrs.get(pc).ok_or(Trap::Invalid)?;
            pc += 1;

            match instr {
                // ---- control ------------------------------------------
                Instr::Unreachable => return Err(Trap::Unreachable),
                Instr::Nop => {}
                Instr::Block { ty, end } => {
                    let (np, nr) = block_arity(module, ty)?;
                    self.push_label(end + 1, nr, np)?;
                }
                Instr::Loop { ty, .. } => {
                    let (np, _) = block_arity(module, ty)?;
                    self.push_label((pc - 1) as u32, np, np)?;
                }
                Instr::If { ty, else_, end } => {
                    let cond = self.pop_as::<i32>()?;
                    let (np, nr) = block_arity(module, ty)?;
                    if cond != 0 {
                        self.push_label(end + 1, nr, np)?;
                    } else if else_ != end {
                        self.push_label(end + 1, nr, np)?;
                        pc = else_ as usize + 1;
                    } else {
                        pc = end as usize + 1;
                    }
                }
                Instr::Else { end } => {
                    // Reached from the then arm: the block is complete.
                    self.labels.pop().ok_or(Trap::Invalid)?;
                    pc = end as usize + 1;
                }
                Instr::End => {
                    if self.labels.len() > self.frames[fi].label_base {
                        self.labels.pop();
                    } else {
                        ret!();
                    }
                }
                Instr::Br(depth) => jump!(self.branch(fi, depth)?),
                Instr::BrIf(depth) => {
                    if self.pop_as::<i32>()? != 0 {
                        jump!(self.branch(fi, depth)?);
                    }
                }
                Instr::BrTable(idx) => {
                    let i = self.pop_as::<i32>()? as u32;
                    let table = body.br_table(idx).ok_or(Trap::Invalid)?;
                    let depth = table
                        .targets
                        .get(i as usize)
                        .copied()
                        .unwrap_or(table.default);
                    jump!(self.branch(fi, depth)?);
                }
                Instr::Return => ret!(),
                Instr::Call(f) => call!(f),
                Instr::CallIndirect { ty, table } => {
                    let i = self.pop_as::<i32>()? as u32;
                    let t = inst.tables.get(table as usize).ok_or(Trap::Invalid)?;
                    let f = match t.get(i) {
                        Ok(Value::FuncRef(Some(f))) => f,
                        Ok(Value::FuncRef(None)) => return Err(Trap::UninitializedElement),
                        Ok(_) => return Err(Trap::Invalid),
                        Err(_) => return Err(Trap::UndefinedElement),
                    };
                    let actual = inst.func_type_index(f).ok_or(Trap::Invalid)?;
                    if actual != ty {
                        let expected = module.types.get(ty as usize).ok_or(Trap::Invalid)?;
                        let got = module.types.get(actual as usize).ok_or(Trap::Invalid)?;
                        if expected != got {
                            return Err(Trap::IndirectCallTypeMismatch);
                        }
                    }
                    // The popped index is restored if the host blocks.
                    call!(f, Some(Value::I32(i as i32)));
                }

                // ---- parametric ---------------------------------------
                Instr::Drop => {
                    self.pop()?;
                }
                Instr::Select | Instr::SelectT(_) => {
                    let cond = self.pop_as::<i32>()?;
                    let v2 = self.pop()?;
                    let v1 = self.pop()?;
                    self.push(if cond != 0 { v1 } else { v2 })?;
                }

                // ---- variables ----------------------------------------
                Instr::LocalGet(i) => {
                    let v = *local!(i);
                    self.push(v)?;
                }
                Instr::LocalSet(i) => {
                    let v = self.pop()?;
                    *local!(i) = v;
                }
                Instr::LocalTee(i) => {
                    let v = *self.stack.last().ok_or(Trap::Invalid)?;
                    *local!(i) = v;
                }
                Instr::GlobalGet(i) => {
                    let v = inst.global(i).ok_or(Trap::Invalid)?;
                    self.push(v)?;
                }
                Instr::GlobalSet(i) => {
                    let v = self.pop()?;
                    *inst.globals.get_mut(i as usize).ok_or(Trap::Invalid)? = v;
                }

                // ---- references ---------------------------------------
                Instr::RefNull(t) => self.push(Value::null(t))?,
                Instr::RefIsNull => {
                    let v = self.pop()?;
                    if !v.ty().is_ref() {
                        return Err(Trap::Invalid);
                    }
                    self.push(Value::I32(v.is_null_ref() as i32))?;
                }
                Instr::RefFunc(f) => {
                    if (f as usize) >= inst.num_funcs() {
                        return Err(Trap::Invalid);
                    }
                    self.push(Value::FuncRef(Some(f)))?;
                }

                // ---- tables -------------------------------------------
                Instr::TableGet(t) => {
                    let i = self.pop_as::<i32>()? as u32;
                    let v = table!(t).get(i)?;
                    self.push(v)?;
                }
                Instr::TableSet(t) => {
                    let v = self.pop()?;
                    let i = self.pop_as::<i32>()? as u32;
                    table!(t).set(i, v)?;
                }
                Instr::TableSize(t) => {
                    let n = table!(t).size();
                    self.push(Value::I32(n as i32))?;
                }
                Instr::TableGrow(t) => {
                    let n = self.pop_as::<i32>()? as u32;
                    let init = self.pop()?;
                    let r = inst.grow_table(t, n, init)?.map_or(-1, |old| old as i32);
                    self.push(Value::I32(r))?;
                }
                Instr::TableFill(t) => {
                    let n = self.pop_as::<i32>()? as u32;
                    let v = self.pop()?;
                    let i = self.pop_as::<i32>()? as u32;
                    table!(t).fill(i, v, n)?;
                }
                Instr::TableCopy { dst, src } => {
                    let n = self.pop_as::<i32>()? as u32;
                    let s = self.pop_as::<i32>()? as u32;
                    let d = self.pop_as::<i32>()? as u32;
                    if dst == src {
                        table!(dst).copy_within(d, s, n)?;
                    } else {
                        let (di, si) = (dst as usize, src as usize);
                        if di.max(si) >= inst.tables.len() {
                            return Err(Trap::Invalid);
                        }
                        let (lo, hi) = inst.tables.split_at_mut(di.max(si));
                        let (dt, st) = if di < si {
                            (&mut lo[di], &hi[0])
                        } else {
                            (&mut hi[0], &lo[si])
                        };
                        dt.init(d, st.entries(), s, n)?;
                    }
                }
                Instr::TableInit { elem, table } => {
                    let n = self.pop_as::<i32>()? as u32;
                    let s = self.pop_as::<i32>()? as u32;
                    let d = self.pop_as::<i32>()? as u32;
                    let seg = inst.elems.get(elem as usize).ok_or(Trap::Invalid)?;
                    inst.tables
                        .get_mut(table as usize)
                        .ok_or(Trap::Invalid)?
                        .init(d, seg, s, n)?;
                }
                Instr::ElemDrop(e) => {
                    *inst.elems.get_mut(e as usize).ok_or(Trap::Invalid)? = Vec::new();
                }

                // ---- memory -------------------------------------------
                Instr::I32Load(m) => {
                    let b = self.load::<4>(inst, m)?;
                    self.push(Value::I32(i32::from_le_bytes(b)))?;
                }
                Instr::I64Load(m) => {
                    let b = self.load::<8>(inst, m)?;
                    self.push(Value::I64(i64::from_le_bytes(b)))?;
                }
                Instr::F32Load(m) => {
                    let b = self.load::<4>(inst, m)?;
                    self.push(Value::F32(f32::from_le_bytes(b)))?;
                }
                Instr::F64Load(m) => {
                    let b = self.load::<8>(inst, m)?;
                    self.push(Value::F64(f64::from_le_bytes(b)))?;
                }
                Instr::I32Load8S(m) => {
                    let b = self.load::<1>(inst, m)?;
                    self.push(Value::I32(b[0] as i8 as i32))?;
                }
                Instr::I32Load8U(m) => {
                    let b = self.load::<1>(inst, m)?;
                    self.push(Value::I32(b[0] as i32))?;
                }
                Instr::I32Load16S(m) => {
                    let b = self.load::<2>(inst, m)?;
                    self.push(Value::I32(i16::from_le_bytes(b) as i32))?;
                }
                Instr::I32Load16U(m) => {
                    let b = self.load::<2>(inst, m)?;
                    self.push(Value::I32(u16::from_le_bytes(b) as i32))?;
                }
                Instr::I64Load8S(m) => {
                    let b = self.load::<1>(inst, m)?;
                    self.push(Value::I64(b[0] as i8 as i64))?;
                }
                Instr::I64Load8U(m) => {
                    let b = self.load::<1>(inst, m)?;
                    self.push(Value::I64(b[0] as i64))?;
                }
                Instr::I64Load16S(m) => {
                    let b = self.load::<2>(inst, m)?;
                    self.push(Value::I64(i16::from_le_bytes(b) as i64))?;
                }
                Instr::I64Load16U(m) => {
                    let b = self.load::<2>(inst, m)?;
                    self.push(Value::I64(u16::from_le_bytes(b) as i64))?;
                }
                Instr::I64Load32S(m) => {
                    let b = self.load::<4>(inst, m)?;
                    self.push(Value::I64(i32::from_le_bytes(b) as i64))?;
                }
                Instr::I64Load32U(m) => {
                    let b = self.load::<4>(inst, m)?;
                    self.push(Value::I64(u32::from_le_bytes(b) as i64))?;
                }
                Instr::I32Store(m) => {
                    let v = self.pop_as::<i32>()?;
                    self.store(inst, m, &v.to_le_bytes())?;
                }
                Instr::I64Store(m) => {
                    let v = self.pop_as::<i64>()?;
                    self.store(inst, m, &v.to_le_bytes())?;
                }
                Instr::F32Store(m) => {
                    let v = self.pop_as::<f32>()?;
                    self.store(inst, m, &v.to_le_bytes())?;
                }
                Instr::F64Store(m) => {
                    let v = self.pop_as::<f64>()?;
                    self.store(inst, m, &v.to_le_bytes())?;
                }
                Instr::I32Store8(m) => {
                    let v = self.pop_as::<i32>()?;
                    self.store(inst, m, &[v as u8])?;
                }
                Instr::I32Store16(m) => {
                    let v = self.pop_as::<i32>()?;
                    self.store(inst, m, &(v as u16).to_le_bytes())?;
                }
                Instr::I64Store8(m) => {
                    let v = self.pop_as::<i64>()?;
                    self.store(inst, m, &[v as u8])?;
                }
                Instr::I64Store16(m) => {
                    let v = self.pop_as::<i64>()?;
                    self.store(inst, m, &(v as u16).to_le_bytes())?;
                }
                Instr::I64Store32(m) => {
                    let v = self.pop_as::<i64>()?;
                    self.store(inst, m, &(v as u32).to_le_bytes())?;
                }
                Instr::MemorySize => {
                    let n = inst.memory.size_pages();
                    self.push(Value::I32(n as i32))?;
                }
                Instr::MemoryGrow => {
                    let n = self.pop_as::<i32>()? as u32;
                    let r = inst.memory.grow(n).map_or(-1, |old| old as i32);
                    self.push(Value::I32(r))?;
                }
                Instr::MemoryFill => {
                    let n = self.pop_as::<i32>()? as u32;
                    let v = self.pop_as::<i32>()? as u8;
                    let d = self.pop_as::<i32>()? as u32;
                    inst.memory.fill(d, v, n)?;
                }
                Instr::MemoryCopy => {
                    let n = self.pop_as::<i32>()? as u32;
                    let s = self.pop_as::<i32>()? as u32;
                    let d = self.pop_as::<i32>()? as u32;
                    inst.memory.copy_within(d, s, n)?;
                }
                Instr::MemoryInit(seg) => {
                    let n = self.pop_as::<i32>()? as u32;
                    let s = self.pop_as::<i32>()? as u32;
                    let d = self.pop_as::<i32>()? as u32;
                    let dropped = *inst.data_dropped.get(seg as usize).ok_or(Trap::Invalid)?;
                    let data: &[u8] = if dropped {
                        &[]
                    } else {
                        &module.data.get(seg as usize).ok_or(Trap::Invalid)?.bytes
                    };
                    let start = s as usize;
                    let end = start.checked_add(n as usize).ok_or(Trap::OutOfBounds)?;
                    if end > data.len() {
                        return Err(Trap::OutOfBounds);
                    }
                    // `write` checks the destination before touching memory.
                    let bytes = &data[start..end];
                    inst.memory.write(d, bytes)?;
                }
                Instr::DataDrop(seg) => {
                    *inst
                        .data_dropped
                        .get_mut(seg as usize)
                        .ok_or(Trap::Invalid)? = true;
                }

                // ---- constants ----------------------------------------
                Instr::I32Const(v) => self.push(Value::I32(v))?,
                Instr::I64Const(v) => self.push(Value::I64(v))?,
                Instr::F32Const(bits) => self.push(Value::F32(f32::from_bits(bits)))?,
                Instr::F64Const(bits) => self.push(Value::F64(f64::from_bits(bits)))?,

                // ---- i32 ----------------------------------------------
                Instr::I32Eqz => self.un(|a: i32| a == 0)?,
                Instr::I32Eq => self.bin(|a: i32, b: i32| a == b)?,
                Instr::I32Ne => self.bin(|a: i32, b: i32| a != b)?,
                Instr::I32LtS => self.bin(|a: i32, b: i32| a < b)?,
                Instr::I32LtU => self.bin(|a: i32, b: i32| (a as u32) < (b as u32))?,
                Instr::I32GtS => self.bin(|a: i32, b: i32| a > b)?,
                Instr::I32GtU => self.bin(|a: i32, b: i32| (a as u32) > (b as u32))?,
                Instr::I32LeS => self.bin(|a: i32, b: i32| a <= b)?,
                Instr::I32LeU => self.bin(|a: i32, b: i32| (a as u32) <= (b as u32))?,
                Instr::I32GeS => self.bin(|a: i32, b: i32| a >= b)?,
                Instr::I32GeU => self.bin(|a: i32, b: i32| (a as u32) >= (b as u32))?,
                Instr::I32Clz => self.un(|a: i32| a.leading_zeros() as i32)?,
                Instr::I32Ctz => self.un(|a: i32| a.trailing_zeros() as i32)?,
                Instr::I32Popcnt => self.un(|a: i32| a.count_ones() as i32)?,
                Instr::I32Add => self.bin(|a: i32, b: i32| a.wrapping_add(b))?,
                Instr::I32Sub => self.bin(|a: i32, b: i32| a.wrapping_sub(b))?,
                Instr::I32Mul => self.bin(|a: i32, b: i32| a.wrapping_mul(b))?,
                Instr::I32DivS => self.bin_try(|a: i32, b: i32| {
                    if b == 0 {
                        Err(Trap::IntegerDivideByZero)
                    } else if a == i32::MIN && b == -1 {
                        Err(Trap::IntegerOverflow)
                    } else {
                        Ok(a.wrapping_div(b))
                    }
                })?,
                Instr::I32DivU => self.bin_try(|a: i32, b: i32| {
                    (a as u32)
                        .checked_div(b as u32)
                        .map(|v| v as i32)
                        .ok_or(Trap::IntegerDivideByZero)
                })?,
                Instr::I32RemS => self.bin_try(|a: i32, b: i32| {
                    if b == 0 {
                        Err(Trap::IntegerDivideByZero)
                    } else {
                        Ok(a.wrapping_rem(b))
                    }
                })?,
                Instr::I32RemU => self.bin_try(|a: i32, b: i32| {
                    (a as u32)
                        .checked_rem(b as u32)
                        .map(|v| v as i32)
                        .ok_or(Trap::IntegerDivideByZero)
                })?,
                Instr::I32And => self.bin(|a: i32, b: i32| a & b)?,
                Instr::I32Or => self.bin(|a: i32, b: i32| a | b)?,
                Instr::I32Xor => self.bin(|a: i32, b: i32| a ^ b)?,
                Instr::I32Shl => self.bin(|a: i32, b: i32| a.wrapping_shl(b as u32))?,
                Instr::I32ShrS => self.bin(|a: i32, b: i32| a.wrapping_shr(b as u32))?,
                Instr::I32ShrU => {
                    self.bin(|a: i32, b: i32| (a as u32).wrapping_shr(b as u32) as i32)?
                }
                Instr::I32Rotl => self.bin(|a: i32, b: i32| a.rotate_left(b as u32 & 31))?,
                Instr::I32Rotr => self.bin(|a: i32, b: i32| a.rotate_right(b as u32 & 31))?,

                // ---- i64 ----------------------------------------------
                Instr::I64Eqz => self.un(|a: i64| a == 0)?,
                Instr::I64Eq => self.bin(|a: i64, b: i64| a == b)?,
                Instr::I64Ne => self.bin(|a: i64, b: i64| a != b)?,
                Instr::I64LtS => self.bin(|a: i64, b: i64| a < b)?,
                Instr::I64LtU => self.bin(|a: i64, b: i64| (a as u64) < (b as u64))?,
                Instr::I64GtS => self.bin(|a: i64, b: i64| a > b)?,
                Instr::I64GtU => self.bin(|a: i64, b: i64| (a as u64) > (b as u64))?,
                Instr::I64LeS => self.bin(|a: i64, b: i64| a <= b)?,
                Instr::I64LeU => self.bin(|a: i64, b: i64| (a as u64) <= (b as u64))?,
                Instr::I64GeS => self.bin(|a: i64, b: i64| a >= b)?,
                Instr::I64GeU => self.bin(|a: i64, b: i64| (a as u64) >= (b as u64))?,
                Instr::I64Clz => self.un(|a: i64| a.leading_zeros() as i64)?,
                Instr::I64Ctz => self.un(|a: i64| a.trailing_zeros() as i64)?,
                Instr::I64Popcnt => self.un(|a: i64| a.count_ones() as i64)?,
                Instr::I64Add => self.bin(|a: i64, b: i64| a.wrapping_add(b))?,
                Instr::I64Sub => self.bin(|a: i64, b: i64| a.wrapping_sub(b))?,
                Instr::I64Mul => self.bin(|a: i64, b: i64| a.wrapping_mul(b))?,
                Instr::I64DivS => self.bin_try(|a: i64, b: i64| {
                    if b == 0 {
                        Err(Trap::IntegerDivideByZero)
                    } else if a == i64::MIN && b == -1 {
                        Err(Trap::IntegerOverflow)
                    } else {
                        Ok(a.wrapping_div(b))
                    }
                })?,
                Instr::I64DivU => self.bin_try(|a: i64, b: i64| {
                    (a as u64)
                        .checked_div(b as u64)
                        .map(|v| v as i64)
                        .ok_or(Trap::IntegerDivideByZero)
                })?,
                Instr::I64RemS => self.bin_try(|a: i64, b: i64| {
                    if b == 0 {
                        Err(Trap::IntegerDivideByZero)
                    } else {
                        Ok(a.wrapping_rem(b))
                    }
                })?,
                Instr::I64RemU => self.bin_try(|a: i64, b: i64| {
                    (a as u64)
                        .checked_rem(b as u64)
                        .map(|v| v as i64)
                        .ok_or(Trap::IntegerDivideByZero)
                })?,
                Instr::I64And => self.bin(|a: i64, b: i64| a & b)?,
                Instr::I64Or => self.bin(|a: i64, b: i64| a | b)?,
                Instr::I64Xor => self.bin(|a: i64, b: i64| a ^ b)?,
                Instr::I64Shl => self.bin(|a: i64, b: i64| a.wrapping_shl(b as u32))?,
                Instr::I64ShrS => self.bin(|a: i64, b: i64| a.wrapping_shr(b as u32))?,
                Instr::I64ShrU => {
                    self.bin(|a: i64, b: i64| (a as u64).wrapping_shr(b as u32) as i64)?
                }
                Instr::I64Rotl => self.bin(|a: i64, b: i64| a.rotate_left(b as u32 & 63))?,
                Instr::I64Rotr => self.bin(|a: i64, b: i64| a.rotate_right(b as u32 & 63))?,

                // ---- f32 ----------------------------------------------
                Instr::F32Eq => self.bin(|a: f32, b: f32| a == b)?,
                Instr::F32Ne => self.bin(|a: f32, b: f32| a != b)?,
                Instr::F32Lt => self.bin(|a: f32, b: f32| a < b)?,
                Instr::F32Gt => self.bin(|a: f32, b: f32| a > b)?,
                Instr::F32Le => self.bin(|a: f32, b: f32| a <= b)?,
                Instr::F32Ge => self.bin(|a: f32, b: f32| a >= b)?,
                Instr::F32Abs => self.un(f32_abs)?,
                Instr::F32Neg => self.un(f32_neg)?,
                Instr::F32Ceil => self.un(|a: f32| round_f32(a, libm::ceilf))?,
                Instr::F32Floor => self.un(|a: f32| round_f32(a, libm::floorf))?,
                Instr::F32Trunc => self.un(|a: f32| round_f32(a, libm::truncf))?,
                Instr::F32Nearest => self.un(|a: f32| round_f32(a, libm::rintf))?,
                Instr::F32Sqrt => self.un(f32_sqrt)?,
                Instr::F32Add => self.bin(|a: f32, b: f32| a + b)?,
                Instr::F32Sub => self.bin(|a: f32, b: f32| a - b)?,
                Instr::F32Mul => self.bin(|a: f32, b: f32| a * b)?,
                Instr::F32Div => self.bin(|a: f32, b: f32| a / b)?,
                Instr::F32Min => self.bin(f32_min)?,
                Instr::F32Max => self.bin(f32_max)?,
                Instr::F32Copysign => self.bin(f32_copysign)?,

                // ---- f64 ----------------------------------------------
                Instr::F64Eq => self.bin(|a: f64, b: f64| a == b)?,
                Instr::F64Ne => self.bin(|a: f64, b: f64| a != b)?,
                Instr::F64Lt => self.bin(|a: f64, b: f64| a < b)?,
                Instr::F64Gt => self.bin(|a: f64, b: f64| a > b)?,
                Instr::F64Le => self.bin(|a: f64, b: f64| a <= b)?,
                Instr::F64Ge => self.bin(|a: f64, b: f64| a >= b)?,
                Instr::F64Abs => self.un(f64_abs)?,
                Instr::F64Neg => self.un(f64_neg)?,
                Instr::F64Ceil => self.un(|a: f64| round_f64(a, libm::ceil))?,
                Instr::F64Floor => self.un(|a: f64| round_f64(a, libm::floor))?,
                Instr::F64Trunc => self.un(|a: f64| round_f64(a, libm::trunc))?,
                Instr::F64Nearest => self.un(|a: f64| round_f64(a, libm::rint))?,
                Instr::F64Sqrt => self.un(f64_sqrt)?,
                Instr::F64Add => self.bin(|a: f64, b: f64| a + b)?,
                Instr::F64Sub => self.bin(|a: f64, b: f64| a - b)?,
                Instr::F64Mul => self.bin(|a: f64, b: f64| a * b)?,
                Instr::F64Div => self.bin(|a: f64, b: f64| a / b)?,
                Instr::F64Min => self.bin(f64_min)?,
                Instr::F64Max => self.bin(f64_max)?,
                Instr::F64Copysign => self.bin(f64_copysign)?,

                // ---- conversions --------------------------------------
                Instr::I32WrapI64 => self.un(|a: i64| a as i32)?,
                Instr::I32TruncF32S => self.un_try(i32_trunc_f32_s)?,
                Instr::I32TruncF32U => {
                    self.un_try(|a: f32| i32_trunc_f32_u(a).map(|v| v as i32))?
                }
                Instr::I32TruncF64S => self.un_try(i32_trunc_f64_s)?,
                Instr::I32TruncF64U => {
                    self.un_try(|a: f64| i32_trunc_f64_u(a).map(|v| v as i32))?
                }
                Instr::I64ExtendI32S => self.un(|a: i32| a as i64)?,
                Instr::I64ExtendI32U => self.un(|a: i32| a as u32 as i64)?,
                Instr::I64TruncF32S => self.un_try(i64_trunc_f32_s)?,
                Instr::I64TruncF32U => {
                    self.un_try(|a: f32| i64_trunc_f32_u(a).map(|v| v as i64))?
                }
                Instr::I64TruncF64S => self.un_try(i64_trunc_f64_s)?,
                Instr::I64TruncF64U => {
                    self.un_try(|a: f64| i64_trunc_f64_u(a).map(|v| v as i64))?
                }
                Instr::F32ConvertI32S => self.un(|a: i32| a as f32)?,
                Instr::F32ConvertI32U => self.un(|a: i32| a as u32 as f32)?,
                Instr::F32ConvertI64S => self.un(|a: i64| a as f32)?,
                Instr::F32ConvertI64U => self.un(|a: i64| a as u64 as f32)?,
                Instr::F32DemoteF64 => self.un(f32_demote)?,
                Instr::F64ConvertI32S => self.un(|a: i32| a as f64)?,
                Instr::F64ConvertI32U => self.un(|a: i32| a as u32 as f64)?,
                Instr::F64ConvertI64S => self.un(|a: i64| a as f64)?,
                Instr::F64ConvertI64U => self.un(|a: i64| a as u64 as f64)?,
                Instr::F64PromoteF32 => self.un(f64_promote)?,
                Instr::I32ReinterpretF32 => self.un(|a: f32| a.to_bits() as i32)?,
                Instr::I64ReinterpretF64 => self.un(|a: f64| a.to_bits() as i64)?,
                Instr::F32ReinterpretI32 => self.un(|a: i32| f32::from_bits(a as u32))?,
                Instr::F64ReinterpretI64 => self.un(|a: i64| f64::from_bits(a as u64))?,

                // ---- sign extension -----------------------------------
                Instr::I32Extend8S => self.un(|a: i32| a as i8 as i32)?,
                Instr::I32Extend16S => self.un(|a: i32| a as i16 as i32)?,
                Instr::I64Extend8S => self.un(|a: i64| a as i8 as i64)?,
                Instr::I64Extend16S => self.un(|a: i64| a as i16 as i64)?,
                Instr::I64Extend32S => self.un(|a: i64| a as i32 as i64)?,

                // ---- saturating truncation (Rust `as` semantics) -------
                Instr::I32TruncSatF32S => self.un(|a: f32| a as i32)?,
                Instr::I32TruncSatF32U => self.un(|a: f32| a as u32 as i32)?,
                Instr::I32TruncSatF64S => self.un(|a: f64| a as i32)?,
                Instr::I32TruncSatF64U => self.un(|a: f64| a as u32 as i32)?,
                Instr::I64TruncSatF32S => self.un(|a: f32| a as i64)?,
                Instr::I64TruncSatF32U => self.un(|a: f32| a as u64 as i64)?,
                Instr::I64TruncSatF64S => self.un(|a: f64| a as i64)?,
                Instr::I64TruncSatF64U => self.un(|a: f64| a as u64 as i64)?,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{HostFuncId, Limits};
    use crate::module::Module;
    use alloc::sync::Arc;
    use alloc::vec;

    /// A host that refuses every call.
    struct NoHost;

    impl Host for NoHost {
        fn call(
            &mut self,
            _ctx: &mut HostCtx<'_>,
            _func: HostFuncId,
            _args: &[Value],
            _results: &mut [Value],
        ) -> Result<HostOutcome, Trap> {
            Err(Trap::Host)
        }
    }

    fn module(wat: &str) -> Arc<Module> {
        let bytes = wat::parse_str(wat).unwrap_or_else(|e| panic!("bad wat: {e}\n{wat}"));
        Arc::new(Module::new(&bytes).unwrap_or_else(|e| panic!("bad module: {e}\n{wat}")))
    }

    fn instantiate_with(wat: &str, limits: &Limits) -> Result<Instance, Error> {
        let mut next = 0;
        Instance::new(
            module(wat),
            &mut |_, _, _| {
                next += 1;
                Ok(HostFuncId(next - 1))
            },
            limits,
        )
    }

    fn instantiate(wat: &str) -> Instance {
        instantiate_with(wat, &Limits::default()).unwrap_or_else(|e| panic!("{e}"))
    }

    fn call(inst: &mut Instance, name: &str, args: &[Value]) -> ExecResult {
        let f = inst
            .export_func(name)
            .unwrap_or_else(|| panic!("no export {name}"));
        let mut e = Execution::new(inst, f, args).unwrap();
        e.run(inst, &mut NoHost, u64::MAX)
    }

    fn ok(inst: &mut Instance, name: &str, args: &[Value]) -> Vec<Value> {
        match call(inst, name, args) {
            ExecResult::Done(v) => v,
            other => panic!("{name}{args:?}: expected Done, got {other:?}"),
        }
    }

    fn one(inst: &mut Instance, name: &str, args: &[Value]) -> Value {
        let v = ok(inst, name, args);
        assert_eq!(v.len(), 1, "{name}: one result expected, got {v:?}");
        v[0]
    }

    fn trap(inst: &mut Instance, name: &str, args: &[Value]) -> Trap {
        match call(inst, name, args) {
            ExecResult::Trap(t) => t,
            other => panic!("{name}{args:?}: expected a trap, got {other:?}"),
        }
    }

    fn i32v(v: i32) -> Value {
        Value::I32(v)
    }
    fn i64v(v: i64) -> Value {
        Value::I64(v)
    }
    fn f32v(v: f32) -> Value {
        Value::F32(v)
    }
    fn f64v(v: f64) -> Value {
        Value::F64(v)
    }

    fn bits32(v: Value) -> u32 {
        v.as_f32().unwrap().to_bits()
    }
    fn bits64(v: Value) -> u64 {
        v.as_f64().unwrap().to_bits()
    }

    const BINOPS: &str = r#"(module
        (func (export "div_s") (param i32 i32) (result i32) (i32.div_s (local.get 0) (local.get 1)))
        (func (export "div_u") (param i32 i32) (result i32) (i32.div_u (local.get 0) (local.get 1)))
        (func (export "rem_s") (param i32 i32) (result i32) (i32.rem_s (local.get 0) (local.get 1)))
        (func (export "rem_u") (param i32 i32) (result i32) (i32.rem_u (local.get 0) (local.get 1)))
        (func (export "add") (param i32 i32) (result i32) (i32.add (local.get 0) (local.get 1)))
        (func (export "mul") (param i32 i32) (result i32) (i32.mul (local.get 0) (local.get 1)))
        (func (export "shl") (param i32 i32) (result i32) (i32.shl (local.get 0) (local.get 1)))
        (func (export "shr_s") (param i32 i32) (result i32) (i32.shr_s (local.get 0) (local.get 1)))
        (func (export "shr_u") (param i32 i32) (result i32) (i32.shr_u (local.get 0) (local.get 1)))
        (func (export "rotl") (param i32 i32) (result i32) (i32.rotl (local.get 0) (local.get 1)))
        (func (export "rotr") (param i32 i32) (result i32) (i32.rotr (local.get 0) (local.get 1)))
        (func (export "clz") (param i32) (result i32) (i32.clz (local.get 0)))
        (func (export "ctz") (param i32) (result i32) (i32.ctz (local.get 0)))
        (func (export "popcnt") (param i32) (result i32) (i32.popcnt (local.get 0)))
        (func (export "ext8") (param i32) (result i32) (i32.extend8_s (local.get 0)))
        (func (export "ext16") (param i32) (result i32) (i32.extend16_s (local.get 0)))
        (func (export "lt_u") (param i32 i32) (result i32) (i32.lt_u (local.get 0) (local.get 1)))
        (func (export "div_s64") (param i64 i64) (result i64) (i64.div_s (local.get 0) (local.get 1)))
        (func (export "rem_s64") (param i64 i64) (result i64) (i64.rem_s (local.get 0) (local.get 1)))
        (func (export "rotl64") (param i64 i64) (result i64) (i64.rotl (local.get 0) (local.get 1)))
        (func (export "shr_u64") (param i64 i64) (result i64) (i64.shr_u (local.get 0) (local.get 1)))
        (func (export "ext32") (param i64) (result i64) (i64.extend32_s (local.get 0)))
        (func (export "wrap") (param i64) (result i32) (i32.wrap_i64 (local.get 0)))
        (func (export "extend_u") (param i32) (result i64) (i64.extend_i32_u (local.get 0)))
    )"#;

    #[test]
    fn integer_arithmetic_edges() {
        let mut inst = instantiate(BINOPS);
        let i = &mut inst;
        assert_eq!(one(i, "add", &[i32v(i32::MAX), i32v(1)]), i32v(i32::MIN));
        assert_eq!(one(i, "mul", &[i32v(0x10000), i32v(0x10000)]), i32v(0));
        assert_eq!(one(i, "div_s", &[i32v(-7), i32v(2)]), i32v(-3));
        assert_eq!(
            trap(i, "div_s", &[i32v(1), i32v(0)]),
            Trap::IntegerDivideByZero
        );
        assert_eq!(
            trap(i, "div_s", &[i32v(i32::MIN), i32v(-1)]),
            Trap::IntegerOverflow
        );
        assert_eq!(one(i, "div_u", &[i32v(-1), i32v(2)]), i32v(0x7fff_ffff));
        assert_eq!(
            trap(i, "div_u", &[i32v(1), i32v(0)]),
            Trap::IntegerDivideByZero
        );
        assert_eq!(one(i, "rem_s", &[i32v(i32::MIN), i32v(-1)]), i32v(0));
        assert_eq!(one(i, "rem_s", &[i32v(-7), i32v(2)]), i32v(-1));
        assert_eq!(
            trap(i, "rem_s", &[i32v(1), i32v(0)]),
            Trap::IntegerDivideByZero
        );
        assert_eq!(one(i, "rem_u", &[i32v(-1), i32v(2)]), i32v(1));
        assert_eq!(
            trap(i, "rem_u", &[i32v(1), i32v(0)]),
            Trap::IntegerDivideByZero
        );
        assert_eq!(one(i, "shl", &[i32v(1), i32v(33)]), i32v(2));
        assert_eq!(one(i, "shr_s", &[i32v(-8), i32v(1)]), i32v(-4));
        assert_eq!(one(i, "shr_u", &[i32v(-8), i32v(1)]), i32v(0x7fff_fffc));
        assert_eq!(
            one(i, "rotl", &[i32v(0x8000_0001u32 as i32), i32v(33)]),
            i32v(3)
        );
        assert_eq!(one(i, "rotr", &[i32v(1), i32v(1)]), i32v(i32::MIN));
        assert_eq!(one(i, "clz", &[i32v(0)]), i32v(32));
        assert_eq!(one(i, "ctz", &[i32v(0)]), i32v(32));
        assert_eq!(one(i, "popcnt", &[i32v(-1)]), i32v(32));
        assert_eq!(one(i, "ext8", &[i32v(0x80)]), i32v(-128));
        assert_eq!(one(i, "ext16", &[i32v(0x8000)]), i32v(-32768));
        assert_eq!(one(i, "lt_u", &[i32v(-1), i32v(0)]), i32v(0));
        assert_eq!(
            trap(i, "div_s64", &[i64v(i64::MIN), i64v(-1)]),
            Trap::IntegerOverflow
        );
        assert_eq!(one(i, "rem_s64", &[i64v(i64::MIN), i64v(-1)]), i64v(0));
        assert_eq!(one(i, "rotl64", &[i64v(1), i64v(65)]), i64v(2));
        assert_eq!(one(i, "shr_u64", &[i64v(-1), i64v(63)]), i64v(1));
        assert_eq!(one(i, "ext32", &[i64v(0x8000_0000)]), i64v(i32::MIN as i64));
        assert_eq!(one(i, "wrap", &[i64v(0x1_ffff_ffff)]), i32v(-1));
        assert_eq!(one(i, "extend_u", &[i32v(-1)]), i64v(0xffff_ffff));
    }

    const FLOATS: &str = r#"(module
        (func (export "min") (param f32 f32) (result f32) (f32.min (local.get 0) (local.get 1)))
        (func (export "max") (param f32 f32) (result f32) (f32.max (local.get 0) (local.get 1)))
        (func (export "nearest") (param f32) (result f32) (f32.nearest (local.get 0)))
        (func (export "nearest64") (param f64) (result f64) (f64.nearest (local.get 0)))
        (func (export "copysign") (param f32 f32) (result f32) (f32.copysign (local.get 0) (local.get 1)))
        (func (export "abs") (param f32) (result f32) (f32.abs (local.get 0)))
        (func (export "neg") (param f64) (result f64) (f64.neg (local.get 0)))
        (func (export "sqrt") (param f32) (result f32) (f32.sqrt (local.get 0)))
        (func (export "trunc_s") (param f32) (result i32) (i32.trunc_f32_s (local.get 0)))
        (func (export "trunc_u") (param f64) (result i32) (i32.trunc_f64_u (local.get 0)))
        (func (export "trunc64_s") (param f64) (result i64) (i64.trunc_f64_s (local.get 0)))
        (func (export "trunc64_u") (param f32) (result i64) (i64.trunc_f32_u (local.get 0)))
        (func (export "sat_s") (param f32) (result i32) (i32.trunc_sat_f32_s (local.get 0)))
        (func (export "sat_u") (param f64) (result i32) (i32.trunc_sat_f64_u (local.get 0)))
        (func (export "sat64_u") (param f64) (result i64) (i64.trunc_sat_f64_u (local.get 0)))
        (func (export "demote") (param f64) (result f32) (f32.demote_f64 (local.get 0)))
        (func (export "promote") (param f32) (result f64) (f64.promote_f32 (local.get 0)))
        (func (export "reinterpret") (param i32) (result f32) (f32.reinterpret_i32 (local.get 0)))
        (func (export "reinterpret64") (param f64) (result i64) (i64.reinterpret_f64 (local.get 0)))
        (func (export "convert_u") (param i64) (result f32) (f32.convert_i64_u (local.get 0)))
        (func (export "convert_s") (param i32) (result f64) (f64.convert_i32_s (local.get 0)))
        (func (export "div") (param f64 f64) (result f64) (f64.div (local.get 0) (local.get 1)))
    )"#;

    #[test]
    fn float_semantics() {
        let mut inst = instantiate(FLOATS);
        let i = &mut inst;
        // min/max: NaN propagates, -0 < +0.
        assert_eq!(bits32(one(i, "min", &[f32v(-0.0), f32v(0.0)])), 0x8000_0000);
        assert_eq!(bits32(one(i, "min", &[f32v(0.0), f32v(-0.0)])), 0x8000_0000);
        assert_eq!(bits32(one(i, "max", &[f32v(-0.0), f32v(0.0)])), 0);
        assert_eq!(one(i, "min", &[f32v(1.0), f32v(2.0)]), f32v(1.0));
        assert_eq!(
            one(i, "max", &[f32v(-1.0), f32v(f32::NEG_INFINITY)]),
            f32v(-1.0)
        );
        let snan = f32::from_bits(0x7f80_0001);
        let r = bits32(one(i, "min", &[f32v(1.0), f32v(snan)]));
        assert_eq!(
            r & 0x7fc0_0000,
            0x7fc0_0000,
            "quiet NaN expected, got {r:#x}"
        );
        assert!(one(i, "max", &[f32v(f32::NAN), f32v(1.0)])
            .as_f32()
            .unwrap()
            .is_nan());
        // nearest: ties to even, sign of zero kept.
        assert_eq!(one(i, "nearest", &[f32v(2.5)]), f32v(2.0));
        assert_eq!(one(i, "nearest", &[f32v(3.5)]), f32v(4.0));
        assert_eq!(one(i, "nearest", &[f32v(-2.5)]), f32v(-2.0));
        assert_eq!(bits32(one(i, "nearest", &[f32v(-0.4)])), 0x8000_0000);
        assert_eq!(one(i, "nearest64", &[f64v(-3.5)]), f64v(-4.0));
        assert_eq!(one(i, "nearest64", &[f64v(1e300)]), f64v(1e300));
        let r = bits32(one(i, "nearest", &[f32v(snan)]));
        assert_eq!(r, 0x7fc0_0001);
        // bit-exact sign ops, NaN payload preserved.
        assert_eq!(
            bits32(one(i, "copysign", &[f32v(snan), f32v(-1.0)])),
            0xff80_0001
        );
        assert_eq!(
            bits32(one(i, "abs", &[f32v(f32::from_bits(0xff80_0001))])),
            0x7f80_0001
        );
        assert_eq!(
            bits64(one(
                i,
                "neg",
                &[f64v(f64::from_bits(0x7ff0_0000_0000_0001))]
            )),
            0xfff0_0000_0000_0001
        );
        assert_eq!(bits32(one(i, "sqrt", &[f32v(-0.0)])), 0x8000_0000);
        assert!(one(i, "sqrt", &[f32v(-1.0)]).as_f32().unwrap().is_nan());
        assert_eq!(one(i, "sqrt", &[f32v(16.0)]), f32v(4.0));
        // trapping truncation.
        assert_eq!(one(i, "trunc_s", &[f32v(-2147483648.0)]), i32v(i32::MIN));
        assert_eq!(one(i, "trunc_s", &[f32v(2147483520.0)]), i32v(2147483520));
        assert_eq!(one(i, "trunc_s", &[f32v(-0.9)]), i32v(0));
        assert_eq!(
            trap(i, "trunc_s", &[f32v(2147483648.0)]),
            Trap::IntegerOverflow
        );
        assert_eq!(
            trap(i, "trunc_s", &[f32v(-2147483904.0)]),
            Trap::IntegerOverflow
        );
        assert_eq!(
            trap(i, "trunc_s", &[f32v(f32::NAN)]),
            Trap::InvalidConversion
        );
        assert_eq!(
            trap(i, "trunc_s", &[f32v(f32::INFINITY)]),
            Trap::IntegerOverflow
        );
        assert_eq!(one(i, "trunc_u", &[f64v(-0.9)]), i32v(0));
        assert_eq!(one(i, "trunc_u", &[f64v(4294967295.0)]), i32v(-1));
        assert_eq!(
            trap(i, "trunc_u", &[f64v(4294967296.0)]),
            Trap::IntegerOverflow
        );
        assert_eq!(trap(i, "trunc_u", &[f64v(-1.0)]), Trap::IntegerOverflow);
        assert_eq!(
            one(i, "trunc64_s", &[f64v(-9223372036854775808.0)]),
            i64v(i64::MIN)
        );
        assert_eq!(
            trap(i, "trunc64_s", &[f64v(9223372036854775808.0)]),
            Trap::IntegerOverflow
        );
        assert_eq!(
            one(i, "trunc64_u", &[f32v(18446742974197923840.0)]),
            i64v(-1099511627776)
        );
        assert_eq!(
            trap(i, "trunc64_u", &[f32v(18446744073709551616.0)]),
            Trap::IntegerOverflow
        );
        // saturating truncation.
        assert_eq!(one(i, "sat_s", &[f32v(f32::NAN)]), i32v(0));
        assert_eq!(one(i, "sat_s", &[f32v(1e10)]), i32v(i32::MAX));
        assert_eq!(one(i, "sat_s", &[f32v(-1e10)]), i32v(i32::MIN));
        assert_eq!(one(i, "sat_u", &[f64v(-5.0)]), i32v(0));
        assert_eq!(one(i, "sat_u", &[f64v(1e10)]), i32v(-1));
        assert_eq!(one(i, "sat64_u", &[f64v(f64::INFINITY)]), i64v(-1));
        // conversions.
        assert!(one(i, "demote", &[f64v(f64::NAN)])
            .as_f32()
            .unwrap()
            .is_nan());
        assert_eq!(one(i, "demote", &[f64v(1e40)]), f32v(f32::INFINITY));
        assert_eq!(one(i, "promote", &[f32v(0.5)]), f64v(0.5));
        assert_eq!(
            bits32(one(i, "reinterpret", &[i32v(0x7f80_0001)])),
            0x7f80_0001
        );
        assert_eq!(one(i, "reinterpret64", &[f64v(-0.0)]), i64v(i64::MIN));
        assert_eq!(
            one(i, "convert_u", &[i64v(-1)]),
            f32v(18446744073709551616.0)
        );
        assert_eq!(one(i, "convert_s", &[i32v(i32::MIN)]), f64v(-2147483648.0));
        assert_eq!(one(i, "div", &[f64v(1.0), f64v(0.0)]), f64v(f64::INFINITY));
        assert_eq!(
            bits64(one(i, "div", &[f64v(-1.0), f64v(0.0)])),
            f64::NEG_INFINITY.to_bits()
        );
    }

    const MEMORY: &str = r#"(module
        (memory 1 3)
        (data (i32.const 8) "\01\02\03\04\05\06\07\08")
        (data $p "\10\20\30\40")
        (func (export "load") (param i32) (result i32) (i32.load (local.get 0)))
        (func (export "load_off") (param i32) (result i32) (i32.load offset=4 (local.get 0)))
        (func (export "load_big_off") (param i32) (result i32) (i32.load offset=0xffffffff (local.get 0)))
        (func (export "load8_s") (param i32) (result i32) (i32.load8_s (local.get 0)))
        (func (export "load16_u") (param i32) (result i32) (i32.load16_u (local.get 0)))
        (func (export "load64") (param i32) (result i64) (i64.load (local.get 0)))
        (func (export "load32_s") (param i32) (result i64) (i64.load32_s (local.get 0)))
        (func (export "loadf64") (param i32) (result f64) (f64.load (local.get 0)))
        (func (export "store") (param i32 i32) (i32.store (local.get 0) (local.get 1)))
        (func (export "store8") (param i32 i32) (i32.store8 (local.get 0) (local.get 1)))
        (func (export "store64_32") (param i32 i64) (i64.store32 (local.get 0) (local.get 1)))
        (func (export "storef32") (param i32 f32) (f32.store (local.get 0) (local.get 1)))
        (func (export "size") (result i32) (memory.size))
        (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0)))
        (func (export "fill") (param i32 i32 i32) (memory.fill (local.get 0) (local.get 1) (local.get 2)))
        (func (export "copy") (param i32 i32 i32) (memory.copy (local.get 0) (local.get 1) (local.get 2)))
        (func (export "init") (param i32 i32 i32) (memory.init $p (local.get 0) (local.get 1) (local.get 2)))
        (func (export "drop") (data.drop $p))
    )"#;

    #[test]
    fn memory_access_and_bounds() {
        let mut inst = instantiate(MEMORY);
        let i = &mut inst;
        assert_eq!(one(i, "load", &[i32v(8)]), i32v(0x0403_0201));
        assert_eq!(one(i, "load_off", &[i32v(8)]), i32v(0x0807_0605));
        assert_eq!(one(i, "load8_s", &[i32v(0)]), i32v(0));
        assert_eq!(one(i, "load16_u", &[i32v(9)]), i32v(0x0302));
        assert_eq!(one(i, "load64", &[i32v(8)]), i64v(0x0807_0605_0403_0201));
        assert_eq!(one(i, "load32_s", &[i32v(12)]), i64v(0x0807_0605));
        assert_eq!(one(i, "load", &[i32v(65532)]), i32v(0));
        assert_eq!(trap(i, "load", &[i32v(65533)]), Trap::OutOfBounds);
        assert_eq!(trap(i, "load", &[i32v(-1)]), Trap::OutOfBounds);
        assert_eq!(trap(i, "load_off", &[i32v(-4)]), Trap::OutOfBounds);
        // Effective address overflow must not wrap around.
        assert_eq!(trap(i, "load_big_off", &[i32v(1)]), Trap::OutOfBounds);
        assert_eq!(trap(i, "load_big_off", &[i32v(0)]), Trap::OutOfBounds);
        ok(i, "store", &[i32v(0), i32v(-1)]);
        ok(i, "store8", &[i32v(1), i32v(0x1234)]);
        assert_eq!(one(i, "load", &[i32v(0)]), i32v(0xffff_34ffu32 as i32));
        ok(i, "store64_32", &[i32v(0), i64v(0x1_2345_6789)]);
        assert_eq!(one(i, "load", &[i32v(0)]), i32v(0x2345_6789));
        ok(i, "storef32", &[i32v(0), f32v(1.5)]);
        assert_eq!(one(i, "load", &[i32v(0)]), i32v(0x3fc0_0000));
        assert_eq!(trap(i, "store", &[i32v(65533), i32v(0)]), Trap::OutOfBounds);
        ok(i, "store", &[i32v(65532), i32v(0x0a0b_0c0d)]);
        assert_eq!(
            one(i, "loadf64", &[i32v(8)]),
            f64v(f64::from_bits(0x0807_0605_0403_0201))
        );
        // grow: type max 3 and instance limit.
        assert_eq!(one(i, "size", &[]), i32v(1));
        assert_eq!(one(i, "grow", &[i32v(1)]), i32v(1));
        assert_eq!(one(i, "grow", &[i32v(2)]), i32v(-1));
        assert_eq!(one(i, "grow", &[i32v(-1)]), i32v(-1));
        assert_eq!(one(i, "grow", &[i32v(1)]), i32v(2));
        assert_eq!(one(i, "size", &[]), i32v(3));
        assert_eq!(one(i, "load", &[i32v(3 * 65536 - 4)]), i32v(0));
        // bulk ops: bounds checked before any write.
        assert_eq!(
            trap(i, "fill", &[i32v(3 * 65536 - 2), i32v(0xaa), i32v(3)]),
            Trap::OutOfBounds
        );
        assert_eq!(one(i, "load", &[i32v(3 * 65536 - 4)]), i32v(0));
        ok(i, "fill", &[i32v(100), i32v(0xaa), i32v(4)]);
        assert_eq!(one(i, "load", &[i32v(100)]), i32v(0xaaaa_aaaau32 as i32));
        ok(i, "fill", &[i32v(3 * 65536), i32v(1), i32v(0)]);
        assert_eq!(
            trap(i, "fill", &[i32v(3 * 65536 + 1), i32v(1), i32v(0)]),
            Trap::OutOfBounds
        );
        assert_eq!(
            trap(i, "copy", &[i32v(0), i32v(3 * 65536 - 2), i32v(4)]),
            Trap::OutOfBounds
        );
        assert_eq!(
            trap(i, "copy", &[i32v(3 * 65536 - 2), i32v(0), i32v(4)]),
            Trap::OutOfBounds
        );
        ok(i, "copy", &[i32v(200), i32v(8), i32v(8)]);
        assert_eq!(one(i, "load64", &[i32v(200)]), i64v(0x0807_0605_0403_0201));
        ok(i, "copy", &[i32v(201), i32v(200), i32v(7)]);
        assert_eq!(one(i, "load64", &[i32v(200)]), i64v(0x0706_0504_0302_0101));
        assert_eq!(
            trap(i, "init", &[i32v(0), i32v(1), i32v(4)]),
            Trap::OutOfBounds
        );
        assert_eq!(
            trap(i, "init", &[i32v(3 * 65536 - 2), i32v(0), i32v(4)]),
            Trap::OutOfBounds
        );
        assert_eq!(one(i, "load", &[i32v(0)]), i32v(0x3fc0_0000));
        ok(i, "init", &[i32v(0), i32v(1), i32v(3)]);
        assert_eq!(one(i, "load", &[i32v(0)]), i32v(0x3f40_3020));
        ok(i, "drop", &[]);
        ok(i, "init", &[i32v(0), i32v(0), i32v(0)]);
        assert_eq!(
            trap(i, "init", &[i32v(0), i32v(0), i32v(1)]),
            Trap::OutOfBounds
        );
        assert_eq!(
            trap(i, "init", &[i32v(0), i32v(1), i32v(0)]),
            Trap::OutOfBounds
        );
        // The host sees the same memory.
        let mut buf = [0u8; 4];
        inst.memory().read(0, &mut buf).unwrap();
        assert_eq!(buf, [0x20, 0x30, 0x40, 0x3f]);
    }

    const CONTROL: &str = r#"(module
        (type $pair (func (param i32 i32) (result i32 i32)))
        (func $fac (export "fac") (param i64) (result i64)
            (if (result i64) (i64.eqz (local.get 0))
                (then (i64.const 1))
                (else (i64.mul (local.get 0) (call $fac (i64.sub (local.get 0) (i64.const 1)))))))
        (func (export "fac-iter") (param i64) (result i64) (local i64)
            (local.set 1 (i64.const 1))
            (block (loop
                (br_if 1 (i64.eqz (local.get 0)))
                (local.set 1 (i64.mul (local.get 1) (local.get 0)))
                (local.set 0 (i64.sub (local.get 0) (i64.const 1)))
                (br 0)))
            (local.get 1))
        (func (export "switch") (param i32) (result i32)
            (block (block (block (block
                (br_table 0 1 2 3 (local.get 0)))
                (return (i32.const 100)))
                (return (i32.const 101)))
                (return (i32.const 102)))
            (i32.const 103))
        (func (export "swap") (param i32 i32) (result i32 i32)
            (local.get 1) (local.get 0)
            (block (type $pair)))
        (func (export "loop-params") (param i32) (result i32)
            (i32.const 0) (local.get 0)
            (loop (param i32 i32) (result i32)
                (i32.add)
                (local.set 0 (i32.sub (local.get 0) (i32.const 1)))
                (local.get 0)
                (br_if 0 (i32.gt_s (local.get 0) (i32.const 0)))
                (drop)))
        (func (export "nested-br") (result i32)
            (block (result i32)
                (block (result i32)
                    (i32.const 1) (i32.const 2) (i32.const 3)
                    (br 1))
                (drop)
                (i32.const 9)))
        (func (export "if-no-else") (param i32) (result i32)
            (if (local.get 0) (then (return (i32.const 7))))
            (i32.const 8))
        (func (export "select") (param i32) (result i64)
            (select (i64.const 1) (i64.const 2) (local.get 0)))
        (func (export "select-t") (param i32) (result funcref)
            (select (result funcref) (ref.func $fac) (ref.null func) (local.get 0)))
        (func (export "unreachable") (unreachable))
        (func (export "br-in-loop-return") (result i32)
            (loop (result i32) (i32.const 5) (br 1)))
    )"#;

    #[test]
    fn control_flow() {
        let mut inst = instantiate(CONTROL);
        let i = &mut inst;
        assert_eq!(one(i, "fac", &[i64v(20)]), i64v(2432902008176640000));
        assert_eq!(one(i, "fac-iter", &[i64v(20)]), i64v(2432902008176640000));
        assert_eq!(one(i, "fac-iter", &[i64v(0)]), i64v(1));
        assert_eq!(one(i, "switch", &[i32v(0)]), i32v(100));
        assert_eq!(one(i, "switch", &[i32v(1)]), i32v(101));
        assert_eq!(one(i, "switch", &[i32v(2)]), i32v(102));
        assert_eq!(one(i, "switch", &[i32v(3)]), i32v(103));
        assert_eq!(one(i, "switch", &[i32v(-1)]), i32v(103));
        assert_eq!(ok(i, "swap", &[i32v(1), i32v(2)]), vec![i32v(2), i32v(1)]);
        assert_eq!(one(i, "loop-params", &[i32v(4)]), i32v(10));
        assert_eq!(one(i, "nested-br", &[]), i32v(3));
        assert_eq!(one(i, "if-no-else", &[i32v(1)]), i32v(7));
        assert_eq!(one(i, "if-no-else", &[i32v(0)]), i32v(8));
        assert_eq!(one(i, "select", &[i32v(1)]), i64v(1));
        assert_eq!(one(i, "select", &[i32v(0)]), i64v(2));
        assert_eq!(one(i, "select-t", &[i32v(0)]), Value::FuncRef(None));
        assert_eq!(one(i, "select-t", &[i32v(5)]), Value::FuncRef(Some(0)));
        assert_eq!(trap(i, "unreachable", &[]), Trap::Unreachable);
        assert_eq!(one(i, "br-in-loop-return", &[]), i32v(5));
    }

    const TABLES: &str = r#"(module
        (type $i (func (result i32)))
        (type $ii (func (param i32) (result i32)))
        (table $t 4 funcref)
        (table $u 2 externref)
        (elem (i32.const 1) func $one $two)
        (elem $p func $two $one $one)
        (elem $e funcref (ref.func $one) (ref.null func))
        (elem declare func $inc)
        (func $one (result i32) (i32.const 1))
        (func $two (result i32) (i32.const 2))
        (func $inc (param i32) (result i32) (i32.add (local.get 0) (i32.const 1)))
        (func (export "call") (param i32) (result i32) (call_indirect (type $i) (local.get 0)))
        (func (export "call_ii") (param i32) (result i32) (call_indirect (type $ii) (i32.const 7) (local.get 0)))
        (func (export "set") (param i32 funcref) (table.set $t (local.get 0) (local.get 1)))
        (func (export "get") (param i32) (result funcref) (table.get $t (local.get 0)))
        (func (export "size") (result i32) (table.size $t))
        (func (export "grow") (param i32) (result i32) (table.grow $t (ref.func $inc) (local.get 0)))
        (func (export "fill") (param i32 i32) (table.fill $t (local.get 0) (ref.null func) (local.get 1)))
        (func (export "copy") (param i32 i32 i32) (table.copy $t $t (local.get 0) (local.get 1) (local.get 2)))
        (func (export "init") (param i32 i32 i32) (table.init $t $p (local.get 0) (local.get 1) (local.get 2)))
        (func (export "init_e") (param i32 i32 i32) (table.init $t $e (local.get 0) (local.get 1) (local.get 2)))
        (func (export "drop") (elem.drop $p))
        (func (export "is_null") (param i32) (result i32) (ref.is_null (table.get $t (local.get 0))))
        (func (export "ref_inc") (result funcref) (ref.func $inc))
        (func (export "uget") (param i32) (result externref) (table.get $u (local.get 0)))
        (func (export "uset") (param i32 externref) (table.set $u (local.get 0) (local.get 1)))
    )"#;

    #[test]
    fn tables_and_indirect_calls() {
        let mut inst = instantiate(TABLES);
        let i = &mut inst;
        assert_eq!(one(i, "call", &[i32v(1)]), i32v(1));
        assert_eq!(one(i, "call", &[i32v(2)]), i32v(2));
        assert_eq!(trap(i, "call", &[i32v(0)]), Trap::UninitializedElement);
        assert_eq!(trap(i, "call", &[i32v(4)]), Trap::UndefinedElement);
        assert_eq!(trap(i, "call", &[i32v(-1)]), Trap::UndefinedElement);
        assert_eq!(
            trap(i, "call_ii", &[i32v(1)]),
            Trap::IndirectCallTypeMismatch
        );
        ok(i, "set", &[i32v(0), Value::FuncRef(Some(2))]);
        assert_eq!(one(i, "call_ii", &[i32v(0)]), i32v(8));
        assert_eq!(one(i, "get", &[i32v(0)]), Value::FuncRef(Some(2)));
        assert_eq!(one(i, "get", &[i32v(3)]), Value::FuncRef(None));
        assert_eq!(trap(i, "get", &[i32v(4)]), Trap::TableOutOfBounds);
        assert_eq!(
            trap(i, "set", &[i32v(4), Value::FuncRef(None)]),
            Trap::TableOutOfBounds
        );
        assert_eq!(one(i, "is_null", &[i32v(3)]), i32v(1));
        assert_eq!(one(i, "is_null", &[i32v(1)]), i32v(0));
        assert_eq!(one(i, "ref_inc", &[]), Value::FuncRef(Some(2)));
        assert_eq!(one(i, "size", &[]), i32v(4));
        assert_eq!(one(i, "grow", &[i32v(2)]), i32v(4));
        assert_eq!(one(i, "size", &[]), i32v(6));
        assert_eq!(one(i, "call_ii", &[i32v(5)]), i32v(8));
        assert_eq!(one(i, "grow", &[i32v(-1)]), i32v(-1));
        assert_eq!(trap(i, "fill", &[i32v(5), i32v(2)]), Trap::TableOutOfBounds);
        assert_eq!(one(i, "get", &[i32v(5)]), Value::FuncRef(Some(2)));
        ok(i, "fill", &[i32v(4), i32v(2)]);
        assert_eq!(one(i, "get", &[i32v(5)]), Value::FuncRef(None));
        assert_eq!(
            trap(i, "copy", &[i32v(5), i32v(0), i32v(2)]),
            Trap::TableOutOfBounds
        );
        ok(i, "copy", &[i32v(3), i32v(0), i32v(3)]);
        assert_eq!(one(i, "get", &[i32v(5)]), Value::FuncRef(Some(1)));
        assert_eq!(
            trap(i, "init", &[i32v(0), i32v(2), i32v(2)]),
            Trap::TableOutOfBounds
        );
        ok(i, "init", &[i32v(0), i32v(0), i32v(3)]);
        assert_eq!(one(i, "call", &[i32v(0)]), i32v(2));
        assert_eq!(one(i, "call", &[i32v(2)]), i32v(1));
        ok(i, "init_e", &[i32v(0), i32v(0), i32v(2)]);
        assert_eq!(one(i, "get", &[i32v(1)]), Value::FuncRef(None));
        ok(i, "drop", &[]);
        ok(i, "init", &[i32v(0), i32v(0), i32v(0)]);
        assert_eq!(
            trap(i, "init", &[i32v(0), i32v(0), i32v(1)]),
            Trap::TableOutOfBounds
        );
        assert_eq!(one(i, "uget", &[i32v(1)]), Value::ExternRef(None));
        ok(i, "uset", &[i32v(1), Value::ExternRef(Some(42))]);
        assert_eq!(one(i, "uget", &[i32v(1)]), Value::ExternRef(Some(42)));
        assert_eq!(inst.table(0).unwrap().size(), 6);
    }

    #[test]
    fn globals() {
        let mut inst = instantiate(
            r#"(module
                (global $a (mut i32) (i32.const 5))
                (global $b i64 (i64.const -1))
                (global $c (mut f32) (f32.const 1.5))
                (global $d funcref (ref.func $get))
                (func $get (export "get") (result i32 i64 f32) (global.get $a) (global.get $b) (global.get $c))
                (func (export "set") (param i32 f32) (global.set $a (local.get 0)) (global.set $c (local.get 1)))
                (func (export "d") (result funcref) (global.get $d))
            )"#,
        );
        let i = &mut inst;
        assert_eq!(ok(i, "get", &[]), vec![i32v(5), i64v(-1), f32v(1.5)]);
        ok(i, "set", &[i32v(-9), f32v(2.0)]);
        assert_eq!(ok(i, "get", &[]), vec![i32v(-9), i64v(-1), f32v(2.0)]);
        assert_eq!(one(i, "d", &[]), Value::FuncRef(Some(0)));
        assert_eq!(inst.global(0), Some(i32v(-9)));
        assert_eq!(inst.global(4), None);
        inst.set_global(0, i32v(1)).unwrap();
        assert_eq!(inst.set_global(0, i64v(1)), Err(Error::ArgumentMismatch));
        assert_eq!(inst.global(0), Some(i32v(1)));
    }

    const HOSTED: &str = r#"(module
        (import "env" "add" (func $add (param i32 i32) (result i32)))
        (import "env" "exit" (func $exit (param i32)))
        (import "env" "none" (func $none))
        (memory 1)
        (func (export "run") (param i32) (result i32)
            (i32.const 100)
            (call $add (local.get 0) (i32.const 1))
            (i32.add))
        (func (export "quit") (param i32) (result i32)
            (call $exit (local.get 0))
            (i32.const 9))
        (func (export "none") (call $none))
    )"#;

    /// A host whose `add` blocks `blocks` times before answering, records
    /// every call, and writes into memory on each call.
    struct CountingHost {
        blocks: u32,
        calls: Vec<(u32, Vec<Value>)>,
        exit_code: i32,
    }

    impl Host for CountingHost {
        fn call(
            &mut self,
            ctx: &mut HostCtx<'_>,
            func: HostFuncId,
            args: &[Value],
            results: &mut [Value],
        ) -> Result<HostOutcome, Trap> {
            self.calls.push((func.0, args.to_vec()));
            ctx.memory
                .write(0, &(self.calls.len() as u32).to_le_bytes())?;
            match func.0 {
                0 => {
                    if self.blocks > 0 {
                        self.blocks -= 1;
                        return Ok(HostOutcome::Block);
                    }
                    let a = args[0].as_i32().ok_or(Trap::Host)?;
                    let b = args[1].as_i32().ok_or(Trap::Host)?;
                    results[0] = Value::I32(a + b);
                    Ok(HostOutcome::Continue)
                }
                1 => {
                    self.exit_code = args[0].as_i32().ok_or(Trap::Host)?;
                    Ok(HostOutcome::Exit(self.exit_code))
                }
                _ => Err(Trap::Host),
            }
        }
    }

    #[test]
    fn host_block_continue_exit() {
        let mut inst = instantiate(HOSTED);
        let mut host = CountingHost {
            blocks: 2,
            calls: Vec::new(),
            exit_code: 0,
        };
        let run = inst.export_func("run").unwrap();
        let mut e = Execution::new(&inst, run, &[i32v(41)]).unwrap();
        // Blocked twice: the arguments stay on the stack and the call is
        // re-issued with the same arguments each time.
        assert_eq!(e.run(&mut inst, &mut host, 1000), ExecResult::Blocked);
        let stack_after_block = e.stack_len();
        assert_eq!(e.run(&mut inst, &mut host, 1000), ExecResult::Blocked);
        assert_eq!(e.stack_len(), stack_after_block);
        assert!(!e.is_finished());
        assert_eq!(
            e.run(&mut inst, &mut host, 1000),
            ExecResult::Done(vec![i32v(142)])
        );
        assert!(e.is_finished());
        assert_eq!(host.calls.len(), 3);
        for (f, args) in &host.calls {
            assert_eq!(*f, 0);
            assert_eq!(args, &[i32v(41), i32v(1)]);
        }
        let mut buf = [0u8; 4];
        inst.memory().read(0, &mut buf).unwrap();
        assert_eq!(u32::from_le_bytes(buf), 3);
        // A finished execution keeps returning its result.
        assert_eq!(
            e.run(&mut inst, &mut host, 1000),
            ExecResult::Done(vec![i32v(142)])
        );

        // Exit.
        let quit = inst.export_func("quit").unwrap();
        let mut e = Execution::new(&inst, quit, &[i32v(3)]).unwrap();
        assert_eq!(e.run(&mut inst, &mut host, 1000), ExecResult::Exited(3));
        assert_eq!(host.exit_code, 3);
        assert_eq!(e.run(&mut inst, &mut host, 1000), ExecResult::Exited(3));

        // Host trap.
        let none = inst.export_func("none").unwrap();
        let mut e = Execution::new(&inst, none, &[]).unwrap();
        assert_eq!(
            e.run(&mut inst, &mut host, 1000),
            ExecResult::Trap(Trap::Host)
        );

        // Host result of the wrong type is a host trap too.
        struct BadHost;
        impl Host for BadHost {
            fn call(
                &mut self,
                _: &mut HostCtx<'_>,
                _: HostFuncId,
                _: &[Value],
                results: &mut [Value],
            ) -> Result<HostOutcome, Trap> {
                results[0] = Value::I64(0);
                Ok(HostOutcome::Continue)
            }
        }
        let mut e = Execution::new(&inst, run, &[i32v(1)]).unwrap();
        assert_eq!(
            e.run(&mut inst, &mut BadHost, 1000),
            ExecResult::Trap(Trap::Host)
        );

        // An import can be the entry function: the execution is one host
        // call, with the same blocking and exit behaviour.
        host.blocks = 1;
        let mut e = Execution::new(&inst, 0, &[i32v(1), i32v(2)]).unwrap();
        assert_eq!(e.run(&mut inst, &mut host, 0), ExecResult::OutOfFuel);
        assert_eq!(e.run(&mut inst, &mut host, 1), ExecResult::Blocked);
        assert_eq!(e.stack_len(), 2);
        assert_eq!(
            e.run(&mut inst, &mut host, 1),
            ExecResult::Done(vec![i32v(3)])
        );
        let mut e = Execution::new(&inst, 1, &[i32v(5)]).unwrap();
        assert_eq!(e.run(&mut inst, &mut host, 1), ExecResult::Exited(5));
        let mut e = Execution::new(&inst, 2, &[]).unwrap();
        assert_eq!(e.run(&mut inst, &mut host, 1), ExecResult::Trap(Trap::Host));
        assert_eq!(
            Execution::new(&inst, 0, &[i32v(1)]).err(),
            Some(Error::ArgumentMismatch)
        );
    }

    /// A host that blocks the first call to each function and answers the
    /// second, recording every call it sees.
    struct BlockOnceHost {
        seen: Vec<u32>,
        calls: Vec<(u32, Vec<Value>)>,
    }

    impl Host for BlockOnceHost {
        fn call(
            &mut self,
            _ctx: &mut HostCtx<'_>,
            func: HostFuncId,
            args: &[Value],
            results: &mut [Value],
        ) -> Result<HostOutcome, Trap> {
            self.calls.push((func.0, args.to_vec()));
            if !self.seen.contains(&func.0) {
                self.seen.push(func.0);
                return Ok(HostOutcome::Block);
            }
            match func.0 {
                0 => {
                    let a = args[0].as_i32().ok_or(Trap::Host)?;
                    let b = args[1].as_i32().ok_or(Trap::Host)?;
                    results[0] = Value::I32(a + b);
                    Ok(HostOutcome::Continue)
                }
                1 => Ok(HostOutcome::Continue),
                _ => Err(Trap::Host),
            }
        }
    }

    /// `call_indirect` pops the table index before the host call; when the
    /// host blocks, the index must go back on the stack so that the
    /// re-issued instruction calls the same function with the same
    /// arguments (KW-1).
    #[test]
    fn call_indirect_blocking_import_restores_table_index() {
        let mut inst = instantiate(
            r#"(module
                (type $binop (func (param i32 i32) (result i32)))
                (type $nullary (func))
                (import "env" "add" (func $add (type $binop)))
                (import "env" "none" (func $none (type $nullary)))
                (table 2 funcref)
                (elem (i32.const 0) $add $none)
                (func (export "run") (param i32) (result i32)
                    (call_indirect (type $binop) (local.get 0) (i32.const 1) (i32.const 0)))
                (func (export "nullary") (result i32)
                    (i32.const 7)
                    (call_indirect (type $nullary) (i32.const 1)))
            )"#,
        );
        let mut host = BlockOnceHost {
            seen: Vec::new(),
            calls: Vec::new(),
        };

        // Two arguments: the slot index (0) sits above them when the host
        // blocks and must still be there on resume.
        let run = inst.export_func("run").unwrap();
        let mut e = Execution::new(&inst, run, &[i32v(41)]).unwrap();
        assert_eq!(e.run(&mut inst, &mut host, 1000), ExecResult::Blocked);
        // Local 0, the two arguments and the table index.
        assert_eq!(e.stack_len(), 4);
        assert_eq!(
            e.run(&mut inst, &mut host, 1000),
            ExecResult::Done(vec![i32v(42)])
        );
        assert_eq!(
            host.calls,
            vec![(0, vec![i32v(41), i32v(1)]), (0, vec![i32v(41), i32v(1)])]
        );

        // Zero arguments: the value below the index (7) must not be taken
        // for the index on resume.
        host.calls.clear();
        let nullary = inst.export_func("nullary").unwrap();
        let mut e = Execution::new(&inst, nullary, &[]).unwrap();
        assert_eq!(e.run(&mut inst, &mut host, 1000), ExecResult::Blocked);
        assert_eq!(e.stack_len(), 2);
        assert_eq!(
            e.run(&mut inst, &mut host, 1000),
            ExecResult::Done(vec![i32v(7)])
        );
        assert_eq!(host.calls, vec![(1, vec![]), (1, vec![])]);
    }

    /// Tables are bounded by the instance-wide element budget
    /// (`Limits::max_table_elements`), not only per table (KW-2).
    #[test]
    fn table_element_budget() {
        let budget = |n| Limits {
            max_table_elements: n,
            ..Limits::default()
        };
        // Declared minimums are summed over all tables.
        let two = "(module (table 100 funcref) (table 100 externref))";
        assert!(matches!(
            instantiate_with(two, &budget(150)),
            Err(Error::Limit { .. })
        ));
        assert!(instantiate_with(two, &budget(200)).is_ok());
        assert!(matches!(
            instantiate_with("(module (table 151 funcref))", &budget(150)),
            Err(Error::Limit { .. })
        ));
        // An absurd minimum (u32::MAX elements would be 64 GiB) is rejected
        // under the default limits without allocating anything.
        assert!(matches!(
            instantiate_with("(module (table 0xffff_ffff funcref))", &Limits::default()),
            Err(Error::Limit { .. })
        ));
        assert!(matches!(
            instantiate_with(
                "(module (table 40000 funcref) (table 30000 funcref))",
                &Limits::default()
            ),
            Err(Error::Limit { .. })
        ));

        // Growth returns -1 (not a trap) when the budget would be exceeded,
        // even though each table's own bound would allow it.
        let wat = r#"(module
            (table $a 10 funcref)
            (table $b 10 funcref)
            (func (export "grow_a") (param i32) (result i32)
                (table.grow $a (ref.null func) (local.get 0)))
            (func (export "grow_b") (param i32) (result i32)
                (table.grow $b (ref.null func) (local.get 0)))
            (func (export "size_b") (result i32) (table.size $b))
        )"#;
        let mut inst = instantiate_with(wat, &budget(30)).unwrap();
        assert_eq!(inst.table_elements(), 20);
        assert_eq!(one(&mut inst, "grow_b", &[i32v(10)]), i32v(10));
        assert_eq!(inst.table_elements(), 30);
        assert_eq!(one(&mut inst, "grow_b", &[i32v(1)]), i32v(-1));
        assert_eq!(one(&mut inst, "grow_a", &[i32v(1)]), i32v(-1));
        assert_eq!(one(&mut inst, "grow_a", &[i32v(-1)]), i32v(-1));
        // A zero-length grow always succeeds, as in the spec.
        assert_eq!(one(&mut inst, "grow_a", &[i32v(0)]), i32v(10));
        assert_eq!(one(&mut inst, "size_b", &[]), i32v(20));
        assert_eq!(inst.table_elements(), 30);

        // A normal module under the default limits is unaffected.
        let mut inst = instantiate_with(wat, &Limits::default()).unwrap();
        assert_eq!(one(&mut inst, "grow_a", &[i32v(1000)]), i32v(10));
        assert_eq!(inst.table_elements(), 1020);
    }

    #[test]
    fn fuel_resumption_matches_single_run() {
        let mut inst = instantiate(CONTROL);
        let fac = inst.export_func("fac").unwrap();
        let mut whole = Execution::new(&inst, fac, &[i64v(10)]).unwrap();
        let expected = whole.run(&mut inst, &mut NoHost, u64::MAX);
        assert_eq!(expected, ExecResult::Done(vec![i64v(3628800)]));

        // Count instructions with fuel 1 per run.
        let mut e = Execution::new(&inst, fac, &[i64v(10)]).unwrap();
        let mut runs = 0u64;
        let result = loop {
            runs += 1;
            match e.run(&mut inst, &mut NoHost, 1) {
                ExecResult::OutOfFuel => continue,
                other => break other,
            }
        };
        assert_eq!(result, expected);
        assert!(
            runs > 50,
            "fac(10) takes more than 50 instructions, ran {runs}"
        );

        // Exactly that many instructions as one budget finishes it; one less
        // does not.
        let mut e = Execution::new(&inst, fac, &[i64v(10)]).unwrap();
        assert_eq!(
            e.run(&mut inst, &mut NoHost, runs - 1),
            ExecResult::OutOfFuel
        );
        assert_eq!(e.run(&mut inst, &mut NoHost, 1), expected);
        let mut e = Execution::new(&inst, fac, &[i64v(10)]).unwrap();
        assert_eq!(e.run(&mut inst, &mut NoHost, runs), expected);

        // Fuel 0 never executes anything.
        let mut e = Execution::new(&inst, fac, &[i64v(10)]).unwrap();
        assert_eq!(e.run(&mut inst, &mut NoHost, 0), ExecResult::OutOfFuel);

        // Odd chunk sizes through loops and multi-value blocks.
        for &chunk in &[1u64, 2, 3, 7, 13] {
            for name in ["fac-iter", "loop-params", "switch", "swap"] {
                let f = inst.export_func(name).unwrap();
                let args: Vec<Value> = match name {
                    "fac-iter" => vec![i64v(12)],
                    "swap" => vec![i32v(1), i32v(2)],
                    _ => vec![i32v(3)],
                };
                let mut whole = Execution::new(&inst, f, &args).unwrap();
                let expected = whole.run(&mut inst, &mut NoHost, u64::MAX);
                let mut e = Execution::new(&inst, f, &args).unwrap();
                let got = loop {
                    match e.run(&mut inst, &mut NoHost, chunk) {
                        ExecResult::OutOfFuel => continue,
                        other => break other,
                    }
                };
                assert_eq!(got, expected, "{name} with chunk {chunk}");
            }
        }

        // Fuel and host blocking interleaved.
        let mut inst = instantiate(HOSTED);
        let mut host = CountingHost {
            blocks: 3,
            calls: Vec::new(),
            exit_code: 0,
        };
        let run = inst.export_func("run").unwrap();
        let mut e = Execution::new(&inst, run, &[i32v(1)]).unwrap();
        let mut blocked = 0;
        let got = loop {
            match e.run(&mut inst, &mut host, 1) {
                ExecResult::OutOfFuel => continue,
                ExecResult::Blocked => blocked += 1,
                other => break other,
            }
        };
        assert_eq!(got, ExecResult::Done(vec![i32v(102)]));
        assert_eq!(blocked, 3);
        assert_eq!(host.calls.len(), 4);
    }

    #[test]
    fn call_depth_and_stack_limits() {
        let wat = r#"(module
            (func (export "runaway") (call 0))
            (func (export "deep") (param i32) (result i32)
                (if (result i32) (i32.eqz (local.get 0))
                    (then (i32.const 0))
                    (else (i32.add (i32.const 1) (call 1 (i32.sub (local.get 0) (i32.const 1)))))))
            (func (export "locals") (param i32)
                (local i64 i64 i64 i64 i64 i64 i64 i64 i64 i64 i64 i64 i64 i64 i64)
                (if (local.get 0) (then (call 2 (i32.sub (local.get 0) (i32.const 1))))))
        )"#;
        let limits = Limits {
            max_memory_pages: 1,
            max_stack_values: 1000,
            max_call_depth: 100,
            max_table_elements: 100,
        };
        let mut inst = instantiate_with(wat, &limits).unwrap();
        assert_eq!(trap(&mut inst, "runaway", &[]), Trap::CallDepth);
        assert_eq!(one(&mut inst, "deep", &[i32v(99)]), i32v(99));
        assert_eq!(trap(&mut inst, "deep", &[i32v(100)]), Trap::CallDepth);
        // 16 stack values per frame (the parameter and 15 locals): the value
        // stack (1000) overflows before the call depth (100) is reached.
        assert_eq!(trap(&mut inst, "locals", &[i32v(99)]), Trap::StackOverflow);
        assert_eq!(ok(&mut inst, "locals", &[i32v(50)]), vec![]);

        // Limits too small to even start.
        let zero = Limits {
            max_call_depth: 0,
            ..limits
        };
        let inst = instantiate_with(wat, &zero).unwrap();
        let f = inst.export_func("deep").unwrap();
        assert!(matches!(
            Execution::new(&inst, f, &[i32v(1)]),
            Err(Error::Limit { .. })
        ));
        let tiny = Limits {
            max_stack_values: 0,
            ..limits
        };
        let inst = instantiate_with(wat, &tiny).unwrap();
        assert!(matches!(
            Execution::new(&inst, f, &[i32v(1)]),
            Err(Error::Limit { .. })
        ));
    }

    #[test]
    fn instantiation() {
        // Link errors name the import.
        let m = module(r#"(module (import "a" "b" (func)) (import "a" "c" (func)))"#);
        let r = Instance::new(
            m,
            &mut |_, name, _| {
                if name == "b" {
                    Ok(HostFuncId(0))
                } else {
                    Err(())
                }
            },
            &Limits::default(),
        );
        assert_eq!(r.err(), Some(Error::Link { import: 1 }));
        // Non-function imports are unsupported in M1.
        for imp in ["(table 1 funcref)", "(memory 1)", "(global i32)"] {
            let r = instantiate_with(
                &alloc::format!(r#"(module (import "a" "b" {imp}))"#),
                &Limits::default(),
            );
            assert!(matches!(r, Err(Error::Unsupported { .. })), "{imp}: {r:?}");
        }
        // Out-of-bounds active segments trap at instantiation.
        let r = instantiate_with(
            r#"(module (memory 1) (data (i32.const 65535) "ab"))"#,
            &Limits::default(),
        );
        assert_eq!(r.err(), Some(Error::Trap(Trap::OutOfBounds)));
        let r = instantiate_with(
            r#"(module (table 1 funcref) (func) (elem (i32.const 1) func 0))"#,
            &Limits::default(),
        );
        assert_eq!(r.err(), Some(Error::Trap(Trap::TableOutOfBounds)));
        assert!(instantiate_with(
            r#"(module (memory 0) (data (i32.const 0) ""))"#,
            &Limits::default()
        )
        .is_ok());
        assert!(instantiate_with(
            r#"(module (memory 0) (data (i32.const 1) ""))"#,
            &Limits::default()
        )
        .is_err());
        // Memory larger than the limit.
        let r = instantiate_with(
            "(module (memory 2))",
            &Limits {
                max_memory_pages: 1,
                ..Limits::default()
            },
        );
        assert!(matches!(r, Err(Error::Limit { .. })));
        // The limit caps growth below the type's own maximum.
        let mut inst = instantiate_with(
            r#"(module (memory 1 10) (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0))))"#,
            &Limits {
                max_memory_pages: 2,
                ..Limits::default()
            },
        )
        .unwrap();
        assert_eq!(one(&mut inst, "grow", &[i32v(1)]), i32v(1));
        assert_eq!(one(&mut inst, "grow", &[i32v(1)]), i32v(-1));
        // Segments apply in order (later ones overwrite earlier ones); the
        // start function is recorded, not run.
        let mut inst = instantiate(
            r#"(module
                (memory 1)
                (data (i32.const 4) "\05\06")
                (data (i32.const 5) "\07")
                (global $g (mut i32) (i32.const 0))
                (func $start (global.set $g (i32.const 77)))
                (start $start)
                (func (export "g") (result i32) (global.get $g))
                (func (export "m") (result i32) (i32.load (i32.const 4)))
            )"#,
        );
        assert_eq!(inst.start_function(), Some(0));
        assert_eq!(one(&mut inst, "g", &[]), i32v(0));
        assert_eq!(one(&mut inst, "m", &[]), i32v(0x0705));
        let mut s = Execution::start(&inst).unwrap().unwrap();
        assert_eq!(s.run(&mut inst, &mut NoHost, 100), ExecResult::Done(vec![]));
        assert_eq!(one(&mut inst, "g", &[]), i32v(77));
        let inst = instantiate("(module)");
        assert!(Execution::start(&inst).unwrap().is_none());
        // Execution::new argument checks.
        let inst = instantiate(CONTROL);
        let fac = inst.export_func("fac").unwrap();
        assert_eq!(
            Execution::new(&inst, fac, &[]).err(),
            Some(Error::ArgumentMismatch)
        );
        assert_eq!(
            Execution::new(&inst, fac, &[i32v(1)]).err(),
            Some(Error::ArgumentMismatch)
        );
        assert_eq!(
            Execution::new(&inst, 999, &[]).err(),
            Some(Error::NoSuchFunction { func: 999 })
        );
        assert!(Execution::new(&inst, fac, &[i64v(1)]).is_ok());
    }
}
