//! Error and trap types shared by the decoder, validator and interpreter.
//!
//! `Error` is returned by everything that happens before execution: decoding
//! (`Malformed`, `Unsupported`), validation (`Invalid`) and instantiation
//! (`Link`, `Limit`, ...). `Trap` is what a running instance can hit.
//!
//! Later steps (validator, interpreter) may add variants; the decoder only
//! produces `Malformed`, `Unsupported` and `Limit`.

use core::fmt;

/// Errors raised while decoding, validating or instantiating a module, or
/// while setting up an execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The binary does not follow the WebAssembly binary format. `offset` is
    /// the byte offset into the module where decoding failed; `msg` is a
    /// short, stable description (spec test suite wording where possible).
    Malformed { offset: usize, msg: &'static str },
    /// The binary uses a feature Kalium does not support (SIMD, threads,
    /// exception handling, tail calls, ...). `offset` points at the opcode or
    /// section that triggered it.
    Unsupported { offset: usize, msg: &'static str },
    /// The module decoded but does not validate. `func` is the index of the
    /// function (in the function index space) being validated, if any, and
    /// `instr` the index of the offending instruction in its body.
    Invalid {
        msg: &'static str,
        func: Option<u32>,
        instr: Option<u32>,
    },
    /// A configured resource limit was exceeded (too many locals, too large
    /// a minimum memory, ...). `msg` names the limit.
    Limit { msg: &'static str },
    /// An import could not be resolved at instantiation. `import` is the
    /// index into `Module::imports()`.
    Link { import: u32 },
    /// `Execution::new` was asked for a function index that does not exist
    /// or is an import.
    NoSuchFunction { func: u32 },
    /// `Execution::new` was given arguments that do not match the function's
    /// parameter types.
    ArgumentMismatch,
    /// Instantiation or execution needed more memory than the limits allow.
    OutOfMemory,
    /// Instantiation trapped while applying an active data or element
    /// segment (the trap carries the spec message, e.g. "out of bounds
    /// table access").
    Trap(Trap),
}

impl Error {
    /// The `msg` of a decode/validation error, or a generic description.
    pub fn message(&self) -> &'static str {
        match self {
            Error::Malformed { msg, .. } => msg,
            Error::Unsupported { msg, .. } => msg,
            Error::Invalid { msg, .. } => msg,
            Error::Limit { msg } => msg,
            Error::Link { .. } => "unknown import",
            Error::NoSuchFunction { .. } => "unknown function",
            Error::ArgumentMismatch => "argument type mismatch",
            Error::OutOfMemory => "out of memory",
            Error::Trap(t) => t.message(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Malformed { offset, msg } => {
                write!(f, "malformed module at offset {offset:#x}: {msg}")
            }
            Error::Unsupported { offset, msg } => {
                write!(f, "unsupported feature at offset {offset:#x}: {msg}")
            }
            Error::Invalid { msg, func, instr } => {
                write!(f, "invalid module: {msg}")?;
                if let Some(func) = func {
                    write!(f, " (function {func}")?;
                    if let Some(instr) = instr {
                        write!(f, ", instruction {instr}")?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
            Error::Limit { msg } => write!(f, "limit exceeded: {msg}"),
            Error::Link { import } => write!(f, "unresolved import #{import}"),
            Error::NoSuchFunction { func } => write!(f, "no such function: {func}"),
            Error::ArgumentMismatch => write!(f, "argument type mismatch"),
            Error::OutOfMemory => write!(f, "out of memory"),
            Error::Trap(t) => write!(f, "instantiation trapped: {t}"),
        }
    }
}

/// Runtime traps. A trap terminates the execution that raised it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    /// Linear memory access outside the current memory size.
    OutOfBounds,
    /// Table access outside the table size (`table.get/set/copy/init/fill`).
    TableOutOfBounds,
    /// `unreachable` was executed.
    Unreachable,
    /// `i32.div_s`/`i64.div_s` of `INT_MIN / -1`.
    IntegerOverflow,
    /// Integer division or remainder by zero.
    IntegerDivideByZero,
    /// Trapping float-to-int conversion of NaN or an out-of-range value.
    InvalidConversion,
    /// `call_indirect` through a table entry whose type does not match.
    IndirectCallTypeMismatch,
    /// `call_indirect` with an index outside the table.
    UndefinedElement,
    /// `call_indirect` through a null table entry.
    UninitializedElement,
    /// The value stack grew past `Limits::max_stack_values`.
    StackOverflow,
    /// The call depth grew past `Limits::max_call_depth`.
    CallDepth,
    /// The fuel budget of this run was exhausted (only used when the
    /// interpreter reports it as a trap rather than `ExecResult::OutOfFuel`).
    OutOfFuel,
    /// The host function reported an unrecoverable error.
    Host,
    /// The interpreter reached a state only an invalid module can produce
    /// (type confusion on the stack, an index out of range, ...). The
    /// validator should have rejected the module; the execution stops
    /// instead of panicking.
    Invalid,
}

impl Trap {
    /// The message the spec test suite uses for this trap.
    pub fn message(self) -> &'static str {
        match self {
            Trap::OutOfBounds => "out of bounds memory access",
            Trap::TableOutOfBounds => "out of bounds table access",
            Trap::Unreachable => "unreachable",
            Trap::IntegerOverflow => "integer overflow",
            Trap::IntegerDivideByZero => "integer divide by zero",
            Trap::InvalidConversion => "invalid conversion to integer",
            Trap::IndirectCallTypeMismatch => "indirect call type mismatch",
            Trap::UndefinedElement => "undefined element",
            Trap::UninitializedElement => "uninitialized element",
            Trap::StackOverflow => "stack overflow",
            Trap::CallDepth => "call stack exhausted",
            Trap::OutOfFuel => "out of fuel",
            Trap::Host => "host error",
            Trap::Invalid => "invalid module state",
        }
    }
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}
