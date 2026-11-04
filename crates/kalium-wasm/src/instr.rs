//! The instruction set and decoded function bodies.
//!
//! # Representation
//!
//! A function body is decoded into a flat `Vec<Instr>` ([`FuncBody::instrs`]),
//! one entry per instruction, with every immediate already resolved
//! (LEB128 decoded, block types parsed, memargs split). The final `end` of
//! the function is kept as the last instruction, so a body always ends with
//! [`Instr::End`] and the program counter of the interpreter is simply an
//! index into `instrs`.
//!
//! ## Control flow is pre-resolved
//!
//! Structured control instructions carry the index of their matching
//! instructions so the interpreter never has to scan for `end`/`else`:
//!
//! | Instruction | Fields |
//! | --- | --- |
//! | `Block { ty, end }` | `end` is the index of the matching `End`. |
//! | `Loop { ty, end }` | `end` is the index of the matching `End`. |
//! | `If { ty, else_, end }` | `end` is the index of the matching `End`; `else_` is the index of the matching `Else`, or **equal to `end`** when the `if` has no else arm. |
//! | `Else { end }` | `end` is the index of the `End` closing the enclosing `if`. |
//! | `End` | Closes the innermost block, or the function. |
//!
//! Given these, an interpreter that keeps a label stack can execute:
//!
//! - `Block`/`Loop`/`If`: push a label whose continuation is `end + 1`
//!   (for `Loop`, the loop's own index). On a false `If` condition, jump to
//!   `else_ + 1` when `else_ != end` (into the else arm, label pushed), or
//!   to `end + 1` without pushing a label when `else_ == end` (no else arm;
//!   jumping to `end` instead, after pushing, works too since `End` pops).
//! - `Else` (reached from the then arm): jump to `end + 1`, popping the label.
//! - `End`: pop the label and continue.
//! - `Br n`: pop `n + 1` labels and jump to the continuation of the last one.
//!
//! Branch targets are relative label depths exactly as in the binary format;
//! `br_table` keeps its targets in a side table ([`FuncBody::br_tables`])
//! so that `Instr` stays a small, `Copy` enum (`size_of::<Instr>() == 16`).
//!
//! ## Immediates
//!
//! - Float constants are stored as raw bits (`F32Const(u32)`, `F64Const(u64)`)
//!   so NaN payloads survive decoding unchanged.
//! - Memory instructions carry a [`MemArg`] with the `offset` and the
//!   alignment *exponent* as encoded in the binary (`2^align` bytes); the
//!   validator checks it against the natural alignment. All memory
//!   instructions address memory 0 (multi-memory is out of scope).
//! - `CallIndirect { ty, table }` and the table instructions carry explicit
//!   table indices (reference-types encoding).
//! - `SelectT(ValType)` is the typed `select`; untyped `select` is
//!   [`Instr::Select`].
//!
//! Locals are stored expanded in [`FuncBody::locals`] (one entry per local,
//! *excluding* parameters), so local index `i` of the function refers to
//! `params[i]` when `i < params.len()` and `locals[i - params.len()]`
//! otherwise.

use crate::types::{RefType, ValType};
use alloc::vec::Vec;

/// The type of a structured control instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockType {
    /// `[] -> []`
    Empty,
    /// `[] -> [t]`
    Val(ValType),
    /// `[t1*] -> [t2*]` through a type index (multi-value).
    Type(u32),
}

/// The memory immediate of load/store instructions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MemArg {
    /// Alignment exponent: the alignment hint is `1 << align` bytes.
    pub align: u32,
    /// Static offset added to the dynamic address.
    pub offset: u32,
}

/// A `br_table` immediate, stored in [`FuncBody::br_tables`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BrTable {
    pub targets: Vec<u32>,
    pub default: u32,
}

/// A decoded instruction with all immediates resolved. See the module
/// documentation for the control-flow representation.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Instr {
    // ---- control --------------------------------------------------------
    Unreachable,
    Nop,
    Block {
        ty: BlockType,
        end: u32,
    },
    Loop {
        ty: BlockType,
        end: u32,
    },
    /// `else_ == end` when there is no else arm (see the module docs).
    If {
        ty: BlockType,
        else_: u32,
        end: u32,
    },
    Else {
        end: u32,
    },
    End,
    Br(u32),
    BrIf(u32),
    /// Index into [`FuncBody::br_tables`].
    BrTable(u32),
    Return,
    Call(u32),
    CallIndirect {
        ty: u32,
        table: u32,
    },

    // ---- parametric -----------------------------------------------------
    Drop,
    Select,
    SelectT(ValType),

    // ---- variable -------------------------------------------------------
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    GlobalGet(u32),
    GlobalSet(u32),

    // ---- reference ------------------------------------------------------
    RefNull(RefType),
    RefIsNull,
    RefFunc(u32),

    // ---- table ----------------------------------------------------------
    TableGet(u32),
    TableSet(u32),
    TableInit {
        elem: u32,
        table: u32,
    },
    ElemDrop(u32),
    TableCopy {
        dst: u32,
        src: u32,
    },
    TableGrow(u32),
    TableSize(u32),
    TableFill(u32),

    // ---- memory ---------------------------------------------------------
    I32Load(MemArg),
    I64Load(MemArg),
    F32Load(MemArg),
    F64Load(MemArg),
    I32Load8S(MemArg),
    I32Load8U(MemArg),
    I32Load16S(MemArg),
    I32Load16U(MemArg),
    I64Load8S(MemArg),
    I64Load8U(MemArg),
    I64Load16S(MemArg),
    I64Load16U(MemArg),
    I64Load32S(MemArg),
    I64Load32U(MemArg),
    I32Store(MemArg),
    I64Store(MemArg),
    F32Store(MemArg),
    F64Store(MemArg),
    I32Store8(MemArg),
    I32Store16(MemArg),
    I64Store8(MemArg),
    I64Store16(MemArg),
    I64Store32(MemArg),
    MemorySize,
    MemoryGrow,
    MemoryInit(u32),
    DataDrop(u32),
    MemoryCopy,
    MemoryFill,

    // ---- numeric constants ---------------------------------------------
    I32Const(i32),
    I64Const(i64),
    /// Raw IEEE 754 bits.
    F32Const(u32),
    /// Raw IEEE 754 bits.
    F64Const(u64),

    // ---- i32 ------------------------------------------------------------
    I32Eqz,
    I32Eq,
    I32Ne,
    I32LtS,
    I32LtU,
    I32GtS,
    I32GtU,
    I32LeS,
    I32LeU,
    I32GeS,
    I32GeU,
    I32Clz,
    I32Ctz,
    I32Popcnt,
    I32Add,
    I32Sub,
    I32Mul,
    I32DivS,
    I32DivU,
    I32RemS,
    I32RemU,
    I32And,
    I32Or,
    I32Xor,
    I32Shl,
    I32ShrS,
    I32ShrU,
    I32Rotl,
    I32Rotr,

    // ---- i64 ------------------------------------------------------------
    I64Eqz,
    I64Eq,
    I64Ne,
    I64LtS,
    I64LtU,
    I64GtS,
    I64GtU,
    I64LeS,
    I64LeU,
    I64GeS,
    I64GeU,
    I64Clz,
    I64Ctz,
    I64Popcnt,
    I64Add,
    I64Sub,
    I64Mul,
    I64DivS,
    I64DivU,
    I64RemS,
    I64RemU,
    I64And,
    I64Or,
    I64Xor,
    I64Shl,
    I64ShrS,
    I64ShrU,
    I64Rotl,
    I64Rotr,

    // ---- f32 ------------------------------------------------------------
    F32Eq,
    F32Ne,
    F32Lt,
    F32Gt,
    F32Le,
    F32Ge,
    F32Abs,
    F32Neg,
    F32Ceil,
    F32Floor,
    F32Trunc,
    F32Nearest,
    F32Sqrt,
    F32Add,
    F32Sub,
    F32Mul,
    F32Div,
    F32Min,
    F32Max,
    F32Copysign,

    // ---- f64 ------------------------------------------------------------
    F64Eq,
    F64Ne,
    F64Lt,
    F64Gt,
    F64Le,
    F64Ge,
    F64Abs,
    F64Neg,
    F64Ceil,
    F64Floor,
    F64Trunc,
    F64Nearest,
    F64Sqrt,
    F64Add,
    F64Sub,
    F64Mul,
    F64Div,
    F64Min,
    F64Max,
    F64Copysign,

    // ---- conversions ----------------------------------------------------
    I32WrapI64,
    I32TruncF32S,
    I32TruncF32U,
    I32TruncF64S,
    I32TruncF64U,
    I64ExtendI32S,
    I64ExtendI32U,
    I64TruncF32S,
    I64TruncF32U,
    I64TruncF64S,
    I64TruncF64U,
    F32ConvertI32S,
    F32ConvertI32U,
    F32ConvertI64S,
    F32ConvertI64U,
    F32DemoteF64,
    F64ConvertI32S,
    F64ConvertI32U,
    F64ConvertI64S,
    F64ConvertI64U,
    F64PromoteF32,
    I32ReinterpretF32,
    I64ReinterpretF64,
    F32ReinterpretI32,
    F64ReinterpretI64,

    // ---- sign extension -------------------------------------------------
    I32Extend8S,
    I32Extend16S,
    I64Extend8S,
    I64Extend16S,
    I64Extend32S,

    // ---- non-trapping float-to-int --------------------------------------
    I32TruncSatF32S,
    I32TruncSatF32U,
    I32TruncSatF64S,
    I32TruncSatF64U,
    I64TruncSatF32S,
    I64TruncSatF32U,
    I64TruncSatF64S,
    I64TruncSatF64U,
}

/// A decoded function body: its declared locals (expanded, parameters
/// excluded) and its instruction sequence. See the module documentation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FuncBody {
    /// Declared locals, one entry per local, in order (parameters excluded).
    pub locals: Vec<ValType>,
    /// The instructions; always ends with [`Instr::End`].
    pub instrs: Vec<Instr>,
    /// Side table for [`Instr::BrTable`].
    pub br_tables: Vec<BrTable>,
}

impl FuncBody {
    /// The `br_table` immediate referenced by `Instr::BrTable(idx)`.
    pub fn br_table(&self, idx: u32) -> Option<&BrTable> {
        self.br_tables.get(idx as usize)
    }
}

/// A constant expression (global initializers, segment offsets, element
/// items). The decoder stores the instruction sequence without the
/// terminating `end`; validation restricts it to a single constant
/// instruction (`*.const`, `ref.null`, `ref.func`, `global.get`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConstExpr {
    pub instrs: Vec<Instr>,
}

impl ConstExpr {
    /// The single instruction of a well-formed constant expression, or
    /// `None` if the expression is empty or has more than one instruction.
    pub fn single(&self) -> Option<Instr> {
        match self.instrs.as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instr_is_small() {
        assert_eq!(core::mem::size_of::<Instr>(), 16);
    }
}
