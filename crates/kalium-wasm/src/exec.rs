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
//! ## Value stack
//!
//! The stack is a `Vec<u64>` of raw bits ([`Stack`]), not of [`Value`]s:
//! validation guarantees the type of every slot, so the interpreter stores
//! `i32` zero-extended, `i64` as is, `f32`/`f64` as their IEEE bits and
//! references as the index with `u64::MAX` for null ([`NULL_REF`]). Typed
//! helpers (`push_as`/`pop_as` through the [`Bits`] trait, `bin`/`un` for
//! operators that rewrite the top of the stack in place) are the only way
//! the loop touches it. The logical height `sp` lives in a local of the
//! interpreter loop; the `Vec` is only ever grown (in cold code, doubling up
//! to the limit) so that a push is one bounds check and a store. Values are
//! converted to [`Value`] only at the edges: host call arguments and
//! results, globals, tables, and the final results.
//!
//! In debug builds the stack carries a shadow `Vec<ValType>` and every
//! typed access checks it, reporting a mismatch as [`Trap::Invalid`] (a
//! trap rather than an assertion, so that unvalidated modules in the fuzz
//! tests stay panic-free). Release builds trust the validator and compile
//! the shadow away.
//!
//! ## Frames and locals
//!
//! Locals live on the value stack. When a function is entered its arguments
//! are already on top of the stack and become the first locals; the declared
//! locals are pushed after them (zero / null) and the frame's operand stack
//! grows above that. A [`Frame`] records the function, the saved pc, the
//! stack index of local 0 (`base`), the label stack height at entry and the
//! result count, all as `u32` (so `Limits::max_stack_values` is clamped to
//! [`MAX_STACK`]). Returning moves the results down to `base`, truncates
//! the stack to `base + arity` and drops the frame's labels. The loop caches
//! the current frame's `base`, `label_base`, instruction slice and pc in
//! locals and reloads them only when the frame changes; everything needed
//! only on rare paths (the frame index, the `br_table` side table, the
//! number of imports) is re-read from `self`/`inst` instead, because every
//! extra loop-carried value costs a spill in the dispatch sequence. The
//! frame and label stacks themselves stay as `Vec`s in `self`: measured
//! against moving them into loop locals too, that was 15% faster.
//!
//! ## Labels and branches
//!
//! A [`Label`] records the continuation pc, the number of values a branch
//! carries and the stack height to unwind to. Structured instructions carry
//! their matching `end`/`else` indices (see [`crate::instr`]), so branching
//! is a truncate-and-jump without scanning. A `loop` label is marked as
//! such: branching to it keeps the label and jumps to the first instruction
//! of the body, so a back-edge costs no label push and no re-dispatch of the
//! `loop` instruction. Labels are stored in a `Vec` that is allocated once
//! per execution and reused; no per-instruction allocation happens anywhere
//! on the hot path.
//!
//! ## Fuel
//!
//! Fuel is exactly one unit per executed instruction, checked before each
//! instruction (a register decrement and a never-taken branch), so
//! [`Execution::run`] with a budget of `n` executes exactly `n`
//! instructions before returning `OutOfFuel`, and `run(0)` executes
//! nothing. The structural `end` of a block or function, `else` reached
//! from a then-arm and `loop` on first entry each count as one instruction;
//! a back-edge to a `loop` counts only the branch. The granularity is
//! therefore exact: the scheduler never overshoots its budget. (Batching
//! the check per back-edge was considered and rejected: the per-instruction
//! check is two instructions out of about thirty-five per Wasm instruction
//! and measures as noise, while the kernel's accounting stays exact.)
//!
//! ## Host calls
//!
//! Host calls see their arguments as a slice of [`Value`]s built in a
//! buffer that is reused across calls; results come back the same way. On
//! [`HostOutcome::Block`] nothing is popped, anything the instruction popped
//! before the call (the table index of `call_indirect`) is pushed back, and
//! the pc is rewound to the call instruction so that resuming re-issues the
//! call with the stack exactly as it was. A `call_indirect` through a
//! function address of another instance (see [`crate::instance`]) is a
//! host call too, through [`Host::call_foreign`], with the same contract.
//!
//! ## Shared memory
//!
//! The instance's memory is a shared object ([`crate::MemoryRef`]). The
//! interpreter loop never borrows it itself: its caller (`run_slices`)
//! takes the mutable borrow and hands the loop a plain `&mut Memory`, so a
//! load or store is one bounds check against the `Vec` inside and the loop
//! carries no extra state. For a host call the loop saves its state and
//! *returns* (`Flow::HostCall`); the caller drops the borrow, makes the
//! call, takes the borrow again and re-enters the loop. The host (and any
//! instance it runs that shares the memory) may therefore use and grow the
//! memory, and every `run` and every re-entry starts with a fresh borrow,
//! so a grow by another instance between two runs is seen. Leaving the
//! loop costs a host call about a hundred extra machine instructions
//! (+17% on the host-call benchmark, which calls every 11 instructions),
//! but the loop no longer carries the `host` fat pointer or a memory guard,
//! and every other workload executes 5-10% fewer instructions than before
//! (cachegrind). The alternatives were measured and rejected: a `RefMut`
//! live inside the loop cost +6-8% on every workload, an `Option<RefMut>`
//! slot released around host calls +8-14%. Tables are borrowed for one
//! instruction at a time; globals are `Cell`s and need no borrow. A borrow
//! that fails (the host kept one across a call) ends the execution with
//! [`Trap::Host`] rather than panicking.
//!
//! ## Untrusted state
//!
//! The interpreter assumes a validated module but never panics on one that
//! is not: an index out of range, an empty stack where a value is expected
//! or a frame without a body end the execution with [`Trap::Invalid`].
//! Type confusion on the stack is detected in debug builds only (see
//! above); in release builds it yields wrong values, never memory unsafety.

use crate::error::{Error, Trap};
use crate::instance::{Host, HostCtx, HostFuncId, HostOutcome, Instance};
use crate::instr::{BlockType, FuncBody, Instr};
use crate::memory::Memory;
use crate::module::Module;
use crate::types::{RefType, ValType};
use alloc::rc::Rc;
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

    /// The stack encoding of this value (see the module documentation).
    #[inline(always)]
    pub(crate) fn to_bits(self) -> u64 {
        match self {
            Value::I32(v) => v as u32 as u64,
            Value::I64(v) => v as u64,
            Value::F32(v) => v.to_bits() as u64,
            Value::F64(v) => v.to_bits(),
            Value::FuncRef(r) | Value::ExternRef(r) => ref_to_bits(r),
        }
    }

    /// Decode a stack slot of type `ty`.
    #[inline(always)]
    pub(crate) fn from_bits(bits: u64, ty: ValType) -> Value {
        match ty {
            ValType::I32 => Value::I32(bits as u32 as i32),
            ValType::I64 => Value::I64(bits as i64),
            ValType::F32 => Value::F32(f32::from_bits(bits as u32)),
            ValType::F64 => Value::F64(f64::from_bits(bits)),
            ValType::FuncRef => Value::FuncRef(ref_from_bits(bits)),
            ValType::ExternRef => Value::ExternRef(ref_from_bits(bits)),
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

// ---------------------------------------------------------------------------
// Stack encoding
// ---------------------------------------------------------------------------

/// The stack encoding of a null reference. Indices are `u32`, so this never
/// collides with a non-null one.
const NULL_REF: u64 = u64::MAX;

#[inline(always)]
fn ref_to_bits(r: Option<u32>) -> u64 {
    match r {
        Some(i) => i as u64,
        None => NULL_REF,
    }
}

#[inline(always)]
fn ref_from_bits(bits: u64) -> Option<u32> {
    if bits == NULL_REF {
        None
    } else {
        Some(bits as u32)
    }
}

/// A reference value of type `ty`.
fn ref_value(ty: RefType, r: Option<u32>) -> Value {
    match ty {
        RefType::FuncRef => Value::FuncRef(r),
        RefType::ExternRef => Value::ExternRef(r),
    }
}

/// The stack bits of a local's initial value.
#[inline(always)]
fn default_bits(ty: ValType) -> u64 {
    if ty.is_ref() {
        NULL_REF
    } else {
        0
    }
}

/// Numeric types the typed stack helpers move in and out of stack slots.
trait Bits: Copy {
    const TY: ValType;
    fn from_bits(bits: u64) -> Self;
    fn to_bits(self) -> u64;
}

impl Bits for i32 {
    const TY: ValType = ValType::I32;
    #[inline(always)]
    fn from_bits(bits: u64) -> i32 {
        bits as u32 as i32
    }
    #[inline(always)]
    fn to_bits(self) -> u64 {
        self as u32 as u64
    }
}

impl Bits for i64 {
    const TY: ValType = ValType::I64;
    #[inline(always)]
    fn from_bits(bits: u64) -> i64 {
        bits as i64
    }
    #[inline(always)]
    fn to_bits(self) -> u64 {
        self as u64
    }
}

impl Bits for f32 {
    const TY: ValType = ValType::F32;
    #[inline(always)]
    fn from_bits(bits: u64) -> f32 {
        f32::from_bits(bits as u32)
    }
    #[inline(always)]
    fn to_bits(self) -> u64 {
        f32::to_bits(self) as u64
    }
}

impl Bits for f64 {
    const TY: ValType = ValType::F64;
    #[inline(always)]
    fn from_bits(bits: u64) -> f64 {
        f64::from_bits(bits)
    }
    #[inline(always)]
    fn to_bits(self) -> u64 {
        f64::to_bits(self)
    }
}

/// Comparison results: an `i32` that is 0 or 1.
impl Bits for bool {
    const TY: ValType = ValType::I32;
    #[inline(always)]
    fn from_bits(bits: u64) -> bool {
        bits as u32 != 0
    }
    #[inline(always)]
    fn to_bits(self) -> u64 {
        self as u64
    }
}

/// The shadow type of a stack slot: a [`ValType`] in debug builds, nothing
/// in release builds (see the module documentation).
#[cfg(debug_assertions)]
type Shadow = ValType;
#[cfg(not(debug_assertions))]
type Shadow = ();

#[cfg(debug_assertions)]
#[inline(always)]
fn shadow(ty: ValType) -> Shadow {
    ty
}

#[cfg(not(debug_assertions))]
#[inline(always)]
fn shadow(_ty: ValType) -> Shadow {}

#[cfg(debug_assertions)]
#[inline(always)]
fn shadow_is_ref(s: Shadow) -> bool {
    s.is_ref()
}

#[cfg(not(debug_assertions))]
#[inline(always)]
fn shadow_is_ref(_s: Shadow) -> bool {
    true
}

/// Grow the stack buffer whose slot `i == buf.len()` is about to be
/// written, doubling up to `max` entries. The buffer is moved in and out
/// rather than borrowed so that the address of the (register-resident)
/// stack header never escapes into this cold call.
#[cold]
#[inline(never)]
fn grow_buf<T: Copy + Default>(mut buf: Vec<T>, i: usize, max: usize) -> Result<Vec<T>, Trap> {
    if i >= max {
        return Err(Trap::StackOverflow);
    }
    let new_len = buf.len().max(64).saturating_mul(2).min(max);
    debug_assert!(new_len > i);
    buf.try_reserve_exact(new_len - buf.len())
        .map_err(|_| Trap::StackOverflow)?;
    buf.resize(new_len, T::default());
    Ok(buf)
}

/// The value stack: raw bits, grown on demand up to a limit. The logical
/// height is passed in by the caller (`sp`), which lets the interpreter
/// loop keep it in a register; the invariant is `sp <= buf.len()`.
#[derive(Default)]
struct Stack {
    buf: Vec<u64>,
    #[cfg(debug_assertions)]
    tys: Vec<ValType>,
    /// `Limits::max_stack_values`.
    max: usize,
}

impl Stack {
    fn new(max: usize) -> Stack {
        Stack {
            max,
            ..Stack::default()
        }
    }

    /// Make room for a value at index `sp == buf.len()`.
    #[inline(always)]
    fn grow(&mut self, sp: usize) -> Result<(), Trap> {
        let buf = core::mem::take(&mut self.buf);
        self.buf = grow_buf(buf, sp, self.max)?;
        #[cfg(debug_assertions)]
        {
            self.tys.resize(self.buf.len(), ValType::I32);
        }
        Ok(())
    }

    #[inline(always)]
    fn push_raw(&mut self, sp: &mut usize, v: u64, ty: Shadow) -> Result<(), Trap> {
        let i = *sp;
        match self.buf.get_mut(i) {
            Some(slot) => *slot = v,
            None => {
                self.grow(i)?;
                *self.buf.get_mut(i).ok_or(Trap::Invalid)? = v;
            }
        }
        #[cfg(debug_assertions)]
        {
            self.tys[i] = ty;
        }
        let _ = ty;
        *sp = i + 1;
        Ok(())
    }

    #[inline(always)]
    fn pop_raw(&mut self, sp: &mut usize) -> Result<(u64, Shadow), Trap> {
        let i = sp.wrapping_sub(1);
        let v = *self.buf.get(i).ok_or(Trap::Invalid)?;
        *sp = i;
        Ok((v, self.shadow_at(i)))
    }

    #[inline(always)]
    fn shadow_at(&self, i: usize) -> Shadow {
        #[cfg(debug_assertions)]
        {
            self.tys[i]
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = i;
        }
    }

    /// Debug-only type check of slot `i`.
    #[inline(always)]
    fn check(&self, i: usize, ty: ValType) -> Result<(), Trap> {
        #[cfg(debug_assertions)]
        {
            if self.tys.get(i) != Some(&ty) {
                return Err(Trap::Invalid);
            }
        }
        let _ = (i, ty);
        Ok(())
    }

    #[inline(always)]
    fn push_as<T: Bits>(&mut self, sp: &mut usize, v: T) -> Result<(), Trap> {
        self.push_raw(sp, v.to_bits(), shadow(T::TY))
    }

    #[inline(always)]
    fn pop_as<T: Bits>(&mut self, sp: &mut usize) -> Result<T, Trap> {
        let i = sp.wrapping_sub(1);
        let v = *self.buf.get(i).ok_or(Trap::Invalid)?;
        self.check(i, T::TY)?;
        *sp = i;
        Ok(T::from_bits(v))
    }

    /// Pop two values, `a` below `b`.
    #[inline(always)]
    fn pop2<A: Bits, B: Bits>(&mut self, sp: &mut usize) -> Result<(A, B), Trap> {
        let i = sp.wrapping_sub(2);
        let s = self.buf.get(i..i.wrapping_add(2)).ok_or(Trap::Invalid)?;
        let (a, b) = (s[0], s[1]);
        self.check(i, A::TY)?;
        self.check(i + 1, B::TY)?;
        *sp = i;
        Ok((A::from_bits(a), B::from_bits(b)))
    }

    #[inline(always)]
    fn push_value(&mut self, sp: &mut usize, v: Value) -> Result<(), Trap> {
        self.push_raw(sp, v.to_bits(), shadow(v.ty()))
    }

    #[inline(always)]
    fn push_ref(&mut self, sp: &mut usize, ty: RefType, r: Option<u32>) -> Result<(), Trap> {
        self.push_raw(sp, ref_to_bits(r), shadow(ty.into()))
    }

    /// Pop a reference of either type.
    #[inline(always)]
    fn pop_ref(&mut self, sp: &mut usize) -> Result<Option<u32>, Trap> {
        let (bits, ty) = self.pop_raw(sp)?;
        if !shadow_is_ref(ty) {
            return Err(Trap::Invalid);
        }
        Ok(ref_from_bits(bits))
    }

    /// Replace the top value `a` by `f(a)`.
    #[inline(always)]
    fn un<A: Bits, R: Bits>(&mut self, sp: usize, f: impl FnOnce(A) -> R) -> Result<(), Trap> {
        self.un_try(sp, |a| Ok(f(a)))
    }

    #[inline(always)]
    fn un_try<A: Bits, R: Bits>(
        &mut self,
        sp: usize,
        f: impl FnOnce(A) -> Result<R, Trap>,
    ) -> Result<(), Trap> {
        let i = sp.wrapping_sub(1);
        self.check(i, A::TY)?;
        let slot = self.buf.get_mut(i).ok_or(Trap::Invalid)?;
        *slot = f(A::from_bits(*slot))?.to_bits();
        #[cfg(debug_assertions)]
        {
            self.tys[i] = R::TY;
        }
        Ok(())
    }

    /// Replace the top two values `a b` by `f(a, b)`.
    #[inline(always)]
    fn bin<A: Bits, B: Bits, R: Bits>(
        &mut self,
        sp: &mut usize,
        f: impl FnOnce(A, B) -> R,
    ) -> Result<(), Trap> {
        self.bin_try(sp, |a, b| Ok(f(a, b)))
    }

    #[inline(always)]
    fn bin_try<A: Bits, B: Bits, R: Bits>(
        &mut self,
        sp: &mut usize,
        f: impl FnOnce(A, B) -> Result<R, Trap>,
    ) -> Result<(), Trap> {
        let i = sp.wrapping_sub(2);
        self.check(i, A::TY)?;
        self.check(i.wrapping_add(1), B::TY)?;
        let s = self
            .buf
            .get_mut(i..i.wrapping_add(2))
            .ok_or(Trap::Invalid)?;
        let (a, b) = (A::from_bits(s[0]), B::from_bits(s[1]));
        s[0] = f(a, b)?.to_bits();
        #[cfg(debug_assertions)]
        {
            self.tys[i] = R::TY;
        }
        *sp = i + 1;
        Ok(())
    }

    /// `local.get`: push slot `idx`.
    #[inline(always)]
    fn local_get(&mut self, sp: &mut usize, idx: usize) -> Result<(), Trap> {
        let v = *self.buf.get(idx).ok_or(Trap::Invalid)?;
        let ty = self.shadow_at(idx);
        self.push_raw(sp, v, ty)
    }

    /// `local.set`: pop into slot `idx` (which must be below the top).
    #[inline(always)]
    fn local_set(&mut self, sp: &mut usize, idx: usize) -> Result<(), Trap> {
        let (v, ty) = self.pop_raw(sp)?;
        self.set_slot(idx, v, ty)
    }

    /// `local.tee`: copy the top into slot `idx`.
    #[inline(always)]
    fn local_tee(&mut self, sp: usize, idx: usize) -> Result<(), Trap> {
        let i = sp.wrapping_sub(1);
        let v = *self.buf.get(i).ok_or(Trap::Invalid)?;
        let ty = self.shadow_at(i);
        self.set_slot(idx, v, ty)
    }

    #[inline(always)]
    fn set_slot(&mut self, idx: usize, v: u64, ty: Shadow) -> Result<(), Trap> {
        *self.buf.get_mut(idx).ok_or(Trap::Invalid)? = v;
        #[cfg(debug_assertions)]
        {
            self.tys[idx] = ty;
        }
        let _ = ty;
        Ok(())
    }

    /// `select`: pop the condition and two values, push one of them.
    #[inline(always)]
    fn select(&mut self, sp: &mut usize) -> Result<(), Trap> {
        let cond = self.pop_as::<i32>(sp)?;
        let (v2, t2) = self.pop_raw(sp)?;
        let (v1, t1) = self.pop_raw(sp)?;
        if t1 != t2 {
            return Err(Trap::Invalid);
        }
        self.push_raw(sp, if cond != 0 { v1 } else { v2 }, t1)
    }

    /// Move the top `n` values down to `height` and truncate to
    /// `height + n` (the stack effect of a branch or a return).
    #[inline(always)]
    fn unwind(&mut self, sp: &mut usize, height: usize, n: usize) -> Result<(), Trap> {
        let len = *sp;
        match n {
            // The common cases, with the fewest checks: no value carried,
            // or one (a single move, no memmove).
            0 => {
                if height > len {
                    return Err(Trap::Invalid);
                }
            }
            1 => {
                if height >= len {
                    return Err(Trap::Invalid);
                }
                let v = *self.buf.get(len - 1).ok_or(Trap::Invalid)?;
                *self.buf.get_mut(height).ok_or(Trap::Invalid)? = v;
                #[cfg(debug_assertions)]
                {
                    self.tys[height] = self.tys[len - 1];
                }
            }
            _ => {
                if len < n || height > len - n || len > self.buf.len() {
                    return Err(Trap::Invalid);
                }
                self.buf.copy_within(len - n..len, height);
                #[cfg(debug_assertions)]
                {
                    self.tys.copy_within(len - n..len, height);
                }
            }
        }
        *sp = height + n;
        Ok(())
    }

    /// The `n` values below `sp` as typed values, appended to `out`.
    #[inline(always)]
    fn read_values(&self, sp: usize, tys: &[ValType], out: &mut Vec<Value>) -> Result<(), Trap> {
        let start = sp.checked_sub(tys.len()).ok_or(Trap::Invalid)?;
        let bits = self.buf.get(start..sp).ok_or(Trap::Invalid)?;
        for (i, (&b, &t)) in bits.iter().zip(tys).enumerate() {
            self.check(start + i, t)?;
            out.push(Value::from_bits(b, t));
        }
        Ok(())
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

/// A call frame (20 bytes: stack and label heights are bounded by
/// [`MAX_STACK`], so they fit in `u32`).
#[derive(Clone, Copy, Debug, Default)]
struct Frame {
    /// Function index.
    func: u32,
    /// Saved pc: the resume point while another frame is active, or the
    /// point to resume at after `OutOfFuel`/`Blocked`.
    pc: u32,
    /// Stack index of local 0.
    base: u32,
    /// Index into the label stack of this frame's first label.
    label_base: u32,
    /// Number of results.
    arity: u32,
}

/// A control label (16 bytes).
#[derive(Clone, Copy, Debug, Default)]
struct Label {
    /// pc to continue at when branching to this label: after the `end` of a
    /// block, the first body instruction of a loop.
    cont: u32,
    /// Number of values a branch carries to the continuation.
    arity: u32,
    /// Stack height to unwind to before pushing the carried values.
    height: u32,
    /// A `loop` label survives branches to it.
    is_loop: bool,
}

/// Upper bound on `Limits::max_stack_values` as applied by the interpreter:
/// stack heights are stored as `u32` in frames and labels. (Larger limits
/// are clamped; 4 G values is 32 GiB of stack.)
const MAX_STACK: usize = u32::MAX as usize;

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
    /// The loop stopped at a call that the host must make (state saved,
    /// arguments on the stack, pc after the call instruction); only
    /// `run_loop` returns this. `restore` is what the instruction popped
    /// before the call (the table index of `call_indirect`), to be pushed
    /// back if the host blocks.
    HostCall {
        target: HostTarget,
        restore: Option<i32>,
    },
}

/// What a host call targets.
#[derive(Clone, Copy)]
enum HostTarget {
    /// Imported function (function index space) through [`Host::call`].
    Import(u32),
    /// A function of another instance, by function address, through
    /// [`Host::call_foreign`]; `ty` is the type index `call_indirect`
    /// expects.
    Foreign { addr: u32, ty: u32 },
}

/// A resumable execution of one function: value stack, call frames and
/// program counter. See the module documentation.
pub struct Execution {
    /// The value stack. The interpreter loop moves it into a local for its
    /// duration (see the module documentation).
    stack: Stack,
    /// Logical stack height between `run` calls.
    sp: usize,
    frames: Vec<Frame>,
    labels: Vec<Label>,
    state: State,
    /// `Limits::max_call_depth`.
    max_depth: usize,
    /// `Limits::max_stack_values`, which also bounds the label stack.
    max_stack: usize,
    /// Reused buffers for host call arguments and results.
    host_args: Vec<Value>,
    host_results: Vec<Value>,
    /// Instructions executed so far.
    fuel_used: u64,
    /// The entry function (function index space); its results type the
    /// values left on the stack when the execution is done.
    entry: u32,
    /// Set when the entry function is an import: the whole execution is one
    /// host call with the arguments on the stack (and no frame).
    entry_import: bool,
}

impl fmt::Debug for Execution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Execution")
            .field("stack_len", &self.sp)
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

#[cold]
#[inline(never)]
fn div_s_trap<T: PartialEq>(b: T, zero: T) -> Trap {
    if b == zero {
        Trap::IntegerDivideByZero
    } else {
        Trap::IntegerOverflow
    }
}

#[inline(always)]
fn i32_div_s(a: i32, b: i32) -> Result<i32, Trap> {
    match a.checked_div(b) {
        Some(v) => Ok(v),
        None => Err(div_s_trap(b, 0)),
    }
}

#[inline(always)]
fn i64_div_s(a: i64, b: i64) -> Result<i64, Trap> {
    match a.checked_div(b) {
        Some(v) => Ok(v),
        None => Err(div_s_trap(b, 0)),
    }
}

#[inline(always)]
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
            stack: Stack::new(limits.max_stack_values.min(MAX_STACK)),
            sp: 0,
            frames: Vec::new(),
            labels: Vec::new(),
            state: State::Running,
            max_depth: limits.max_call_depth.min(MAX_STACK),
            max_stack: limits.max_stack_values.min(MAX_STACK),
            host_args: Vec::new(),
            host_results: Vec::new(),
            fuel_used: 0,
            entry: func,
            entry_import: inst.is_imported_func(func),
        };
        let setup = (|| {
            let mut stack = core::mem::take(&mut exec.stack);
            let mut sp = 0;
            for &a in args {
                stack.push_value(&mut sp, a)?;
            }
            if !exec.entry_import {
                exec.push_frame(&mut stack, inst.module(), inst, func, &mut sp)?;
            }
            exec.stack = stack;
            exec.sp = sp;
            Ok(())
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
        self.sp
    }

    /// Total fuel consumed (instructions executed) over all `run` calls.
    pub fn fuel_used(&self) -> u64 {
        self.fuel_used
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
        let flow = if self.entry_import {
            self.run_entry_import(&module, inst, host, fuel)
        } else {
            self.run_inner(&module, inst, host, fuel)
        };
        let result = match flow {
            Ok(Flow::OutOfFuel) => return ExecResult::OutOfFuel,
            Ok(Flow::Blocked) => return ExecResult::Blocked,
            Ok(Flow::HostCall { .. }) => ExecResult::Trap(Trap::Invalid),
            Ok(Flow::Done) => match self.results(inst) {
                Ok(values) => ExecResult::Done(values),
                Err(trap) => ExecResult::Trap(trap),
            },
            Ok(Flow::Exited(code)) => ExecResult::Exited(code),
            Err(trap) => ExecResult::Trap(trap),
        };
        self.stack = Stack::default();
        self.frames = Vec::new();
        self.labels = Vec::new();
        self.sp = 0;
        self.host_args = Vec::new();
        self.host_results = Vec::new();
        self.state = State::Finished(result.clone());
        result
    }

    /// The results left on the stack by a finished execution.
    fn results(&self, inst: &Instance) -> Result<Vec<Value>, Trap> {
        let ty = inst.func_type(self.entry).ok_or(Trap::Invalid)?;
        if self.sp != ty.results.len() {
            return Err(Trap::Invalid);
        }
        let mut out = Vec::with_capacity(ty.results.len());
        self.stack.read_values(self.sp, &ty.results, &mut out)?;
        Ok(out)
    }

    /// An execution whose entry function is an import: one host call with
    /// the arguments on the stack.
    fn run_entry_import(
        &mut self,
        module: &Module,
        inst: &mut Instance,
        host: &mut dyn Host,
        fuel: u64,
    ) -> Result<Flow, Trap> {
        if fuel == 0 {
            return Ok(Flow::OutOfFuel);
        }
        self.fuel_used += 1;
        let outcome = self.host_call(module, inst, host, HostTarget::Import(self.entry));
        Ok(match outcome? {
            HostOutcome::Continue => Flow::Done,
            HostOutcome::Block => Flow::Blocked,
            HostOutcome::Exit(code) => Flow::Exited(code),
        })
    }

    // ---- frames and labels ----------------------------------------------

    #[inline(always)]
    fn body_of<'m>(module: &'m Module, inst: &Instance, func: u32) -> Result<&'m FuncBody, Trap> {
        let defined = (func as usize)
            .checked_sub(inst.num_imported_funcs())
            .ok_or(Trap::Invalid)?;
        module.code.get(defined).ok_or(Trap::Invalid)
    }

    /// Save the pc of the current (innermost) frame.
    #[inline(always)]
    fn save_pc(&mut self, pc: usize) {
        if let Some(f) = self.frames.last_mut() {
            f.pc = pc as u32;
        }
    }

    /// Enter defined function `func`; its arguments are the top of the stack.
    /// Returns the callee's body and the new frame.
    #[inline(always)]
    fn push_frame<'m>(
        &mut self,
        stack: &mut Stack,
        module: &'m Module,
        inst: &Instance,
        func: u32,
        sp: &mut usize,
    ) -> Result<(&'m FuncBody, Frame), Trap> {
        if self.frames.len() >= self.max_depth {
            return Err(Trap::CallDepth);
        }
        let (nparams, nresults) = inst.func_arity(func).ok_or(Trap::Invalid)?;
        let body = Self::body_of(module, inst, func)?;
        let base = sp.checked_sub(nparams).ok_or(Trap::Invalid)?;
        for &l in &body.locals {
            stack.push_raw(sp, default_bits(l), shadow(l))?;
        }
        let frame = Frame {
            func,
            pc: 0,
            base: base as u32,
            label_base: self.labels.len() as u32,
            arity: nresults as u32,
        };
        self.frames.push(frame);
        Ok((body, frame))
    }

    /// Leave the current frame, moving its results to the frame base.
    /// Returns the caller's frame, or `None` if this was the outermost one.
    #[inline(always)]
    fn do_return(&mut self, stack: &mut Stack, sp: &mut usize) -> Result<Option<Frame>, Trap> {
        let frame = self.frames.pop().ok_or(Trap::Invalid)?;
        stack.unwind(sp, frame.base as usize, frame.arity as usize)?;
        self.labels.truncate(frame.label_base as usize);
        Ok(self.frames.last().copied())
    }

    #[inline(always)]
    fn push_label(
        &mut self,
        cont: u32,
        arity: usize,
        nparams: usize,
        is_loop: bool,
        sp: usize,
    ) -> Result<(), Trap> {
        if self.labels.len() >= self.max_stack {
            return Err(Trap::StackOverflow);
        }
        let height = sp.checked_sub(nparams).ok_or(Trap::Invalid)?;
        self.labels.push(Label {
            cont,
            arity: arity as u32,
            height: height as u32,
            is_loop,
        });
        Ok(())
    }

    /// Pop the innermost label (`else` reached from a then-arm).
    #[inline(always)]
    fn pop_label(&mut self) -> Result<(), Trap> {
        self.labels.pop().map(|_| ()).ok_or(Trap::Invalid)
    }

    /// `end`: pop the innermost label of the frame whose labels start at
    /// `label_base`, or report that the frame itself ends (no label left).
    #[inline(always)]
    fn end_label(&mut self, label_base: usize) -> bool {
        if self.labels.len() > label_base {
            self.labels.pop();
            true
        } else {
            false
        }
    }

    /// Branch to the label `depth` levels out in the frame whose labels
    /// start at `label_base`.
    #[inline(always)]
    fn branch(
        &mut self,
        stack: &mut Stack,
        label_base: usize,
        depth: u32,
        sp: &mut usize,
    ) -> Result<Jump, Trap> {
        let n_labels = self.labels.len();
        let in_frame = n_labels.saturating_sub(label_base);
        if depth as usize >= in_frame {
            return Ok(Jump::Return);
        }
        let li = n_labels - 1 - depth as usize;
        let label = *self.labels.get(li).ok_or(Trap::Invalid)?;
        stack.unwind(sp, label.height as usize, label.arity as usize)?;
        // A loop label stays: the branch re-enters the loop body.
        self.labels.truncate(li + label.is_loop as usize);
        Ok(Jump::To(label.cont))
    }

    /// Call an imported or foreign function through the host, with the
    /// arguments on top of `self.stack`; they stay there until the host
    /// returns `Continue`. The interpreter loop is not running (its state is
    /// in `self`) and nobody holds a borrow of the instance's memory.
    fn host_call(
        &mut self,
        module: &Module,
        inst: &mut Instance,
        host: &mut dyn Host,
        target: HostTarget,
    ) -> Result<HostOutcome, Trap> {
        let (id, ty): (Result<HostFuncId, u32>, _) = match target {
            HostTarget::Import(func) => (
                Ok(inst.host_func(func).ok_or(Trap::Invalid)?),
                inst.func_type_index(func)
                    .and_then(|t| module.types.get(t as usize))
                    .ok_or(Trap::Invalid)?,
            ),
            HostTarget::Foreign { addr, ty } => (
                Err(addr),
                module.types.get(ty as usize).ok_or(Trap::Invalid)?,
            ),
        };
        let start = self.sp.checked_sub(ty.params.len()).ok_or(Trap::Invalid)?;
        self.host_args.clear();
        self.host_results.clear();
        self.stack
            .read_values(self.sp, &ty.params, &mut self.host_args)?;
        self.host_results
            .extend(ty.results.iter().map(|&t| Value::default_for(t)));
        let mut ctx = HostCtx {
            memory: &inst.memory,
        };
        let outcome = match id {
            Ok(id) => host.call(&mut ctx, id, &self.host_args, &mut self.host_results),
            Err(addr) => {
                host.call_foreign(&mut ctx, addr, ty, &self.host_args, &mut self.host_results)
            }
        }?;
        if outcome == HostOutcome::Continue {
            let results = &self.host_results;
            let well_typed = results.len() == ty.results.len()
                && results.iter().zip(&ty.results).all(|(v, &t)| v.ty() == t);
            if !well_typed {
                return Err(Trap::Host);
            }
            self.sp = start;
            for &v in results {
                self.stack.push_value(&mut self.sp, v)?;
            }
        }
        Ok(outcome)
    }

    // ---- the interpreter loop -------------------------------------------

    fn run_inner(
        &mut self,
        module: &Module,
        inst: &mut Instance,
        host: &mut dyn Host,
        fuel: u64,
    ) -> Result<Flow, Trap> {
        let mut left = fuel;
        let r = self.run_slices(module, inst, host, &mut left);
        self.fuel_used += fuel - left;
        r
    }

    /// Run the interpreter loop, making the host calls it stops at: the
    /// memory is borrowed for each slice of the loop and released around
    /// every host call (see the module documentation).
    fn run_slices(
        &mut self,
        module: &Module,
        inst: &mut Instance,
        host: &mut dyn Host,
        fuel: &mut u64,
    ) -> Result<Flow, Trap> {
        let memory = inst.memory.clone();
        loop {
            let flow = {
                let mut mem = memory.try_borrow_mut().map_err(|_| Trap::Host)?;
                self.run_loop(module, inst, &mut mem, fuel)?
            };
            let Flow::HostCall { target, restore } = flow else {
                return Ok(flow);
            };
            match self.host_call(module, inst, host, target)? {
                HostOutcome::Continue => {}
                HostOutcome::Block => {
                    // Rewind to the call so that resuming re-issues it with
                    // the stack exactly as it was.
                    if let Some(v) = restore {
                        self.stack.push_as::<i32>(&mut self.sp, v)?;
                    }
                    let frame = self.frames.last_mut().ok_or(Trap::Invalid)?;
                    frame.pc = frame.pc.checked_sub(1).ok_or(Trap::Invalid)?;
                    return Ok(Flow::Blocked);
                }
                HostOutcome::Exit(code) => return Ok(Flow::Exited(code)),
            }
        }
    }

    #[allow(clippy::cognitive_complexity)]
    #[inline(never)]
    fn run_loop(
        &mut self,
        module: &Module,
        inst: &mut Instance,
        mem: &mut Memory,
        fuel_ref: &mut u64,
    ) -> Result<Flow, Trap> {
        let mut fuel = *fuel_ref;
        let mut sp = self.sp;
        // The stack is a local for the duration of the loop so that its
        // buffer pointer and length can live in registers (see the module
        // documentation); it goes back into `self` on every exit that can
        // be resumed. A trap ends the execution, so losing it there is fine.
        let mut stack = core::mem::take(&mut self.stack);
        let frame = *self.frames.last().ok_or(Trap::Invalid)?;
        // Only the instruction slice of the current function is kept in a
        // local; everything else about the frame that is needed rarely
        // (`br_table` side tables, the saved pc) is re-read from
        // `self.frames.last()` to keep the loop's register pressure low.
        let mut code: &[Instr] = &Self::body_of(module, inst, frame.func)?.instrs;
        let mut pc = frame.pc as usize;
        let mut base = frame.base as usize;
        let mut label_base = frame.label_base as usize;

        // Save the loop state into `self` before leaving the loop.
        macro_rules! save {
            () => {{
                self.save_pc(pc);
                self.stack = stack;
                self.sp = sp;
                *fuel_ref = fuel;
            }};
        }
        // Switch to the caller's frame after a return.
        macro_rules! ret {
            () => {
                match self.do_return(&mut stack, &mut sp)? {
                    Some(frame) => {
                        code = &Self::body_of(module, inst, frame.func)?.instrs;
                        pc = frame.pc as usize;
                        base = frame.base as usize;
                        label_base = frame.label_base as usize;
                    }
                    None => {
                        self.stack = stack;
                        self.sp = sp;
                        *fuel_ref = fuel;
                        return Ok(Flow::Done);
                    }
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
        // A call through the host: leave the loop with the state saved;
        // `run_slices` makes the call and re-enters.
        macro_rules! host_dispatch {
            ($target:expr, $restore:expr) => {{
                save!();
                return Ok(Flow::HostCall {
                    target: $target,
                    restore: $restore,
                });
            }};
        }
        macro_rules! call {
            ($f:expr) => {
                call!($f, None)
            };
            ($f:expr, $restore:expr) => {{
                let f: u32 = $f;
                if inst.is_imported_func(f) {
                    host_dispatch!(HostTarget::Import(f), $restore);
                } else {
                    self.save_pc(pc);
                    let (callee, frame) = self.push_frame(&mut stack, module, inst, f, &mut sp)?;
                    code = &callee.instrs;
                    pc = 0;
                    base = frame.base as usize;
                    label_base = frame.label_base as usize;
                }
            }};
        }
        macro_rules! local {
            ($idx:expr) => {
                base.wrapping_add($idx as usize)
            };
        }
        // A table, borrowed for the current instruction.
        macro_rules! table {
            ($idx:expr) => {
                inst.tables
                    .get($idx as usize)
                    .ok_or(Trap::Invalid)?
                    .try_borrow_mut()
                    .map_err(|_| Trap::Host)?
            };
        }
        macro_rules! pop {
            ($t:ty) => {
                stack.pop_as::<$t>(&mut sp)?
            };
        }
        macro_rules! push {
            ($v:expr) => {
                stack.push_as(&mut sp, $v)?
            };
        }
        macro_rules! un {
            ($f:expr) => {
                stack.un(sp, $f)?
            };
        }
        macro_rules! un_try {
            ($f:expr) => {
                stack.un_try(sp, $f)?
            };
        }
        macro_rules! bin {
            ($f:expr) => {
                stack.bin(&mut sp, $f)?
            };
        }
        macro_rules! bin_try {
            ($f:expr) => {
                stack.bin_try(&mut sp, $f)?
            };
        }
        // A load replaces the address on top of the stack by the value.
        macro_rules! load {
            ($n:literal, $m:expr, $conv:expr) => {{
                let marg = $m;
                let m = &*mem;
                un_try!(|addr: i32| m.load::<$n>(addr as u32, marg.offset).map($conv))
            }};
        }
        macro_rules! store {
            ($t:ty, $m:expr, |$v:ident| $bytes:expr) => {{
                let marg = $m;
                let (addr, $v) = stack.pop2::<i32, $t>(&mut sp)?;
                mem.store(addr as u32, marg.offset, &$bytes)?;
            }};
        }

        loop {
            if fuel == 0 {
                save!();
                return Ok(Flow::OutOfFuel);
            }
            fuel -= 1;
            let instr = *code.get(pc).ok_or(Trap::Invalid)?;
            pc += 1;

            match instr {
                // ---- control ------------------------------------------
                Instr::Unreachable => return Err(Trap::Unreachable),
                Instr::Nop => {}
                Instr::Block { ty, end } => {
                    let (np, nr) = block_arity(module, ty)?;
                    self.push_label(end + 1, nr, np, false, sp)?;
                }
                Instr::Loop { ty, .. } => {
                    let (np, _) = block_arity(module, ty)?;
                    self.push_label(pc as u32, np, np, true, sp)?;
                }
                Instr::If { ty, else_, end } => {
                    let cond = pop!(i32);
                    let (np, nr) = block_arity(module, ty)?;
                    if cond != 0 {
                        self.push_label(end + 1, nr, np, false, sp)?;
                    } else if else_ != end {
                        self.push_label(end + 1, nr, np, false, sp)?;
                        pc = else_ as usize + 1;
                    } else {
                        pc = end as usize + 1;
                    }
                }
                Instr::Else { end } => {
                    // Reached from the then arm: the block is complete.
                    self.pop_label()?;
                    pc = end as usize + 1;
                }
                Instr::End => {
                    if !self.end_label(label_base) {
                        ret!();
                    }
                }
                Instr::Br(depth) => jump!(self.branch(&mut stack, label_base, depth, &mut sp)?),
                Instr::BrIf(depth) => {
                    if pop!(i32) != 0 {
                        jump!(self.branch(&mut stack, label_base, depth, &mut sp)?);
                    }
                }
                Instr::BrTable(idx) => {
                    let i = pop!(i32) as u32;
                    let func = self.frames.last().ok_or(Trap::Invalid)?.func;
                    let table = Self::body_of(module, inst, func)?
                        .br_table(idx)
                        .ok_or(Trap::Invalid)?;
                    let depth = table
                        .targets
                        .get(i as usize)
                        .copied()
                        .unwrap_or(table.default);
                    jump!(self.branch(&mut stack, label_base, depth, &mut sp)?);
                }
                Instr::Return => ret!(),
                Instr::Call(f) => call!(f),
                Instr::CallIndirect { ty, table } => {
                    let i = pop!(i32) as u32;
                    let entry = inst
                        .tables
                        .get(table as usize)
                        .ok_or(Trap::Invalid)?
                        .try_borrow()
                        .map_err(|_| Trap::Host)?
                        .get(i);
                    let addr = match entry {
                        Ok(Value::FuncRef(Some(a))) => a,
                        Ok(Value::FuncRef(None)) => return Err(Trap::UninitializedElement),
                        Ok(_) => return Err(Trap::Invalid),
                        Err(_) => return Err(Trap::UndefinedElement),
                    };
                    // The address names a function of this instance (the
                    // common case, one subtraction and two compares), one
                    // of its imports, or a foreign function.
                    let f = addr.wrapping_sub(inst.func_base);
                    let f = if (f as usize) < inst.num_funcs()
                        && (f as usize) >= inst.num_imported_funcs()
                    {
                        f
                    } else {
                        match inst.import_by_addr(addr) {
                            Some(f) => f,
                            None => {
                                host_dispatch!(HostTarget::Foreign { addr, ty }, Some(i as i32))
                            }
                        }
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
                    call!(f, Some(i as i32));
                }

                // ---- parametric ---------------------------------------
                Instr::Drop => {
                    stack.pop_raw(&mut sp)?;
                }
                Instr::Select | Instr::SelectT(_) => stack.select(&mut sp)?,

                // ---- variables ----------------------------------------
                Instr::LocalGet(i) => stack.local_get(&mut sp, local!(i))?,
                Instr::LocalSet(i) => stack.local_set(&mut sp, local!(i))?,
                Instr::LocalTee(i) => stack.local_tee(sp, local!(i))?,
                Instr::GlobalGet(i) => {
                    let v = inst.globals.get(i as usize).ok_or(Trap::Invalid)?.get();
                    stack.push_value(&mut sp, v)?;
                }
                Instr::GlobalSet(i) => {
                    let (bits, ty) = stack.pop_raw(&mut sp)?;
                    let g = inst.globals.get(i as usize).ok_or(Trap::Invalid)?;
                    let gty = g.ty().ty;
                    if shadow(gty) != ty {
                        return Err(Trap::Invalid);
                    }
                    g.store(Value::from_bits(bits, gty));
                }

                // ---- references ---------------------------------------
                Instr::RefNull(t) => stack.push_ref(&mut sp, t, None)?,
                Instr::RefIsNull => {
                    let r = stack.pop_ref(&mut sp)?;
                    push!(r.is_none());
                }
                Instr::RefFunc(f) => {
                    let addr = inst.func_addr(f).ok_or(Trap::Invalid)?;
                    stack.push_ref(&mut sp, RefType::FuncRef, Some(addr))?;
                }

                // ---- tables -------------------------------------------
                Instr::TableGet(t) => {
                    let i = pop!(i32) as u32;
                    let v = table!(t).get(i)?;
                    stack.push_value(&mut sp, v)?;
                }
                Instr::TableSet(t) => {
                    let r = stack.pop_ref(&mut sp)?;
                    let i = pop!(i32) as u32;
                    let mut tb = table!(t);
                    let elem = tb.elem_type();
                    tb.set(i, ref_value(elem, r))?;
                }
                Instr::TableSize(t) => {
                    let n = table!(t).size();
                    push!(n as i32);
                }
                Instr::TableGrow(t) => {
                    let n = pop!(i32) as u32;
                    let init = stack.pop_ref(&mut sp)?;
                    let elem = table!(t).elem_type();
                    let r = inst
                        .grow_table(t, n, ref_value(elem, init))?
                        .map_or(-1, |old| old as i32);
                    push!(r);
                }
                Instr::TableFill(t) => {
                    let n = pop!(i32) as u32;
                    let v = stack.pop_ref(&mut sp)?;
                    let i = pop!(i32) as u32;
                    let mut tb = table!(t);
                    let elem = tb.elem_type();
                    tb.fill(i, ref_value(elem, v), n)?;
                }
                Instr::TableCopy { dst, src } => {
                    let n = pop!(i32) as u32;
                    let (d, s) = stack.pop2::<i32, i32>(&mut sp)?;
                    let (d, s) = (d as u32, s as u32);
                    let dt = inst.tables.get(dst as usize).ok_or(Trap::Invalid)?;
                    let st = inst.tables.get(src as usize).ok_or(Trap::Invalid)?;
                    // Two table indices may name one shared table.
                    if Rc::ptr_eq(dt, st) {
                        dt.try_borrow_mut()
                            .map_err(|_| Trap::Host)?
                            .copy_within(d, s, n)?;
                    } else {
                        let st = st.try_borrow().map_err(|_| Trap::Host)?;
                        dt.try_borrow_mut()
                            .map_err(|_| Trap::Host)?
                            .init(d, st.entries(), s, n)?;
                    }
                }
                Instr::TableInit { elem, table } => {
                    let n = pop!(i32) as u32;
                    let (d, s) = stack.pop2::<i32, i32>(&mut sp)?;
                    let seg = inst.elems.get(elem as usize).ok_or(Trap::Invalid)?;
                    table!(table).init(d as u32, seg, s as u32, n)?;
                }
                Instr::ElemDrop(e) => {
                    *inst.elems.get_mut(e as usize).ok_or(Trap::Invalid)? = Vec::new();
                }

                // ---- memory -------------------------------------------
                Instr::I32Load(m) => load!(4, m, i32::from_le_bytes),
                Instr::I64Load(m) => load!(8, m, i64::from_le_bytes),
                Instr::F32Load(m) => load!(4, m, f32::from_le_bytes),
                Instr::F64Load(m) => load!(8, m, f64::from_le_bytes),
                Instr::I32Load8S(m) => load!(1, m, |b| b[0] as i8 as i32),
                Instr::I32Load8U(m) => load!(1, m, |b| b[0] as i32),
                Instr::I32Load16S(m) => load!(2, m, |b| i16::from_le_bytes(b) as i32),
                Instr::I32Load16U(m) => load!(2, m, |b| u16::from_le_bytes(b) as i32),
                Instr::I64Load8S(m) => load!(1, m, |b| b[0] as i8 as i64),
                Instr::I64Load8U(m) => load!(1, m, |b| b[0] as i64),
                Instr::I64Load16S(m) => load!(2, m, |b| i16::from_le_bytes(b) as i64),
                Instr::I64Load16U(m) => load!(2, m, |b| u16::from_le_bytes(b) as i64),
                Instr::I64Load32S(m) => load!(4, m, |b| i32::from_le_bytes(b) as i64),
                Instr::I64Load32U(m) => load!(4, m, |b| u32::from_le_bytes(b) as i64),
                Instr::I32Store(m) => store!(i32, m, |v| v.to_le_bytes()),
                Instr::I64Store(m) => store!(i64, m, |v| v.to_le_bytes()),
                Instr::F32Store(m) => store!(f32, m, |v| v.to_le_bytes()),
                Instr::F64Store(m) => store!(f64, m, |v| v.to_le_bytes()),
                Instr::I32Store8(m) => store!(i32, m, |v| [v as u8]),
                Instr::I32Store16(m) => store!(i32, m, |v| (v as u16).to_le_bytes()),
                Instr::I64Store8(m) => store!(i64, m, |v| [v as u8]),
                Instr::I64Store16(m) => store!(i64, m, |v| (v as u16).to_le_bytes()),
                Instr::I64Store32(m) => store!(i64, m, |v| (v as u32).to_le_bytes()),
                Instr::MemorySize => {
                    let n = mem.size_pages();
                    push!(n as i32);
                }
                Instr::MemoryGrow => {
                    un!(|n: i32| mem.grow(n as u32).map_or(-1, |old| old as i32));
                }
                Instr::MemoryFill => {
                    let n = pop!(i32) as u32;
                    let (d, v) = stack.pop2::<i32, i32>(&mut sp)?;
                    mem.fill(d as u32, v as u8, n)?;
                }
                Instr::MemoryCopy => {
                    let n = pop!(i32) as u32;
                    let (d, s) = stack.pop2::<i32, i32>(&mut sp)?;
                    mem.copy_within(d as u32, s as u32, n)?;
                }
                Instr::MemoryInit(seg) => {
                    let n = pop!(i32) as u32;
                    let (d, s) = stack.pop2::<i32, i32>(&mut sp)?;
                    let dropped = *inst.data_dropped.get(seg as usize).ok_or(Trap::Invalid)?;
                    let data: &[u8] = if dropped {
                        &[]
                    } else {
                        &module.data.get(seg as usize).ok_or(Trap::Invalid)?.bytes
                    };
                    let start = s as u32 as usize;
                    let end = start.checked_add(n as usize).ok_or(Trap::OutOfBounds)?;
                    if end > data.len() {
                        return Err(Trap::OutOfBounds);
                    }
                    // `write` checks the destination before touching memory.
                    mem.write(d as u32, &data[start..end])?;
                }
                Instr::DataDrop(seg) => {
                    *inst
                        .data_dropped
                        .get_mut(seg as usize)
                        .ok_or(Trap::Invalid)? = true;
                }

                // ---- constants ----------------------------------------
                Instr::I32Const(v) => push!(v),
                Instr::I64Const(v) => push!(v),
                Instr::F32Const(bits) => push!(f32::from_bits(bits)),
                Instr::F64Const(bits) => push!(f64::from_bits(bits)),

                // ---- i32 ----------------------------------------------
                Instr::I32Eqz => un!(|a: i32| a == 0),
                Instr::I32Eq => bin!(|a: i32, b: i32| a == b),
                Instr::I32Ne => bin!(|a: i32, b: i32| a != b),
                Instr::I32LtS => bin!(|a: i32, b: i32| a < b),
                Instr::I32LtU => bin!(|a: i32, b: i32| (a as u32) < (b as u32)),
                Instr::I32GtS => bin!(|a: i32, b: i32| a > b),
                Instr::I32GtU => bin!(|a: i32, b: i32| (a as u32) > (b as u32)),
                Instr::I32LeS => bin!(|a: i32, b: i32| a <= b),
                Instr::I32LeU => bin!(|a: i32, b: i32| (a as u32) <= (b as u32)),
                Instr::I32GeS => bin!(|a: i32, b: i32| a >= b),
                Instr::I32GeU => bin!(|a: i32, b: i32| (a as u32) >= (b as u32)),
                Instr::I32Clz => un!(|a: i32| a.leading_zeros() as i32),
                Instr::I32Ctz => un!(|a: i32| a.trailing_zeros() as i32),
                Instr::I32Popcnt => un!(|a: i32| a.count_ones() as i32),
                Instr::I32Add => bin!(|a: i32, b: i32| a.wrapping_add(b)),
                Instr::I32Sub => bin!(|a: i32, b: i32| a.wrapping_sub(b)),
                Instr::I32Mul => bin!(|a: i32, b: i32| a.wrapping_mul(b)),
                Instr::I32DivS => bin_try!(i32_div_s),
                Instr::I32DivU => bin_try!(|a: i32, b: i32| {
                    (a as u32)
                        .checked_div(b as u32)
                        .map(|v| v as i32)
                        .ok_or(Trap::IntegerDivideByZero)
                }),
                Instr::I32RemS => bin_try!(|a: i32, b: i32| {
                    if b == 0 {
                        Err(Trap::IntegerDivideByZero)
                    } else {
                        Ok(a.wrapping_rem(b))
                    }
                }),
                Instr::I32RemU => bin_try!(|a: i32, b: i32| {
                    (a as u32)
                        .checked_rem(b as u32)
                        .map(|v| v as i32)
                        .ok_or(Trap::IntegerDivideByZero)
                }),
                Instr::I32And => bin!(|a: i32, b: i32| a & b),
                Instr::I32Or => bin!(|a: i32, b: i32| a | b),
                Instr::I32Xor => bin!(|a: i32, b: i32| a ^ b),
                Instr::I32Shl => bin!(|a: i32, b: i32| a.wrapping_shl(b as u32)),
                Instr::I32ShrS => bin!(|a: i32, b: i32| a.wrapping_shr(b as u32)),
                Instr::I32ShrU => {
                    bin!(|a: i32, b: i32| (a as u32).wrapping_shr(b as u32) as i32)
                }
                Instr::I32Rotl => bin!(|a: i32, b: i32| a.rotate_left(b as u32 & 31)),
                Instr::I32Rotr => bin!(|a: i32, b: i32| a.rotate_right(b as u32 & 31)),

                // ---- i64 ----------------------------------------------
                Instr::I64Eqz => un!(|a: i64| a == 0),
                Instr::I64Eq => bin!(|a: i64, b: i64| a == b),
                Instr::I64Ne => bin!(|a: i64, b: i64| a != b),
                Instr::I64LtS => bin!(|a: i64, b: i64| a < b),
                Instr::I64LtU => bin!(|a: i64, b: i64| (a as u64) < (b as u64)),
                Instr::I64GtS => bin!(|a: i64, b: i64| a > b),
                Instr::I64GtU => bin!(|a: i64, b: i64| (a as u64) > (b as u64)),
                Instr::I64LeS => bin!(|a: i64, b: i64| a <= b),
                Instr::I64LeU => bin!(|a: i64, b: i64| (a as u64) <= (b as u64)),
                Instr::I64GeS => bin!(|a: i64, b: i64| a >= b),
                Instr::I64GeU => bin!(|a: i64, b: i64| (a as u64) >= (b as u64)),
                Instr::I64Clz => un!(|a: i64| a.leading_zeros() as i64),
                Instr::I64Ctz => un!(|a: i64| a.trailing_zeros() as i64),
                Instr::I64Popcnt => un!(|a: i64| a.count_ones() as i64),
                Instr::I64Add => bin!(|a: i64, b: i64| a.wrapping_add(b)),
                Instr::I64Sub => bin!(|a: i64, b: i64| a.wrapping_sub(b)),
                Instr::I64Mul => bin!(|a: i64, b: i64| a.wrapping_mul(b)),
                Instr::I64DivS => bin_try!(i64_div_s),
                Instr::I64DivU => bin_try!(|a: i64, b: i64| {
                    (a as u64)
                        .checked_div(b as u64)
                        .map(|v| v as i64)
                        .ok_or(Trap::IntegerDivideByZero)
                }),
                Instr::I64RemS => bin_try!(|a: i64, b: i64| {
                    if b == 0 {
                        Err(Trap::IntegerDivideByZero)
                    } else {
                        Ok(a.wrapping_rem(b))
                    }
                }),
                Instr::I64RemU => bin_try!(|a: i64, b: i64| {
                    (a as u64)
                        .checked_rem(b as u64)
                        .map(|v| v as i64)
                        .ok_or(Trap::IntegerDivideByZero)
                }),
                Instr::I64And => bin!(|a: i64, b: i64| a & b),
                Instr::I64Or => bin!(|a: i64, b: i64| a | b),
                Instr::I64Xor => bin!(|a: i64, b: i64| a ^ b),
                Instr::I64Shl => bin!(|a: i64, b: i64| a.wrapping_shl(b as u32)),
                Instr::I64ShrS => bin!(|a: i64, b: i64| a.wrapping_shr(b as u32)),
                Instr::I64ShrU => {
                    bin!(|a: i64, b: i64| (a as u64).wrapping_shr(b as u32) as i64)
                }
                Instr::I64Rotl => bin!(|a: i64, b: i64| a.rotate_left(b as u32 & 63)),
                Instr::I64Rotr => bin!(|a: i64, b: i64| a.rotate_right(b as u32 & 63)),

                // ---- f32 ----------------------------------------------
                Instr::F32Eq => bin!(|a: f32, b: f32| a == b),
                Instr::F32Ne => bin!(|a: f32, b: f32| a != b),
                Instr::F32Lt => bin!(|a: f32, b: f32| a < b),
                Instr::F32Gt => bin!(|a: f32, b: f32| a > b),
                Instr::F32Le => bin!(|a: f32, b: f32| a <= b),
                Instr::F32Ge => bin!(|a: f32, b: f32| a >= b),
                Instr::F32Abs => un!(f32_abs),
                Instr::F32Neg => un!(f32_neg),
                Instr::F32Ceil => un!(|a: f32| round_f32(a, libm::ceilf)),
                Instr::F32Floor => un!(|a: f32| round_f32(a, libm::floorf)),
                Instr::F32Trunc => un!(|a: f32| round_f32(a, libm::truncf)),
                Instr::F32Nearest => un!(|a: f32| round_f32(a, libm::rintf)),
                Instr::F32Sqrt => un!(f32_sqrt),
                Instr::F32Add => bin!(|a: f32, b: f32| a + b),
                Instr::F32Sub => bin!(|a: f32, b: f32| a - b),
                Instr::F32Mul => bin!(|a: f32, b: f32| a * b),
                Instr::F32Div => bin!(|a: f32, b: f32| a / b),
                Instr::F32Min => bin!(f32_min),
                Instr::F32Max => bin!(f32_max),
                Instr::F32Copysign => bin!(f32_copysign),

                // ---- f64 ----------------------------------------------
                Instr::F64Eq => bin!(|a: f64, b: f64| a == b),
                Instr::F64Ne => bin!(|a: f64, b: f64| a != b),
                Instr::F64Lt => bin!(|a: f64, b: f64| a < b),
                Instr::F64Gt => bin!(|a: f64, b: f64| a > b),
                Instr::F64Le => bin!(|a: f64, b: f64| a <= b),
                Instr::F64Ge => bin!(|a: f64, b: f64| a >= b),
                Instr::F64Abs => un!(f64_abs),
                Instr::F64Neg => un!(f64_neg),
                Instr::F64Ceil => un!(|a: f64| round_f64(a, libm::ceil)),
                Instr::F64Floor => un!(|a: f64| round_f64(a, libm::floor)),
                Instr::F64Trunc => un!(|a: f64| round_f64(a, libm::trunc)),
                Instr::F64Nearest => un!(|a: f64| round_f64(a, libm::rint)),
                Instr::F64Sqrt => un!(f64_sqrt),
                Instr::F64Add => bin!(|a: f64, b: f64| a + b),
                Instr::F64Sub => bin!(|a: f64, b: f64| a - b),
                Instr::F64Mul => bin!(|a: f64, b: f64| a * b),
                Instr::F64Div => bin!(|a: f64, b: f64| a / b),
                Instr::F64Min => bin!(f64_min),
                Instr::F64Max => bin!(f64_max),
                Instr::F64Copysign => bin!(f64_copysign),

                // ---- conversions --------------------------------------
                Instr::I32WrapI64 => un!(|a: i64| a as i32),
                Instr::I32TruncF32S => un_try!(i32_trunc_f32_s),
                Instr::I32TruncF32U => un_try!(|a: f32| i32_trunc_f32_u(a).map(|v| v as i32)),
                Instr::I32TruncF64S => un_try!(i32_trunc_f64_s),
                Instr::I32TruncF64U => un_try!(|a: f64| i32_trunc_f64_u(a).map(|v| v as i32)),
                Instr::I64ExtendI32S => un!(|a: i32| a as i64),
                Instr::I64ExtendI32U => un!(|a: i32| a as u32 as i64),
                Instr::I64TruncF32S => un_try!(i64_trunc_f32_s),
                Instr::I64TruncF32U => un_try!(|a: f32| i64_trunc_f32_u(a).map(|v| v as i64)),
                Instr::I64TruncF64S => un_try!(i64_trunc_f64_s),
                Instr::I64TruncF64U => un_try!(|a: f64| i64_trunc_f64_u(a).map(|v| v as i64)),
                Instr::F32ConvertI32S => un!(|a: i32| a as f32),
                Instr::F32ConvertI32U => un!(|a: i32| a as u32 as f32),
                Instr::F32ConvertI64S => un!(|a: i64| a as f32),
                Instr::F32ConvertI64U => un!(|a: i64| a as u64 as f32),
                Instr::F32DemoteF64 => un!(f32_demote),
                Instr::F64ConvertI32S => un!(|a: i32| a as f64),
                Instr::F64ConvertI32U => un!(|a: i32| a as u32 as f64),
                Instr::F64ConvertI64S => un!(|a: i64| a as f64),
                Instr::F64ConvertI64U => un!(|a: i64| a as u64 as f64),
                Instr::F64PromoteF32 => un!(f64_promote),
                Instr::I32ReinterpretF32 => un!(|a: f32| a.to_bits() as i32),
                Instr::I64ReinterpretF64 => un!(|a: f64| a.to_bits() as i64),
                Instr::F32ReinterpretI32 => un!(|a: i32| f32::from_bits(a as u32)),
                Instr::F64ReinterpretI64 => un!(|a: i64| f64::from_bits(a as u64)),

                // ---- sign extension -----------------------------------
                Instr::I32Extend8S => un!(|a: i32| a as i8 as i32),
                Instr::I32Extend16S => un!(|a: i32| a as i16 as i32),
                Instr::I64Extend8S => un!(|a: i64| a as i8 as i64),
                Instr::I64Extend16S => un!(|a: i64| a as i16 as i64),
                Instr::I64Extend32S => un!(|a: i64| a as i32 as i64),

                // ---- saturating truncation (Rust `as` semantics) -------
                Instr::I32TruncSatF32S => un!(|a: f32| a as i32),
                Instr::I32TruncSatF32U => un!(|a: f32| a as u32 as i32),
                Instr::I32TruncSatF64S => un!(|a: f64| a as i32),
                Instr::I32TruncSatF64U => un!(|a: f64| a as u32 as i32),
                Instr::I64TruncSatF32S => un!(|a: f32| a as i64),
                Instr::I64TruncSatF32U => un!(|a: f32| a as u64 as i64),
                Instr::I64TruncSatF64S => un!(|a: f64| a as i64),
                Instr::I64TruncSatF64U => un!(|a: f64| a as u64 as i64),
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
                .try_borrow_mut()
                .map_err(|_| Trap::Host)?
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
        // The functions-only front end does not resolve other imports: a
        // link error, like any import the resolver does not know.
        for imp in ["(table 1 funcref)", "(memory 1)", "(global i32)"] {
            let r = instantiate_with(
                &alloc::format!(r#"(module (import "a" "b" {imp}))"#),
                &Limits::default(),
            );
            assert_eq!(r.err(), Some(Error::Link { import: 0 }), "{imp}");
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
