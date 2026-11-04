//! The interpreter's instruction encoding and the pass that produces it.
//!
//! The decoder produces [`FuncBody`]s of [`Instr`]s: a 16-byte `enum` whose
//! niche-packed tag costs a remap in every dispatch and whose fields must
//! be re-derived at run time (block arities from type indices, label
//! targets from a label stack). At instantiation the whole module is
//! compiled into a [`Program`] that the interpreter loop in
//! [`crate::exec`] runs instead: one `u64` per instruction with the opcode
//! in the low byte and the immediates packed above it, a few instructions
//! taking a second word for a wide immediate, the bodies of all functions
//! in one array, and every branch resolved. Compilation is total: every
//! [`Instr`] has an encoding, an immediate that does not fit the packed
//! fields gets a wide form, and an instruction the module cannot give a
//! meaning to (a block type index out of range) becomes
//! [`op::TRAP_INVALID`], so an unvalidated body compiles and traps where
//! it would have before (the fuzz test runs such bodies).
//!
//! # Word layout
//!
//! ```text
//! bits  0..8   opcode (`op::*`, numbered densely so the dispatch needs no range check)
//! bits  8..16  `c`: an 8-bit immediate
//! bits 16..32  `b`: a 16-bit immediate
//! bits 32..64  `a`: a 32-bit immediate
//! ```
//!
//! | Instruction | `a` | `b` | `c` | extra words |
//! | --- | --- | --- | --- | --- |
//! | `block`, `loop`, `end` | – | – | – | (no effect at run time, see below) |
//! | `if` | pc of the else arm, or past the `end` | – | – | |
//! | `else` | pc past the `end` | – | – | |
//! | `br`, `br_if` | target pc | unwind height | arity | `BR_W`/`BR_IF_W`: `height | arity << 32` instead |
//! | `br_table` | index into [`Program::br_tables`] | – | – | |
//! | `call` | function index | – | – | |
//! | `call_indirect` | type index | table index | – | `CALL_INDIRECT_W`: table index |
//! | `local.*`, `global.*`, `ref.func`, `table.*`, `elem.drop`, `memory.init`, `data.drop` | index | – | – | `table.init`/`table.copy`: second index |
//! | `ref.null` | 0 funcref, 1 externref | – | – | |
//! | loads, stores | offset | – | – | |
//! | `i32.const`, `f32.const` | bits | – | – | |
//! | `i64.const`, `f64.const` | – | – | – | bits |
//!
//! # Static control flow
//!
//! The operand stack height before every instruction of a valid body is
//! fixed, so a [`Tracker`] (the validator's control-stack algorithm without
//! the types) resolves each `br`/`br_if`/`br_table` at compile time to a
//! [`Branch`]: the pc to continue at, the height (relative to the frame
//! base, locals included) to unwind the stack to, and the number of values
//! carried. A branch to the function's own label, and the final `end`,
//! compile to `return` (`br_if` to [`op::BR_IF_RETURN`]). `block`, `loop`
//! and `end` therefore do nothing at run time beyond costing their fuel
//! unit, `if` is a conditional jump and `else` a jump, and the interpreter
//! keeps no label stack. An unvalidated body gets heights that are
//! clamped at the enclosing block's base; the branches it produces are
//! bounds-checked when run and trap `Invalid` at worst. The tracker also
//! yields the static maximum of the operand stack, which with the locals
//! gives the frame's extent ([`Func::stack`]) that a call reserves at
//! once.
//!
//! The bodies of all defined functions are concatenated into one array
//! ([`Program::words`]) and pcs index into it, so the interpreter loop
//! runs on one code slice for the whole execution and a call is a jump to
//! the callee's [`Func::entry`]. A target outside the body (only an
//! unvalidated body has one) is [`NOWHERE`], where the fetch traps.
//!
//! # Superinstructions
//!
//! A peephole pass then rewrites the *opcode* of the first word of common
//! sequences to a fused opcode whose handler performs the whole sequence
//! in one dispatch. The sequences were chosen with the `pairs` example
//! (a static n-gram histogram) over Rust-compiled modules: `local.get
//! local.get`, `local.get i32.const`, `i32.const i32.const`; `local.get
//! i32.const <op>` and `i32.const <op>` for the common `i32` operators
//! (`add`, `sub`, `and`, `or`, `shl`, `shr_u`, comparisons), `local.get
//! local.get <op>` for a few; `... i32.add local.set` (the `x += k` idiom);
//! `local.get i32.load`, `local.get {local.get,i32.const} i32.store`,
//! `i32.const local.set`, `i32.add local.set`; and an `i32` comparison
//! followed by `br_if`. The words of the other instructions in the
//! sequence stay in place unchanged, and the fused handler reads their
//! immediates from them. That keeps the encoding trivially correct:
//!
//! - a branch into the middle of a sequence lands on a plain instruction
//!   and runs the tail as before (no jump-target analysis is needed, and
//!   sequences may overlap);
//! - fuel stays exact: a fused handler charges one unit per instruction it
//!   covers, and when the budget cannot pay for all of them it *decays* to
//!   the plain first instruction, after which the rest run one by one;
//! - traps inside a sequence end the execution, so the intermediate stack
//!   state of a fused handler never matters.
//!
//! No sequence contains a `call` (a host call may block mid-sequence) or a
//! structured control instruction; `br_if` may only end one, and only in
//! its plain one-word form.

use crate::instr::{BlockType, FuncBody, Instr};
use crate::module::Module;
use crate::types::{RefType, ValType};
use alloc::vec::Vec;

/// One encoded instruction (see the module documentation).
pub(crate) type Word = u64;

/// Opcodes. Numbers are explicit so that the interpreter can match on them
/// as constants; `op::NAMES` lists them for tests and disassembly.
pub(crate) mod op {
    /// Opcodes are numbered densely in declaration order, so that the
    /// interpreter's `match` covers `0..COUNT` and needs no range check
    /// (the unused `COUNT..=u8::MAX` trap); `NAMES` lists them for
    /// tests and disassembly.
    macro_rules! ops {
        ($($(#[$doc:meta])* $name:ident,)*) => {
            #[allow(non_camel_case_types, dead_code, clippy::upper_case_acronyms)]
            #[repr(u8)]
            enum Numbering { $($name,)* }
            $($(#[$doc])* pub const $name: u8 = Numbering::$name as u8;)*
            /// The number of opcodes (at most 255, so that `COUNT..=u8::MAX`
            /// is a non-empty range of unused values).
            pub const COUNT: u8 = {
                const N: usize = [$(Numbering::$name as u8,)*].len();
                assert!(N <= 255, "too many opcodes");
                N as u8
            };
            /// Every opcode with its name.
            #[cfg(test)]
            pub const NAMES: &[(&str, u8)] = &[$((stringify!($name), $name),)*];
        };
    }
    ops! {
        // ---- control --------------------------------------------------
        UNREACHABLE,
        NOP,
        BLOCK,
        LOOP,
        IF,
        ELSE,
        END,
        BR,
        BR_W,
        BR_IF,
        BR_IF_W,
        /// `br_if` out of the function.
        BR_IF_RETURN,
        BR_TABLE,
        RETURN,
        CALL,
        CALL_INDIRECT,
        CALL_INDIRECT_W,
        /// An instruction the module gives no meaning to: traps `Invalid`.
        TRAP_INVALID,
        // ---- parametric / variables ------------------------------------
        DROP,
        SELECT,
        LOCAL_GET,
        LOCAL_SET,
        LOCAL_TEE,
        GLOBAL_GET,
        GLOBAL_SET,
        // ---- references / tables ---------------------------------------
        REF_NULL,
        REF_IS_NULL,
        REF_FUNC,
        TABLE_GET,
        TABLE_SET,
        TABLE_INIT,
        ELEM_DROP,
        TABLE_COPY,
        TABLE_GROW,
        TABLE_SIZE,
        TABLE_FILL,
        // ---- memory ----------------------------------------------------
        I32_LOAD,
        I64_LOAD,
        F32_LOAD,
        F64_LOAD,
        I32_LOAD8_S,
        I32_LOAD8_U,
        I32_LOAD16_S,
        I32_LOAD16_U,
        I64_LOAD8_S,
        I64_LOAD8_U,
        I64_LOAD16_S,
        I64_LOAD16_U,
        I64_LOAD32_S,
        I64_LOAD32_U,
        I32_STORE,
        I64_STORE,
        F32_STORE,
        F64_STORE,
        I32_STORE8,
        I32_STORE16,
        I64_STORE8,
        I64_STORE16,
        I64_STORE32,
        MEMORY_SIZE,
        MEMORY_GROW,
        MEMORY_INIT,
        DATA_DROP,
        MEMORY_COPY,
        MEMORY_FILL,
        // ---- constants -------------------------------------------------
        I32_CONST,
        I64_CONST,
        F32_CONST,
        F64_CONST,
        // ---- i32 -------------------------------------------------------
        I32_EQZ,
        I32_EQ,
        I32_NE,
        I32_LT_S,
        I32_LT_U,
        I32_GT_S,
        I32_GT_U,
        I32_LE_S,
        I32_LE_U,
        I32_GE_S,
        I32_GE_U,
        I32_CLZ,
        I32_CTZ,
        I32_POPCNT,
        I32_ADD,
        I32_SUB,
        I32_MUL,
        I32_DIV_S,
        I32_DIV_U,
        I32_REM_S,
        I32_REM_U,
        I32_AND,
        I32_OR,
        I32_XOR,
        I32_SHL,
        I32_SHR_S,
        I32_SHR_U,
        I32_ROTL,
        I32_ROTR,
        // ---- i64 -------------------------------------------------------
        I64_EQZ,
        I64_EQ,
        I64_NE,
        I64_LT_S,
        I64_LT_U,
        I64_GT_S,
        I64_GT_U,
        I64_LE_S,
        I64_LE_U,
        I64_GE_S,
        I64_GE_U,
        I64_CLZ,
        I64_CTZ,
        I64_POPCNT,
        I64_ADD,
        I64_SUB,
        I64_MUL,
        I64_DIV_S,
        I64_DIV_U,
        I64_REM_S,
        I64_REM_U,
        I64_AND,
        I64_OR,
        I64_XOR,
        I64_SHL,
        I64_SHR_S,
        I64_SHR_U,
        I64_ROTL,
        I64_ROTR,
        // ---- f32 -------------------------------------------------------
        F32_EQ,
        F32_NE,
        F32_LT,
        F32_GT,
        F32_LE,
        F32_GE,
        F32_ABS,
        F32_NEG,
        F32_CEIL,
        F32_FLOOR,
        F32_TRUNC,
        F32_NEAREST,
        F32_SQRT,
        F32_ADD,
        F32_SUB,
        F32_MUL,
        F32_DIV,
        F32_MIN,
        F32_MAX,
        F32_COPYSIGN,
        // ---- f64 -------------------------------------------------------
        F64_EQ,
        F64_NE,
        F64_LT,
        F64_GT,
        F64_LE,
        F64_GE,
        F64_ABS,
        F64_NEG,
        F64_CEIL,
        F64_FLOOR,
        F64_TRUNC,
        F64_NEAREST,
        F64_SQRT,
        F64_ADD,
        F64_SUB,
        F64_MUL,
        F64_DIV,
        F64_MIN,
        F64_MAX,
        F64_COPYSIGN,
        // ---- conversions -----------------------------------------------
        I32_WRAP_I64,
        I32_TRUNC_F32_S,
        I32_TRUNC_F32_U,
        I32_TRUNC_F64_S,
        I32_TRUNC_F64_U,
        I64_EXTEND_I32_S,
        I64_EXTEND_I32_U,
        I64_TRUNC_F32_S,
        I64_TRUNC_F32_U,
        I64_TRUNC_F64_S,
        I64_TRUNC_F64_U,
        F32_CONVERT_I32_S,
        F32_CONVERT_I32_U,
        F32_CONVERT_I64_S,
        F32_CONVERT_I64_U,
        F32_DEMOTE_F64,
        F64_CONVERT_I32_S,
        F64_CONVERT_I32_U,
        F64_CONVERT_I64_S,
        F64_CONVERT_I64_U,
        F64_PROMOTE_F32,
        I32_REINTERPRET_F32,
        I64_REINTERPRET_F64,
        F32_REINTERPRET_I32,
        F64_REINTERPRET_I64,
        I32_EXTEND8_S,
        I32_EXTEND16_S,
        I64_EXTEND8_S,
        I64_EXTEND16_S,
        I64_EXTEND32_S,
        I32_TRUNC_SAT_F32_S,
        I32_TRUNC_SAT_F32_U,
        I32_TRUNC_SAT_F64_S,
        I32_TRUNC_SAT_F64_U,
        I64_TRUNC_SAT_F32_S,
        I64_TRUNC_SAT_F32_U,
        I64_TRUNC_SAT_F64_S,
        I64_TRUNC_SAT_F64_U,
        // ---- superinstructions (see the module documentation) ----------
        /// `local.get a; local.get b`
        LOCAL_GET_LOCAL_GET,
        /// `local.get a; i32.const c`
        LOCAL_GET_I32_CONST,
        /// `i32.const a; i32.const b`
        I32_CONST_I32_CONST,
        /// `local.get a; local.get b; i32.add`
        LOCAL_GET_LOCAL_GET_I32_ADD,
        /// `local.get a; i32.const c; i32.add`
        LOCAL_GET_I32_CONST_I32_ADD,
        /// `local.get a; local.get b; i32.add; local.set d`
        LOCAL_GET_LOCAL_GET_I32_ADD_LOCAL_SET,
        /// `local.get a; i32.const c; i32.add; local.set d`
        LOCAL_GET_I32_CONST_I32_ADD_LOCAL_SET,
        /// `i32.const c; i32.add`
        I32_CONST_I32_ADD,
        /// `i32.const c; local.set a`
        I32_CONST_LOCAL_SET,
        /// `i32.add; local.set a`
        I32_ADD_LOCAL_SET,
        /// `local.get a; i32.load offset`
        LOCAL_GET_I32_LOAD,
        /// `local.get a; local.get b; i32.store offset`
        LOCAL_GET_LOCAL_GET_I32_STORE,
        /// `local.get a; i32.const c; i32.store offset`
        LOCAL_GET_I32_CONST_I32_STORE,
        /// `i32.eqz; br_if depth`
        I32_EQZ_BR_IF,
        I32_EQ_BR_IF,
        I32_NE_BR_IF,
        I32_LT_S_BR_IF,
        I32_LT_U_BR_IF,
        I32_GT_S_BR_IF,
        I32_GT_U_BR_IF,
        I32_LE_S_BR_IF,
        I32_LE_U_BR_IF,
        I32_GE_S_BR_IF,
        I32_GE_U_BR_IF,
        /// `local.get a; i32.const c; <op>` for the operators below.
        LOCAL_GET_I32_CONST_I32_SUB,
        LOCAL_GET_I32_CONST_I32_AND,
        LOCAL_GET_I32_CONST_I32_OR,
        LOCAL_GET_I32_CONST_I32_SHL,
        LOCAL_GET_I32_CONST_I32_SHR_U,
        LOCAL_GET_I32_CONST_I32_LT_U,
        LOCAL_GET_I32_CONST_I32_GT_U,
        LOCAL_GET_I32_CONST_I32_LT_S,
        LOCAL_GET_I32_CONST_I32_GT_S,
        LOCAL_GET_I32_CONST_I32_EQ,
        LOCAL_GET_I32_CONST_I32_NE,
        /// `i32.const c; <op>` for the operators below.
        I32_CONST_I32_SUB,
        I32_CONST_I32_AND,
        I32_CONST_I32_OR,
        I32_CONST_I32_SHL,
        I32_CONST_I32_SHR_U,
        I32_CONST_I32_LT_U,
        I32_CONST_I32_GT_U,
        I32_CONST_I32_LT_S,
        I32_CONST_I32_GT_S,
        I32_CONST_I32_EQ,
        I32_CONST_I32_NE,
        /// `local.get a; local.get b; <op>` for the operators below.
        LOCAL_GET_LOCAL_GET_I32_SUB,
        LOCAL_GET_LOCAL_GET_I32_LT_U,
        LOCAL_GET_LOCAL_GET_I32_GT_U,
        LOCAL_GET_LOCAL_GET_I32_EQ,
    }
}

/// The opcode of a word.
#[inline(always)]
pub(crate) fn opcode(w: Word) -> u8 {
    w as u8
}

/// The 32-bit immediate of a word.
#[inline(always)]
pub(crate) fn imm_a(w: Word) -> u32 {
    (w >> 32) as u32
}

/// The 16-bit immediate of a word.
#[inline(always)]
pub(crate) fn imm_b(w: Word) -> u32 {
    (w >> 16) as u16 as u32
}

/// The 8-bit immediate of a word.
#[inline(always)]
pub(crate) fn imm_c(w: Word) -> u32 {
    (w >> 8) as u8 as u32
}

#[inline(always)]
fn word(op: u8, b: u16, a: u32) -> Word {
    op as Word | (b as Word) << 16 | (a as Word) << 32
}

/// The compiled code of a whole module: every defined function's words
/// in one array, so that the interpreter loop's code slice never changes
/// and a call is a jump to the callee's entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Program {
    /// The instruction words of all defined functions, in order.
    pub(crate) words: Vec<Word>,
    /// Every function (function index space, imports first).
    pub(crate) funcs: Vec<Func>,
    /// The declared locals of all defined functions, concatenated
    /// ([`Func::locals_start`]`..`[`Func::locals_end`] index into it).
    pub(crate) locals: Vec<ValType>,
    /// The resolved `br_table`s of all functions; the `a` of
    /// [`op::BR_TABLE`] indexes into it.
    pub(crate) br_tables: Vec<BranchTable>,
}

/// What the interpreter needs to call a function.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Func {
    /// Index of the first word in [`Program::words`], or [`IMPORTED`] for
    /// an imported function (called through the host).
    pub(crate) entry: u32,
    /// Number of parameters.
    pub(crate) nparams: u32,
    /// Number of results.
    pub(crate) nresults: u32,
    /// The declared locals: `locals_start..locals_end` in
    /// [`Program::locals`].
    pub(crate) locals_start: u32,
    pub(crate) locals_end: u32,
    /// The frame's extent on the value stack, from its base (the first
    /// parameter) to the static maximum of its operand stack: parameters,
    /// declared locals and operands.
    pub(crate) stack: u32,
}

/// The [`Func::entry`] of an imported function.
pub(crate) const IMPORTED: u32 = u32::MAX;

/// The target of a jump outside the compiled code: fetching it traps
/// (distinct from [`RETURN`]).
const NOWHERE: u32 = u32::MAX - 1;

/// `(params, results)` of a block type, `None` for a type index the module
/// does not have.
fn arity_of(module: &Module, ty: BlockType) -> Option<(u32, u32)> {
    match ty {
        BlockType::Empty => Some((0, 0)),
        BlockType::Val(_) => Some((0, 1)),
        BlockType::Type(idx) => {
            let t = module.types.get(idx as usize)?;
            Some((t.params.len() as u32, t.results.len() as u32))
        }
    }
}

/// Compile every defined function of `module`. `arity[f]` is the
/// parameter and result count of function `f` (function index space),
/// `num_imports` the number of imported functions. Never fails (see the
/// module documentation).
pub(crate) fn compile_module(module: &Module, arity: &[(u32, u32)], num_imports: usize) -> Program {
    let mut program = Program {
        words: Vec::new(),
        funcs: Vec::with_capacity(arity.len()),
        locals: Vec::new(),
        br_tables: Vec::new(),
    };
    for &(np, nr) in arity.iter().take(num_imports) {
        program.funcs.push(Func {
            entry: IMPORTED,
            nparams: np,
            nresults: nr,
            locals_start: 0,
            locals_end: 0,
            stack: 0,
        });
    }
    for (body, &(np, nr)) in module.code.iter().zip(arity.iter().skip(num_imports)) {
        let entry = u32::try_from(program.words.len()).unwrap_or(NOWHERE);
        let locals_start = u32::try_from(program.locals.len()).unwrap_or(0);
        program.locals.extend_from_slice(&body.locals);
        let locals_end = u32::try_from(program.locals.len()).unwrap_or(0);
        let max_height = compile_body(
            module,
            arity,
            body,
            np,
            nr,
            entry,
            &mut program.words,
            &mut program.br_tables,
        );
        program.funcs.push(Func {
            entry,
            nparams: np,
            nresults: nr,
            locals_start,
            locals_end,
            stack: np
                .saturating_add(body.locals.len() as u32)
                .saturating_add(max_height),
        });
    }
    program
}

/// The target of a resolved branch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Branch {
    /// Absolute pc to continue at, or [`RETURN`] to leave the function.
    pub(crate) target: u32,
    /// Stack height (relative to the frame base, locals included) to
    /// unwind to before the carried values.
    pub(crate) height: u32,
    /// Number of values the branch carries.
    pub(crate) arity: u32,
}

/// The [`Branch::target`] of a branch out of the function.
pub(crate) const RETURN: u32 = u32::MAX;

/// A resolved `br_table`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BranchTable {
    pub(crate) targets: Vec<Branch>,
    pub(crate) default: Branch,
}

/// A structured control frame of the height tracker.
#[derive(Clone, Copy)]
struct Ctrl {
    /// Operand stack height at entry (after the params were popped).
    height: u32,
    nparams: u32,
    nresults: u32,
    /// The instruction index whose successor a branch continues at: the
    /// `loop` itself (its body starts after it) or the block's `end`.
    cont_after: u32,
    is_loop: bool,
    /// The rest of the block is unreachable (after `br`, `return`, ...).
    unreachable: bool,
}

/// What the first pass resolves about a control instruction, by
/// instruction index: a branch's target or the `end`'s meaning.
#[derive(Clone, Copy, Default)]
struct Resolved {
    /// `br`/`br_if`: the target as an instruction index whose successor is
    /// the continuation (`RETURN` for the function), with height and arity.
    branch: Branch,
    /// `end`: closes the function (compiles to `return`).
    function_end: bool,
    /// Words this instruction takes.
    width: u32,
}

/// Static stack effects: the operand stack height before and after each
/// instruction of a valid function body is fixed, so every branch can be
/// resolved to the height it unwinds to. The tracker is the validator's
/// algorithm without the types: a control stack of frames, and a height
/// that pops past a frame's base are ignored once the frame is
/// unreachable (dead code after a `br`) and clamped otherwise (only an
/// invalid body does that; the code it produces then traps or runs wrong,
/// never unsafely).
struct Tracker<'m> {
    module: &'m Module,
    arity: &'m [(u32, u32)],
    /// Operand stack height, relative to the first operand slot.
    height: u32,
    /// The largest `height` seen.
    max_height: u32,
    ctrls: Vec<Ctrl>,
}

impl Tracker<'_> {
    fn top(&self) -> Ctrl {
        self.ctrls.last().copied().unwrap_or(Ctrl {
            height: 0,
            nparams: 0,
            nresults: 0,
            cont_after: RETURN,
            is_loop: false,
            unreachable: true,
        })
    }

    fn pop(&mut self, n: u32) {
        let floor = self.top().height;
        self.height = self.height.saturating_sub(n).max(floor);
    }

    fn push(&mut self, n: u32) {
        self.height = self.height.saturating_add(n);
        self.max_height = self.max_height.max(self.height);
    }

    /// The function's own type: what `return` and a branch out of all
    /// blocks carry.
    fn enter(&mut self, nresults: u32) {
        self.ctrls.push(Ctrl {
            height: 0,
            nparams: 0,
            nresults,
            cont_after: RETURN,
            is_loop: false,
            unreachable: false,
        });
    }

    fn push_ctrl(&mut self, nparams: u32, nresults: u32, cont_after: u32, is_loop: bool) {
        self.pop(nparams);
        self.ctrls.push(Ctrl {
            height: self.height,
            nparams,
            nresults,
            cont_after,
            is_loop,
            unreachable: false,
        });
        self.push(nparams);
    }

    fn unreachable(&mut self) {
        if let Some(c) = self.ctrls.last_mut() {
            self.height = c.height;
            c.unreachable = true;
        }
    }

    /// The branch `depth` labels out, relative to the frame's locals
    /// (`nlocals` slots below the first operand).
    fn label(&self, depth: u32, nlocals: u32) -> Branch {
        // The outermost frame is the function itself: a branch to it
        // returns.
        let idx = self.ctrls.len().checked_sub(1 + depth as usize);
        match idx.and_then(|i| self.ctrls.get(i)) {
            Some(c) if c.cont_after != RETURN => Branch {
                target: c.cont_after,
                height: nlocals.saturating_add(c.height),
                arity: if c.is_loop { c.nparams } else { c.nresults },
            },
            _ => Branch {
                target: RETURN,
                height: 0,
                arity: 0,
            },
        }
    }

    /// Apply the stack effect of a non-control instruction.
    fn effect(&mut self, instr: &Instr) {
        use Instr::*;
        let (pops, pushes) = match *instr {
            Unreachable
            | Nop
            | Block { .. }
            | Loop { .. }
            | If { .. }
            | Else { .. }
            | End
            | Br(_)
            | BrIf(_)
            | BrTable(_)
            | Return => (0, 0),
            Call(f) => self.arity.get(f as usize).copied().unwrap_or((0, 0)),
            CallIndirect { ty, .. } => {
                let (np, nr) = self
                    .module
                    .types
                    .get(ty as usize)
                    .map_or((0, 0), |t| (t.params.len() as u32, t.results.len() as u32));
                (np.saturating_add(1), nr)
            }
            Drop => (1, 0),
            Select | SelectT(_) => (3, 1),
            LocalGet(_) | GlobalGet(_) | RefNull(_) | RefFunc(_) | TableSize(_) | MemorySize
            | I32Const(_) | I64Const(_) | F32Const(_) | F64Const(_) => (0, 1),
            LocalSet(_) | GlobalSet(_) => (1, 0),
            LocalTee(_) | RefIsNull | TableGet(_) | MemoryGrow => (1, 1),
            TableSet(_) => (2, 0),
            TableGrow(_) => (2, 1),
            TableInit { .. }
            | TableCopy { .. }
            | TableFill(_)
            | MemoryInit(_)
            | MemoryCopy
            | MemoryFill => (3, 0),
            ElemDrop(_) | DataDrop(_) => (0, 0),
            I32Load(_) | I64Load(_) | F32Load(_) | F64Load(_) | I32Load8S(_) | I32Load8U(_)
            | I32Load16S(_) | I32Load16U(_) | I64Load8S(_) | I64Load8U(_) | I64Load16S(_)
            | I64Load16U(_) | I64Load32S(_) | I64Load32U(_) => (1, 1),
            I32Store(_) | I64Store(_) | F32Store(_) | F64Store(_) | I32Store8(_)
            | I32Store16(_) | I64Store8(_) | I64Store16(_) | I64Store32(_) => (2, 0),
            // Binary operators and comparisons.
            I32Eq | I32Ne | I32LtS | I32LtU | I32GtS | I32GtU | I32LeS | I32LeU | I32GeS
            | I32GeU | I32Add | I32Sub | I32Mul | I32DivS | I32DivU | I32RemS | I32RemU
            | I32And | I32Or | I32Xor | I32Shl | I32ShrS | I32ShrU | I32Rotl | I32Rotr | I64Eq
            | I64Ne | I64LtS | I64LtU | I64GtS | I64GtU | I64LeS | I64LeU | I64GeS | I64GeU
            | I64Add | I64Sub | I64Mul | I64DivS | I64DivU | I64RemS | I64RemU | I64And | I64Or
            | I64Xor | I64Shl | I64ShrS | I64ShrU | I64Rotl | I64Rotr | F32Eq | F32Ne | F32Lt
            | F32Gt | F32Le | F32Ge | F32Add | F32Sub | F32Mul | F32Div | F32Min | F32Max
            | F32Copysign | F64Eq | F64Ne | F64Lt | F64Gt | F64Le | F64Ge | F64Add | F64Sub
            | F64Mul | F64Div | F64Min | F64Max | F64Copysign => (2, 1),
            // Unary operators, tests and conversions.
            _ => (1, 1),
        };
        self.pop(pops);
        self.push(pushes);
    }
}

/// Append the code of `body` to `words`, whose next index is `entry`;
/// `(nparams, nresults)` is the function's type, `arity` the types of all
/// functions (for calls), and resolved `br_table`s go to `tables`. Returns
/// the static maximum height of the operand stack.
#[allow(clippy::too_many_arguments)]
fn compile_body(
    module: &Module,
    arity: &[(u32, u32)],
    body: &FuncBody,
    nparams: u32,
    nresults: u32,
    entry: u32,
    words: &mut Vec<Word>,
    tables: &mut Vec<BranchTable>,
) -> u32 {
    let nlocals = nparams.saturating_add(body.locals.len() as u32);
    let instrs = &body.instrs;

    // Pass 1: track the stack, resolve branches and size every
    // instruction.
    let mut tr = Tracker {
        module,
        arity,
        height: 0,
        max_height: 0,
        ctrls: Vec::new(),
    };
    tr.enter(nresults);
    let mut resolved: Vec<Resolved> = Vec::with_capacity(instrs.len());
    let mut raw_tables: Vec<BranchTable> = Vec::new();
    let branch_width = |b: &Branch| {
        if b.height <= u16::MAX as u32 && b.arity <= u8::MAX as u32 {
            1
        } else {
            2
        }
    };
    for (i, instr) in instrs.iter().enumerate() {
        let idx = i as u32;
        let mut r = Resolved {
            width: 1,
            ..Resolved::default()
        };
        match *instr {
            Instr::Block { ty, end } => {
                let (np, nr) = arity_of(module, ty).unwrap_or((0, 0));
                tr.push_ctrl(np, nr, end, false);
            }
            Instr::Loop { ty, .. } => {
                let (np, nr) = arity_of(module, ty).unwrap_or((0, 0));
                tr.push_ctrl(np, nr, idx, true);
            }
            Instr::If { ty, end, .. } => {
                let (np, nr) = arity_of(module, ty).unwrap_or((0, 0));
                tr.pop(1);
                tr.push_ctrl(np, nr, end, false);
            }
            Instr::Else { .. } => {
                let c = tr.top();
                if let Some(c) = tr.ctrls.last_mut() {
                    c.unreachable = false;
                }
                tr.height = c.height.saturating_add(c.nparams);
            }
            Instr::End => {
                // The function's own frame stays; its `end` returns.
                if tr.ctrls.len() <= 1 {
                    r.function_end = true;
                } else {
                    let c = tr.top();
                    tr.ctrls.pop();
                    tr.height = c.height.saturating_add(c.nresults);
                }
            }
            Instr::Br(depth) => {
                r.branch = tr.label(depth, nlocals);
                r.width = branch_width(&r.branch);
                tr.unreachable();
            }
            Instr::BrIf(depth) => {
                tr.pop(1);
                r.branch = tr.label(depth, nlocals);
                r.width = branch_width(&r.branch);
            }
            Instr::BrTable(t) => {
                tr.pop(1);
                let (targets, default) = match body.br_tables.get(t as usize) {
                    Some(bt) => (
                        bt.targets.iter().map(|&d| tr.label(d, nlocals)).collect(),
                        tr.label(bt.default, nlocals),
                    ),
                    // No such table: a branch nowhere, which traps.
                    None => (
                        Vec::new(),
                        Branch {
                            target: NOWHERE,
                            height: 0,
                            arity: 0,
                        },
                    ),
                };
                raw_tables.push(BranchTable { targets, default });
                tr.unreachable();
            }
            Instr::Return | Instr::Unreachable => tr.unreachable(),
            Instr::I64Const(_) | Instr::F64Const(_) => {
                r.width = 2;
                tr.effect(instr);
            }
            Instr::CallIndirect { table, .. } => {
                r.width = if table > u16::MAX as u32 { 2 } else { 1 };
                tr.effect(instr);
            }
            Instr::TableInit { .. } | Instr::TableCopy { .. } => {
                r.width = 2;
                tr.effect(instr);
            }
            _ => tr.effect(instr),
        }
        resolved.push(r);
    }

    // The first word of every instruction, relative to `entry`.
    let mut pc_map: Vec<u32> = Vec::with_capacity(instrs.len() + 1);
    let mut total: u32 = 0;
    for r in &resolved {
        pc_map.push(total);
        total = total.saturating_add(r.width);
    }
    pc_map.push(total);
    // The absolute pc of the instruction after `idx`; outside the body
    // (unvalidated body) it is `NOWHERE`, where the fetch traps, and so is
    // the function's own end: nothing may continue past the final `end`.
    let after = |idx: u32| -> u32 {
        match idx.checked_add(1).and_then(|i| pc_map.get(i as usize)) {
            Some(&rel) if rel < total => entry.saturating_add(rel),
            _ => NOWHERE,
        }
    };
    let absolute = |b: Branch| -> Branch {
        Branch {
            target: if b.target == RETURN {
                RETURN
            } else {
                after(b.target)
            },
            ..b
        }
    };
    // This body's `br_table`s, in order of appearance.
    let mut next_table = tables.len();
    for t in raw_tables {
        tables.push(BranchTable {
            targets: t.targets.into_iter().map(absolute).collect(),
            default: absolute(t.default),
        });
    }

    // Pass 2: encode.
    let first = words.len();
    for (i, (instr, r)) in instrs.iter().zip(&resolved).enumerate() {
        match *instr {
            Instr::Block { .. } => words.push(word(op::BLOCK, 0, 0)),
            Instr::Loop { .. } => words.push(word(op::LOOP, 0, 0)),
            Instr::If { else_, end, .. } => {
                // Into the else arm when there is one, past the `end`
                // otherwise.
                let target = if else_ != end {
                    after(else_)
                } else {
                    after(end)
                };
                words.push(word(op::IF, 0, target));
            }
            Instr::Else { end } => words.push(word(op::ELSE, 0, after(end))),
            // The function's `end` returns; so does the last instruction of
            // an unbalanced (invalid) body, so that nothing runs past it.
            Instr::End => words.push(word(
                if r.function_end || i + 1 == instrs.len() {
                    op::RETURN
                } else {
                    op::END
                },
                0,
                0,
            )),
            Instr::Br(_) | Instr::BrIf(_) => {
                let b = absolute(r.branch);
                let conditional = matches!(instr, Instr::BrIf(_));
                if b.target == RETURN {
                    words.push(word(
                        if conditional {
                            op::BR_IF_RETURN
                        } else {
                            op::RETURN
                        },
                        0,
                        0,
                    ));
                } else if r.width == 1 {
                    let o = if conditional { op::BR_IF } else { op::BR };
                    words.push(word(o, b.height as u16, b.target) | (b.arity as Word) << 8);
                } else {
                    let o = if conditional { op::BR_IF_W } else { op::BR_W };
                    words.push(word(o, 0, b.target));
                    words.push(b.height as Word | (b.arity as Word) << 32);
                }
            }
            Instr::BrTable(_) => {
                words.push(word(op::BR_TABLE, 0, next_table as u32));
                next_table += 1;
            }
            _ => encode(instr, words),
        }
    }
    debug_assert_eq!(words.len() - first, total as usize);

    // Pass 3: superinstructions. A compare fuses with the `br_if` after it
    // only in the latter's plain one-word form.
    for (i, &start) in pc_map.iter().enumerate() {
        let Some(fused) = instrs.get(i..).and_then(fuse) else {
            continue;
        };
        let at = first + start as usize;
        if matches!(instrs.get(i + 1), Some(Instr::BrIf(_)))
            && words.get(at + 1).map(|&w| opcode(w)) != Some(op::BR_IF)
        {
            continue;
        }
        if let Some(slot) = words.get_mut(at) {
            *slot = (*slot & !0xff) | fused as Word;
        }
    }
    tr.max_height
}

/// Append the encoding of a non-control instruction to `out`.
fn encode(instr: &Instr, out: &mut Vec<Word>) {
    use Instr::*;
    macro_rules! simple {
        ($op:expr) => {
            out.push(word($op, 0, 0))
        };
    }
    macro_rules! with_a {
        ($op:expr, $a:expr) => {
            out.push(word($op, 0, $a))
        };
    }
    match *instr {
        Unreachable => simple!(op::UNREACHABLE),
        Nop => simple!(op::NOP),
        Block { .. }
        | Loop { .. }
        | If { .. }
        | Else { .. }
        | End
        | Br(_)
        | BrIf(_)
        | BrTable(_) => simple!(op::TRAP_INVALID),
        Return => simple!(op::RETURN),
        Call(f) => with_a!(op::CALL, f),
        CallIndirect { ty, table } => {
            if table <= u16::MAX as u32 {
                out.push(word(op::CALL_INDIRECT, table as u16, ty));
            } else {
                out.push(word(op::CALL_INDIRECT_W, 0, ty));
                out.push(table as Word);
            }
        }

        Drop => simple!(op::DROP),
        Select | SelectT(_) => simple!(op::SELECT),
        LocalGet(i) => with_a!(op::LOCAL_GET, i),
        LocalSet(i) => with_a!(op::LOCAL_SET, i),
        LocalTee(i) => with_a!(op::LOCAL_TEE, i),
        GlobalGet(i) => with_a!(op::GLOBAL_GET, i),
        GlobalSet(i) => with_a!(op::GLOBAL_SET, i),

        RefNull(RefType::FuncRef) => with_a!(op::REF_NULL, 0),
        RefNull(RefType::ExternRef) => with_a!(op::REF_NULL, 1),
        RefIsNull => simple!(op::REF_IS_NULL),
        RefFunc(f) => with_a!(op::REF_FUNC, f),
        TableGet(t) => with_a!(op::TABLE_GET, t),
        TableSet(t) => with_a!(op::TABLE_SET, t),
        TableInit { elem, table } => {
            with_a!(op::TABLE_INIT, elem);
            out.push(table as Word);
        }
        ElemDrop(e) => with_a!(op::ELEM_DROP, e),
        TableCopy { dst, src } => {
            with_a!(op::TABLE_COPY, dst);
            out.push(src as Word);
        }
        TableGrow(t) => with_a!(op::TABLE_GROW, t),
        TableSize(t) => with_a!(op::TABLE_SIZE, t),
        TableFill(t) => with_a!(op::TABLE_FILL, t),

        I32Load(m) => with_a!(op::I32_LOAD, m.offset),
        I64Load(m) => with_a!(op::I64_LOAD, m.offset),
        F32Load(m) => with_a!(op::F32_LOAD, m.offset),
        F64Load(m) => with_a!(op::F64_LOAD, m.offset),
        I32Load8S(m) => with_a!(op::I32_LOAD8_S, m.offset),
        I32Load8U(m) => with_a!(op::I32_LOAD8_U, m.offset),
        I32Load16S(m) => with_a!(op::I32_LOAD16_S, m.offset),
        I32Load16U(m) => with_a!(op::I32_LOAD16_U, m.offset),
        I64Load8S(m) => with_a!(op::I64_LOAD8_S, m.offset),
        I64Load8U(m) => with_a!(op::I64_LOAD8_U, m.offset),
        I64Load16S(m) => with_a!(op::I64_LOAD16_S, m.offset),
        I64Load16U(m) => with_a!(op::I64_LOAD16_U, m.offset),
        I64Load32S(m) => with_a!(op::I64_LOAD32_S, m.offset),
        I64Load32U(m) => with_a!(op::I64_LOAD32_U, m.offset),
        I32Store(m) => with_a!(op::I32_STORE, m.offset),
        I64Store(m) => with_a!(op::I64_STORE, m.offset),
        F32Store(m) => with_a!(op::F32_STORE, m.offset),
        F64Store(m) => with_a!(op::F64_STORE, m.offset),
        I32Store8(m) => with_a!(op::I32_STORE8, m.offset),
        I32Store16(m) => with_a!(op::I32_STORE16, m.offset),
        I64Store8(m) => with_a!(op::I64_STORE8, m.offset),
        I64Store16(m) => with_a!(op::I64_STORE16, m.offset),
        I64Store32(m) => with_a!(op::I64_STORE32, m.offset),
        MemorySize => simple!(op::MEMORY_SIZE),
        MemoryGrow => simple!(op::MEMORY_GROW),
        MemoryInit(s) => with_a!(op::MEMORY_INIT, s),
        DataDrop(s) => with_a!(op::DATA_DROP, s),
        MemoryCopy => simple!(op::MEMORY_COPY),
        MemoryFill => simple!(op::MEMORY_FILL),

        I32Const(v) => with_a!(op::I32_CONST, v as u32),
        I64Const(v) => {
            simple!(op::I64_CONST);
            out.push(v as Word);
        }
        F32Const(bits) => with_a!(op::F32_CONST, bits),
        F64Const(bits) => {
            simple!(op::F64_CONST);
            out.push(bits);
        }

        I32Eqz => simple!(op::I32_EQZ),
        I32Eq => simple!(op::I32_EQ),
        I32Ne => simple!(op::I32_NE),
        I32LtS => simple!(op::I32_LT_S),
        I32LtU => simple!(op::I32_LT_U),
        I32GtS => simple!(op::I32_GT_S),
        I32GtU => simple!(op::I32_GT_U),
        I32LeS => simple!(op::I32_LE_S),
        I32LeU => simple!(op::I32_LE_U),
        I32GeS => simple!(op::I32_GE_S),
        I32GeU => simple!(op::I32_GE_U),
        I32Clz => simple!(op::I32_CLZ),
        I32Ctz => simple!(op::I32_CTZ),
        I32Popcnt => simple!(op::I32_POPCNT),
        I32Add => simple!(op::I32_ADD),
        I32Sub => simple!(op::I32_SUB),
        I32Mul => simple!(op::I32_MUL),
        I32DivS => simple!(op::I32_DIV_S),
        I32DivU => simple!(op::I32_DIV_U),
        I32RemS => simple!(op::I32_REM_S),
        I32RemU => simple!(op::I32_REM_U),
        I32And => simple!(op::I32_AND),
        I32Or => simple!(op::I32_OR),
        I32Xor => simple!(op::I32_XOR),
        I32Shl => simple!(op::I32_SHL),
        I32ShrS => simple!(op::I32_SHR_S),
        I32ShrU => simple!(op::I32_SHR_U),
        I32Rotl => simple!(op::I32_ROTL),
        I32Rotr => simple!(op::I32_ROTR),

        I64Eqz => simple!(op::I64_EQZ),
        I64Eq => simple!(op::I64_EQ),
        I64Ne => simple!(op::I64_NE),
        I64LtS => simple!(op::I64_LT_S),
        I64LtU => simple!(op::I64_LT_U),
        I64GtS => simple!(op::I64_GT_S),
        I64GtU => simple!(op::I64_GT_U),
        I64LeS => simple!(op::I64_LE_S),
        I64LeU => simple!(op::I64_LE_U),
        I64GeS => simple!(op::I64_GE_S),
        I64GeU => simple!(op::I64_GE_U),
        I64Clz => simple!(op::I64_CLZ),
        I64Ctz => simple!(op::I64_CTZ),
        I64Popcnt => simple!(op::I64_POPCNT),
        I64Add => simple!(op::I64_ADD),
        I64Sub => simple!(op::I64_SUB),
        I64Mul => simple!(op::I64_MUL),
        I64DivS => simple!(op::I64_DIV_S),
        I64DivU => simple!(op::I64_DIV_U),
        I64RemS => simple!(op::I64_REM_S),
        I64RemU => simple!(op::I64_REM_U),
        I64And => simple!(op::I64_AND),
        I64Or => simple!(op::I64_OR),
        I64Xor => simple!(op::I64_XOR),
        I64Shl => simple!(op::I64_SHL),
        I64ShrS => simple!(op::I64_SHR_S),
        I64ShrU => simple!(op::I64_SHR_U),
        I64Rotl => simple!(op::I64_ROTL),
        I64Rotr => simple!(op::I64_ROTR),

        F32Eq => simple!(op::F32_EQ),
        F32Ne => simple!(op::F32_NE),
        F32Lt => simple!(op::F32_LT),
        F32Gt => simple!(op::F32_GT),
        F32Le => simple!(op::F32_LE),
        F32Ge => simple!(op::F32_GE),
        F32Abs => simple!(op::F32_ABS),
        F32Neg => simple!(op::F32_NEG),
        F32Ceil => simple!(op::F32_CEIL),
        F32Floor => simple!(op::F32_FLOOR),
        F32Trunc => simple!(op::F32_TRUNC),
        F32Nearest => simple!(op::F32_NEAREST),
        F32Sqrt => simple!(op::F32_SQRT),
        F32Add => simple!(op::F32_ADD),
        F32Sub => simple!(op::F32_SUB),
        F32Mul => simple!(op::F32_MUL),
        F32Div => simple!(op::F32_DIV),
        F32Min => simple!(op::F32_MIN),
        F32Max => simple!(op::F32_MAX),
        F32Copysign => simple!(op::F32_COPYSIGN),

        F64Eq => simple!(op::F64_EQ),
        F64Ne => simple!(op::F64_NE),
        F64Lt => simple!(op::F64_LT),
        F64Gt => simple!(op::F64_GT),
        F64Le => simple!(op::F64_LE),
        F64Ge => simple!(op::F64_GE),
        F64Abs => simple!(op::F64_ABS),
        F64Neg => simple!(op::F64_NEG),
        F64Ceil => simple!(op::F64_CEIL),
        F64Floor => simple!(op::F64_FLOOR),
        F64Trunc => simple!(op::F64_TRUNC),
        F64Nearest => simple!(op::F64_NEAREST),
        F64Sqrt => simple!(op::F64_SQRT),
        F64Add => simple!(op::F64_ADD),
        F64Sub => simple!(op::F64_SUB),
        F64Mul => simple!(op::F64_MUL),
        F64Div => simple!(op::F64_DIV),
        F64Min => simple!(op::F64_MIN),
        F64Max => simple!(op::F64_MAX),
        F64Copysign => simple!(op::F64_COPYSIGN),

        I32WrapI64 => simple!(op::I32_WRAP_I64),
        I32TruncF32S => simple!(op::I32_TRUNC_F32_S),
        I32TruncF32U => simple!(op::I32_TRUNC_F32_U),
        I32TruncF64S => simple!(op::I32_TRUNC_F64_S),
        I32TruncF64U => simple!(op::I32_TRUNC_F64_U),
        I64ExtendI32S => simple!(op::I64_EXTEND_I32_S),
        I64ExtendI32U => simple!(op::I64_EXTEND_I32_U),
        I64TruncF32S => simple!(op::I64_TRUNC_F32_S),
        I64TruncF32U => simple!(op::I64_TRUNC_F32_U),
        I64TruncF64S => simple!(op::I64_TRUNC_F64_S),
        I64TruncF64U => simple!(op::I64_TRUNC_F64_U),
        F32ConvertI32S => simple!(op::F32_CONVERT_I32_S),
        F32ConvertI32U => simple!(op::F32_CONVERT_I32_U),
        F32ConvertI64S => simple!(op::F32_CONVERT_I64_S),
        F32ConvertI64U => simple!(op::F32_CONVERT_I64_U),
        F32DemoteF64 => simple!(op::F32_DEMOTE_F64),
        F64ConvertI32S => simple!(op::F64_CONVERT_I32_S),
        F64ConvertI32U => simple!(op::F64_CONVERT_I32_U),
        F64ConvertI64S => simple!(op::F64_CONVERT_I64_S),
        F64ConvertI64U => simple!(op::F64_CONVERT_I64_U),
        F64PromoteF32 => simple!(op::F64_PROMOTE_F32),
        I32ReinterpretF32 => simple!(op::I32_REINTERPRET_F32),
        I64ReinterpretF64 => simple!(op::I64_REINTERPRET_F64),
        F32ReinterpretI32 => simple!(op::F32_REINTERPRET_I32),
        F64ReinterpretI64 => simple!(op::F64_REINTERPRET_I64),

        I32Extend8S => simple!(op::I32_EXTEND8_S),
        I32Extend16S => simple!(op::I32_EXTEND16_S),
        I64Extend8S => simple!(op::I64_EXTEND8_S),
        I64Extend16S => simple!(op::I64_EXTEND16_S),
        I64Extend32S => simple!(op::I64_EXTEND32_S),

        I32TruncSatF32S => simple!(op::I32_TRUNC_SAT_F32_S),
        I32TruncSatF32U => simple!(op::I32_TRUNC_SAT_F32_U),
        I32TruncSatF64S => simple!(op::I32_TRUNC_SAT_F64_S),
        I32TruncSatF64U => simple!(op::I32_TRUNC_SAT_F64_U),
        I64TruncSatF32S => simple!(op::I64_TRUNC_SAT_F32_S),
        I64TruncSatF32U => simple!(op::I64_TRUNC_SAT_F32_U),
        I64TruncSatF64S => simple!(op::I64_TRUNC_SAT_F64_S),
        I64TruncSatF64U => simple!(op::I64_TRUNC_SAT_F64_U),
    }
}

/// The fused forms of `i32.const c; <op>` and `local.get a; i32.const c;
/// <op>` for a binary operator.
fn const_fusions(op: &Instr) -> Option<(u8, u8)> {
    use Instr::*;
    Some(match op {
        I32Add => (op::I32_CONST_I32_ADD, op::LOCAL_GET_I32_CONST_I32_ADD),
        I32Sub => (op::I32_CONST_I32_SUB, op::LOCAL_GET_I32_CONST_I32_SUB),
        I32And => (op::I32_CONST_I32_AND, op::LOCAL_GET_I32_CONST_I32_AND),
        I32Or => (op::I32_CONST_I32_OR, op::LOCAL_GET_I32_CONST_I32_OR),
        I32Shl => (op::I32_CONST_I32_SHL, op::LOCAL_GET_I32_CONST_I32_SHL),
        I32ShrU => (op::I32_CONST_I32_SHR_U, op::LOCAL_GET_I32_CONST_I32_SHR_U),
        I32LtU => (op::I32_CONST_I32_LT_U, op::LOCAL_GET_I32_CONST_I32_LT_U),
        I32GtU => (op::I32_CONST_I32_GT_U, op::LOCAL_GET_I32_CONST_I32_GT_U),
        I32LtS => (op::I32_CONST_I32_LT_S, op::LOCAL_GET_I32_CONST_I32_LT_S),
        I32GtS => (op::I32_CONST_I32_GT_S, op::LOCAL_GET_I32_CONST_I32_GT_S),
        I32Eq => (op::I32_CONST_I32_EQ, op::LOCAL_GET_I32_CONST_I32_EQ),
        I32Ne => (op::I32_CONST_I32_NE, op::LOCAL_GET_I32_CONST_I32_NE),
        _ => return None,
    })
}

/// The fused form of `local.get a; local.get b; <op>` for a binary
/// operator.
fn local2_fusion(op: &Instr) -> Option<u8> {
    use Instr::*;
    Some(match op {
        I32Add => op::LOCAL_GET_LOCAL_GET_I32_ADD,
        I32Sub => op::LOCAL_GET_LOCAL_GET_I32_SUB,
        I32LtU => op::LOCAL_GET_LOCAL_GET_I32_LT_U,
        I32GtU => op::LOCAL_GET_LOCAL_GET_I32_GT_U,
        I32Eq => op::LOCAL_GET_LOCAL_GET_I32_EQ,
        _ => return None,
    })
}

/// The fused form of `<relop>; br_if`.
fn br_if_fusion(op: &Instr) -> Option<u8> {
    use Instr::*;
    Some(match op {
        I32Eqz => op::I32_EQZ_BR_IF,
        I32Eq => op::I32_EQ_BR_IF,
        I32Ne => op::I32_NE_BR_IF,
        I32LtS => op::I32_LT_S_BR_IF,
        I32LtU => op::I32_LT_U_BR_IF,
        I32GtS => op::I32_GT_S_BR_IF,
        I32GtU => op::I32_GT_U_BR_IF,
        I32LeS => op::I32_LE_S_BR_IF,
        I32LeU => op::I32_LE_U_BR_IF,
        I32GeS => op::I32_GE_S_BR_IF,
        I32GeU => op::I32_GE_U_BR_IF,
        _ => return None,
    })
}

/// The superinstruction that starts at `instrs[0]`, if any: the longest
/// match wins. Every pattern consists of instructions that encode to one
/// word each, so the fused handler finds the immediates of the `k`-th
/// instruction in the word `k` after its own.
fn fuse(instrs: &[Instr]) -> Option<u8> {
    use Instr::*;
    let i0 = instrs.first()?;
    let i1 = instrs.get(1);
    let i2 = instrs.get(2);
    let i3 = instrs.get(3);
    Some(match (i0, i1, i2, i3) {
        (LocalGet(_), Some(LocalGet(_)), Some(I32Add), Some(LocalSet(_))) => {
            op::LOCAL_GET_LOCAL_GET_I32_ADD_LOCAL_SET
        }
        (LocalGet(_), Some(I32Const(_)), Some(I32Add), Some(LocalSet(_))) => {
            op::LOCAL_GET_I32_CONST_I32_ADD_LOCAL_SET
        }
        (LocalGet(_), Some(LocalGet(_)), Some(I32Store(_)), _) => op::LOCAL_GET_LOCAL_GET_I32_STORE,
        (LocalGet(_), Some(I32Const(_)), Some(I32Store(_)), _) => op::LOCAL_GET_I32_CONST_I32_STORE,
        (LocalGet(_), Some(LocalGet(_)), Some(o), _) if local2_fusion(o).is_some() => {
            local2_fusion(o)?
        }
        (LocalGet(_), Some(I32Const(_)), Some(o), _) if const_fusions(o).is_some() => {
            const_fusions(o)?.1
        }
        (LocalGet(_), Some(LocalGet(_)), _, _) => op::LOCAL_GET_LOCAL_GET,
        (LocalGet(_), Some(I32Const(_)), _, _) => op::LOCAL_GET_I32_CONST,
        (LocalGet(_), Some(I32Load(_)), _, _) => op::LOCAL_GET_I32_LOAD,
        (I32Const(_), Some(I32Const(_)), _, _) => op::I32_CONST_I32_CONST,
        (I32Const(_), Some(LocalSet(_)), _, _) => op::I32_CONST_LOCAL_SET,
        (I32Const(_), Some(o), _, _) if const_fusions(o).is_some() => const_fusions(o)?.0,
        (I32Add, Some(LocalSet(_)), _, _) => op::I32_ADD_LOCAL_SET,
        (o, Some(BrIf(_)), _, _) if br_if_fusion(o).is_some() => br_if_fusion(o)?,
        _ => return None,
    })
}

/// The name of an opcode (for tests and debugging).
#[cfg(test)]
pub(crate) fn name(code: u8) -> &'static str {
    op::NAMES
        .iter()
        .find(|&&(_, v)| v == code)
        .map_or("?", |&(n, _)| n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instr::{BrTable, MemArg};
    use crate::types::{FuncType, ValType};
    use alloc::vec;

    #[test]
    fn opcodes_are_unique() {
        let mut seen = alloc::collections::BTreeSet::new();
        for &(n, v) in op::NAMES {
            assert!(seen.insert(v), "opcode {v} ({n}) is used twice");
        }
    }

    /// A program of one function of type `types[0]` (or `[] -> []`) with
    /// `locals` and `instrs`.
    fn program(types: Vec<FuncType>, locals: Vec<ValType>, instrs: Vec<Instr>) -> Program {
        let (np, nr) = types
            .first()
            .map_or((0, 0), |t| (t.params.len() as u32, t.results.len() as u32));
        let module = Module {
            types,
            funcs: vec![0],
            code: vec![FuncBody {
                locals,
                instrs,
                br_tables: vec![BrTable {
                    targets: vec![0, 1],
                    default: 9,
                }],
            }],
            ..Module::default()
        };
        compile_module(&module, &[(np, nr)], 0)
    }

    fn ops(program: &Program) -> Vec<&'static str> {
        program.words.iter().map(|&w| name(opcode(w))).collect()
    }

    fn branch(w: Word) -> Branch {
        Branch {
            target: imm_a(w),
            height: imm_b(w),
            arity: imm_c(w),
        }
    }

    #[test]
    fn encodes_immediates_and_resolves_branches() {
        let program = program(
            vec![FuncType {
                params: vec![ValType::I32, ValType::I32],
                results: vec![ValType::I64],
            }],
            vec![ValType::I32, ValType::FuncRef],
            vec![
                // 0: i32.const (operand height 1 after) -> pc 0
                Instr::I32Const(1),
                // 1: block (type 0: 2 params, 1 result) -> pc 1
                Instr::Block {
                    ty: BlockType::Type(0),
                    end: 5,
                },
                // 2: i64.const: 2 words -> pc 2,3
                Instr::I64Const(-1),
                // 3: br 0 (block: unwind to its height) -> pc 4
                Instr::Br(0),
                // 4: br 1 (the function: return) -> pc 5
                Instr::Br(1),
                // 5: end -> pc 6
                Instr::End,
                // 6: loop (result i32) -> pc 7
                Instr::Loop {
                    ty: BlockType::Val(ValType::I32),
                    end: 10,
                },
                // 7: i32.load -> pc 8
                Instr::I32Load(MemArg {
                    align: 2,
                    offset: 0x1234,
                }),
                // 8: br_if 0 (loop: back to pc 8) -> pc 9
                Instr::BrIf(0),
                // 9: br_table -> pc 10
                Instr::BrTable(0),
                // 10: end -> pc 11
                Instr::End,
                // 11: if (result i32), no else -> pc 12
                Instr::If {
                    ty: BlockType::Val(ValType::I32),
                    else_: 13,
                    end: 13,
                },
                // 12: i32.const -> pc 13
                Instr::I32Const(7),
                // 13: end -> pc 14
                Instr::End,
                // 14: if with else -> pc 15
                Instr::If {
                    ty: BlockType::Empty,
                    else_: 15,
                    end: 16,
                },
                // 15: else -> pc 16
                Instr::Else { end: 16 },
                // 16: end -> pc 17
                Instr::End,
                // 17: block with a bad type index -> pc 18
                Instr::Block {
                    ty: BlockType::Type(9),
                    end: 99,
                },
                // 18: br_if 0 to nowhere -> pc 19
                Instr::BrIf(0),
                // 19: end (of the function) -> pc 20
                Instr::End,
            ],
        );
        let f = program.funcs[0];
        assert_eq!((f.entry, f.nparams, f.nresults), (0, 2, 1));
        assert_eq!(
            &program.locals[f.locals_start as usize..f.locals_end as usize],
            &[ValType::I32, ValType::FuncRef]
        );
        let names = ops(&program);
        // Word 3 is a raw immediate; its low bits mean nothing.
        let want = [
            (0, "I32_CONST"),
            (1, "BLOCK"),
            (2, "I64_CONST"),
            (4, "BR"),
            (5, "RETURN"),
            (6, "END"),
            (7, "LOOP"),
            (8, "I32_LOAD"),
            (9, "BR_IF"),
            (10, "BR_TABLE"),
            (11, "END"),
            (12, "IF"),
            (13, "I32_CONST"),
            (14, "END"),
            (15, "IF"),
            (16, "ELSE"),
            (17, "END"),
            (18, "BLOCK"),
            (19, "BR_IF"),
            (20, "RETURN"),
        ];
        for (i, n) in want {
            assert_eq!(names[i], n, "word {i}");
        }
        assert_eq!(names.len(), 21);
        let w = &program.words;
        assert_eq!(w[3], -1i64 as u64);
        // 4 locals (2 params + 2); the block was entered at operand height
        // 1 with 2 params... which the tracker clamps to 0 (only an invalid
        // body does that): its height is 0 + 4 locals, arity 1 result, and
        // it continues after its `end` (instruction 5) at pc 7.
        assert_eq!(
            branch(w[4]),
            Branch {
                target: 7,
                height: 4,
                arity: 1
            }
        );
        // The loop: entered at operand height 1 (the block's result), its
        // body starts at pc 8, a branch to it carries its 0 params.
        assert_eq!(
            branch(w[9]),
            Branch {
                target: 8,
                height: 5,
                arity: 0
            }
        );
        assert_eq!(imm_a(w[8]), 0x1234);
        // br_table 0: targets [loop, function], default 9 (out of range: the
        // function).
        assert_eq!(imm_a(w[10]), 0);
        let t = &program.br_tables[0];
        assert_eq!(t.targets[0].target, 8);
        assert_eq!(t.targets[1].target, RETURN);
        assert_eq!(t.default.target, RETURN);
        // if without else: false jumps past the `end` (instruction 13) to
        // pc 15; with else: into the else arm (after instruction 15, pc 17).
        assert_eq!(imm_a(w[12]), 15);
        assert_eq!(imm_a(w[15]), 17);
        // else: past the `end` (instruction 16) to pc 18.
        assert_eq!(imm_a(w[16]), 18);
        // A block whose end is outside the body continues nowhere.
        assert_eq!(branch(w[19]).target, NOWHERE);
    }

    #[test]
    fn functions_are_laid_out_in_order() {
        let body = vec![
            Instr::If {
                ty: BlockType::Empty,
                else_: 2,
                end: 2,
            },
            Instr::Nop,
            Instr::End,
            Instr::Nop,
            Instr::End,
        ];
        let module = Module {
            funcs: vec![0, 0],
            types: vec![FuncType::default()],
            code: vec![
                FuncBody {
                    locals: vec![],
                    instrs: vec![Instr::Nop, Instr::End],
                    br_tables: vec![],
                },
                FuncBody {
                    locals: vec![ValType::I64],
                    instrs: body,
                    br_tables: vec![],
                },
            ],
            ..Module::default()
        };
        let program = compile_module(&module, &[(1, 0), (0, 0), (0, 0)], 1);
        assert_eq!(program.funcs[0].entry, IMPORTED);
        assert_eq!(program.funcs[0].nparams, 1);
        assert_eq!(program.funcs[1].entry, 0);
        assert_eq!(program.funcs[2].entry, 2);
        assert_eq!(ops(&program)[1], "RETURN");
        assert_eq!(ops(&program)[2], "IF");
        // Continues at instruction 3 of the second function: word 2 + 3.
        assert_eq!(imm_a(program.words[2]), 5);
        assert_eq!(ops(&program)[6], "RETURN");
        assert_eq!(program.funcs[2].locals_start, 0);
        assert_eq!(program.funcs[2].locals_end, 1);
    }

    #[test]
    fn dead_code_after_a_branch_does_not_confuse_heights() {
        // After `br`, the stack is polymorphic: the `i32.add` pops nothing
        // real, and the `br_if` after the block still resolves against the
        // block's height.
        let program = program(
            vec![],
            vec![],
            vec![
                Instr::Block {
                    ty: BlockType::Empty,
                    end: 4,
                },
                Instr::Br(0),
                Instr::I32Add,
                Instr::I32Add,
                Instr::End,
                Instr::I32Const(0),
                Instr::BrIf(0),
                Instr::End,
            ],
        );
        let w = &program.words;
        assert_eq!(ops(&program)[6], "BR_IF_RETURN");
        assert_eq!(
            branch(w[1]),
            Branch {
                target: 5,
                height: 0,
                arity: 0
            }
        );
    }

    #[test]
    fn superinstructions_keep_the_tail() {
        let program = program(
            vec![],
            vec![],
            vec![
                Instr::Loop {
                    ty: BlockType::Empty,
                    end: 9,
                },
                Instr::LocalGet(0),
                Instr::I32Const(1),
                Instr::I32Add,
                Instr::LocalSet(0),
                Instr::LocalGet(0),
                Instr::LocalGet(1),
                Instr::I32LtU,
                Instr::BrIf(0),
                Instr::End,
                Instr::I32Const(0),
                Instr::I32Eqz,
                // Out of the function: not fused.
                Instr::BrIf(0),
                Instr::End,
            ],
        );
        assert_eq!(
            ops(&program),
            [
                "LOOP",
                "LOCAL_GET_I32_CONST_I32_ADD_LOCAL_SET",
                "I32_CONST_I32_ADD",
                "I32_ADD_LOCAL_SET",
                "LOCAL_SET",
                "LOCAL_GET_LOCAL_GET_I32_LT_U",
                "LOCAL_GET",
                "I32_LT_U_BR_IF",
                "BR_IF",
                "END",
                "I32_CONST",
                "I32_EQZ",
                "BR_IF_RETURN",
                "RETURN",
            ]
        );
        // The immediates of every word are those of the plain instruction.
        let w = &program.words;
        assert_eq!(imm_a(w[1]), 0);
        assert_eq!(imm_a(w[2]), 1);
        assert_eq!(imm_a(w[4]), 0);
        assert_eq!(imm_a(w[6]), 1);
        assert_eq!(
            branch(w[8]),
            Branch {
                target: 1,
                height: 0,
                arity: 0
            }
        );
    }
}
