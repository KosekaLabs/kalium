//! Validator tests: a table-driven suite of valid and invalid modules, one
//! group per validation rule, plus error-location and never-panic checks.
//!
//! The official spec tests are run separately by `tests/spec_validate.rs`.

use kalium_wasm::decode::decode;
use kalium_wasm::instr::{BlockType, BrTable, ConstExpr, FuncBody, Instr, MemArg};
use kalium_wasm::module::{Export, ExportDesc, Global, Import, ImportDesc};
use kalium_wasm::types::{FuncType, GlobalType, RefType, ValType};
use kalium_wasm::validate::validate;
use kalium_wasm::{Error, Module};

fn wasm(wat: &str) -> Vec<u8> {
    wat::parse_str(wat).unwrap_or_else(|e| panic!("bad wat: {e}\n{wat}"))
}

/// Builds `(module ...)` from the given fields and validates it.
fn check(fields: &str) -> Result<Module, Error> {
    let wat = format!("(module {fields})");
    let bytes = wasm(&wat);
    // The decoder must accept everything in this suite; only the validator
    // may reject it.
    decode(&bytes).unwrap_or_else(|e| panic!("decode failed: {e}\n{wat}"));
    Module::new(&bytes)
}

fn valid(fields: &str) {
    if let Err(e) = check(fields) {
        panic!("expected valid, got {e}\n(module {fields})");
    }
}

fn invalid(fields: &str, msg: &str) {
    match check(fields) {
        Ok(_) => panic!("expected invalid ({msg}), got valid\n(module {fields})"),
        Err(Error::Invalid { msg: got, .. }) => {
            assert_eq!(got, msg, "wrong message for\n(module {fields})");
        }
        Err(e) => panic!("expected Error::Invalid({msg}), got {e:?}\n(module {fields})"),
    }
}

/// Like `invalid` but also checks the reported function and instruction.
fn invalid_at(fields: &str, msg: &str, func: u32, instr: u32) {
    match check(fields) {
        Ok(_) => panic!("expected invalid ({msg}), got valid\n(module {fields})"),
        Err(Error::Invalid { msg: got, func: f, instr: i }) => {
            assert_eq!(got, msg, "wrong message for\n(module {fields})");
            assert_eq!((f, i), (Some(func), Some(instr)), "wrong location for\n(module {fields})");
        }
        Err(e) => panic!("expected Error::Invalid({msg}), got {e:?}\n(module {fields})"),
    }
}

/// Runs a table of `(fields, expected)` where `expected` is `None` for a
/// valid module and `Some(message)` for an invalid one.
fn table(cases: &[(&str, Option<&str>)]) {
    for (fields, expected) in cases {
        match expected {
            None => valid(fields),
            Some(msg) => invalid(fields, msg),
        }
    }
}

// ---------------------------------------------------------------------------
// Operand stack typing
// ---------------------------------------------------------------------------

#[test]
fn numeric_operand_typing() {
    table(&[
        ("(func (result i32) i32.const 1 i32.const 2 i32.add)", None),
        ("(func (result i64) i64.const 1 i64.const 2 i64.mul)", None),
        ("(func (result f32) f32.const 1 f32.const 2 f32.div)", None),
        ("(func (result f64) f64.const 1 f64.sqrt)", None),
        ("(func (result i32) i64.const 1 i64.eqz)", None),
        ("(func (result i32) f64.const 1 f64.const 2 f64.lt)", None),
        ("(func (result i32) i32.const 1 i64.const 2 i32.add)", Some("type mismatch")),
        ("(func (result i32) i32.const 1 i32.add)", Some("type mismatch")),
        ("(func (result i64) i32.const 1 i32.const 2 i32.add)", Some("type mismatch")),
        ("(func i32.const 1)", Some("type mismatch")),
        ("(func (result i32))", Some("type mismatch")),
        ("(func (result i32) i32.const 1 i32.const 2)", Some("type mismatch")),
        ("(func (result i32) f32.const 1 i32.eqz)", Some("type mismatch")),
        ("(func (result i32) i32.const 1 i64.eqz)", Some("type mismatch")),
        ("(func (result f32) f32.const 1 f64.neg)", Some("type mismatch")),
        ("(func (result i32) i32.const 0 i32.const 0 i32.const 0 i32.add)", Some("type mismatch")),
        ("(func drop)", Some("type mismatch")),
        ("(func i32.const 0 drop drop)", Some("type mismatch")),
    ]);
}

#[test]
fn conversion_typing() {
    table(&[
        ("(func (result i32) i64.const 1 i32.wrap_i64)", None),
        ("(func (result i64) i32.const 1 i64.extend_i32_u)", None),
        ("(func (result i32) f32.const 1 i32.trunc_f32_s)", None),
        ("(func (result i32) f64.const 1 i32.trunc_sat_f64_u)", None),
        ("(func (result i64) f32.const 1 i64.trunc_sat_f32_s)", None),
        ("(func (result f32) i64.const 1 f32.convert_i64_u)", None),
        ("(func (result f64) f32.const 1 f64.promote_f32)", None),
        ("(func (result f32) f64.const 1 f32.demote_f64)", None),
        ("(func (result i32) f32.const 1 i32.reinterpret_f32)", None),
        ("(func (result f64) i64.const 1 f64.reinterpret_i64)", None),
        ("(func (result i32) i32.const 1 i32.extend8_s)", None),
        ("(func (result i64) i64.const 1 i64.extend32_s)", None),
        ("(func (result i32) i32.const 1 i32.wrap_i64)", Some("type mismatch")),
        ("(func (result i32) f64.const 1 i32.trunc_f32_s)", Some("type mismatch")),
        ("(func (result i64) i32.const 1 i32.extend8_s)", Some("type mismatch")),
        ("(func (result i32) i64.const 1 i32.reinterpret_f32)", Some("type mismatch")),
        ("(func (result f64) f32.const 1 f64.reinterpret_i64)", Some("type mismatch")),
    ]);
}

#[test]
fn stack_is_polymorphic_after_unreachable() {
    table(&[
        ("(func (result i32) unreachable)", None),
        ("(func (result i32) unreachable i32.add)", None),
        ("(func (result i32) unreachable i64.const 0 i32.add)", Some("type mismatch")),
        ("(func (result i32) unreachable i32.const 0 i64.add)", Some("type mismatch")),
        ("(func (result i32 i64) unreachable)", None),
        ("(func (result i32) unreachable drop)", None),
        ("(func (result i32) i32.const 0 return i64.const 0)", Some("type mismatch")),
        ("(func (result i32) i32.const 0 return i64.const 0 drop)", None),
        ("(func (result i32) block br 0 end i32.const 1)", None),
        ("(func (result i32) br 0 f32.const 0)", Some("type mismatch")),
        ("(func (result i32) i32.const 0 br 0 f32.const 0 drop)", None),
        // The polymorphic stack ends with the frame.
        ("(func (result i32) block unreachable end i32.const 1)", None),
        ("(func (result i32) block unreachable end)", Some("type mismatch")),
        ("(func (result i32) block (result i32) unreachable end)", None),
        ("(func (result i32) block (result i32) unreachable i64.const 1 end)", Some("type mismatch")),
        // Values below the frame are not reachable from a polymorphic stack.
        ("(func (result i32) i32.const 1 block unreachable end)", None),
        ("(func (result i32) i32.const 1 block unreachable drop end)", None),
        ("(func (result i32) i32.const 1 block unreachable end drop)", Some("type mismatch")),
        ("(func (result i32) i64.const 1 block (result i32) unreachable i32.add end drop i32.wrap_i64)", None),
        ("(func (result i32) i64.const 1 block (result i32) unreachable i32.add end)", Some("type mismatch")),
    ]);
}

// ---------------------------------------------------------------------------
// Control frames and label types
// ---------------------------------------------------------------------------

#[test]
fn block_loop_if_typing() {
    table(&[
        ("(func block end)", None),
        ("(func (result i32) block (result i32) i32.const 1 end)", None),
        ("(func (result i32) block (result i32) i64.const 1 end)", Some("type mismatch")),
        ("(func block (result i32) end)", Some("type mismatch")),
        ("(func block i32.const 1 end)", Some("type mismatch")),
        ("(func (result i32) loop (result i32) i32.const 1 end)", None),
        ("(func loop (result i32) end)", Some("type mismatch")),
        ("(func i32.const 0 if end)", None),
        ("(func i32.const 0 if else end)", None),
        ("(func if end)", Some("type mismatch")),
        ("(func i64.const 0 if end)", Some("type mismatch")),
        ("(func (result i32) i32.const 0 if (result i32) i32.const 1 else i32.const 2 end)", None),
        ("(func (result i32) i32.const 0 if (result i32) i32.const 1 else i64.const 2 end)", Some("type mismatch")),
        ("(func (result i32) i32.const 0 if (result i32) i64.const 1 else i32.const 2 end)", Some("type mismatch")),
        ("(func (result i32) i32.const 0 if (result i32) else i32.const 2 end)", Some("type mismatch")),
        // `if` without `else` must have params == results.
        ("(func (result i32) i32.const 0 if (result i32) i32.const 1 end)", Some("type mismatch")),
        ("(func i32.const 0 if nop end)", None),
        // Values entering a block are not visible inside it.
        ("(func (result i32) i32.const 1 block (result i32) i32.const 2 i32.add end)", Some("type mismatch")),
        ("(func i32.const 1 block drop end)", Some("type mismatch")),
        // Nested blocks.
        ("(func (result i32) block (result i32) block (result i32) i32.const 1 end end)", None),
        ("(func (result i32) block (result i32) block (result i64) i64.const 1 end end)", Some("type mismatch")),
    ]);
}

#[test]
fn multi_value_block_types() {
    table(&[
        ("(func (result i32 i64) i32.const 1 i64.const 2)", None),
        ("(func (result i32 i64) i64.const 2 i32.const 1)", Some("type mismatch")),
        ("(func (result i32 i64) block (result i32 i64) i32.const 1 i64.const 2 end)", None),
        ("(func (result i32) i32.const 1 i32.const 2 block (param i32 i32) (result i32) i32.add end)", None),
        ("(func (result i32) i32.const 1 block (param i32 i32) (result i32) i32.add end)", Some("type mismatch")),
        ("(func (result i32) i32.const 1 i64.const 2 block (param i32 i32) (result i32) i32.add end)", Some("type mismatch")),
        ("(func (result i32) i32.const 1 i32.const 2 block (param i32 i32) (result i32) end)", Some("type mismatch")),
        ("(func (result i32) i32.const 1 loop (param i32) (result i32) end)", None),
        ("(func (result i32) i32.const 1 i32.const 0 if (param i32) (result i32) else end)", None),
        ("(func (result i32) i32.const 1 i32.const 0 if (param i32) (result i32) end)", None),
        ("(func (result i32) i32.const 1 i32.const 0 if (param i32) (result i32) drop i32.const 2 end)", None),
        ("(func (result i64) i32.const 1 i32.const 0 if (param i32) (result i64) end)", Some("type mismatch")),
        (
            "(func (result i64) i32.const 1 i32.const 0 if (param i32) (result i64) drop i64.const 2 else drop i64.const 3 end)",
            None,
        ),
        // Loop labels carry the params; block labels the results.
        ("(func i32.const 1 loop (param i32) br 0 end)", None),
        ("(func i32.const 1 loop (param i32) drop br 0 end)", Some("type mismatch")),
        ("(func (result i32 i64) block (result i32 i64) i32.const 1 i64.const 2 br 0 end)", None),
        ("(func (result i32 i64) block (result i32 i64) i32.const 1 br 0 end)", Some("type mismatch")),
        ("(func (result i32 i64) block (result i32 i64) i32.const 1 i64.const 2 i32.const 0 br_if 0 end)", None),
        // A block type with an out-of-range type index.
        ("(type (func)) (func block (type 1) end)", Some("unknown type")),
    ]);
}

#[test]
fn branches_and_labels() {
    table(&[
        ("(func br 0)", None),
        ("(func block br 1 end)", None),
        ("(func block br 2 end)", Some("unknown label")),
        ("(func br 1)", Some("unknown label")),
        ("(func i32.const 0 br_if 0)", None),
        ("(func block i32.const 0 br_if 1 end)", None),
        ("(func block i32.const 0 br_if 2 end)", Some("unknown label")),
        ("(func block br_if 0 end)", Some("type mismatch")),
        ("(func block i64.const 0 br_if 0 end)", Some("type mismatch")),
        ("(func (result i32) block (result i32) i32.const 1 i32.const 0 br_if 0 end)", None),
        ("(func (result i32) block (result i32) i32.const 0 br_if 0 end)", Some("type mismatch")),
        ("(func (result i32) block (result i32) i64.const 1 i32.const 0 br_if 0 end)", Some("type mismatch")),
        // br_if leaves the values on the stack.
        ("(func (result i32) block (result i32) i32.const 1 i32.const 0 br_if 0 drop i32.const 2 end)", None),
        ("(func (result i32) i32.const 1 br 0)", None),
        ("(func (result i32) br 0)", Some("type mismatch")),
        ("(func (result i32) block (result i32) br 0 end)", Some("type mismatch")),
        ("(func (result i32) block (result i32) i64.const 1 br 0 end)", Some("type mismatch")),
        ("(func (result i32) i32.const 1 return)", None),
        ("(func (result i32) return)", Some("type mismatch")),
        ("(func (result i32) i64.const 1 return)", Some("type mismatch")),
        ("(func (result i32) block return end)", Some("type mismatch")),
        ("(func (result i32) block i32.const 1 return end i32.const 2)", None),
        ("(func (result i32) block i32.const 1 return end)", Some("type mismatch")),
        ("(func return i32.const 0)", Some("type mismatch")),
        ("(func (result i32) loop (result i32) i32.const 1 br 1 end)", None),
        ("(func (result i32) loop (result i32) i32.const 1 br 0 end)", None),
        ("(func (result i32) loop (result i32) br 0 end)", None),
        ("(func (result i32) loop (param i32) (result i32) br 0 end)", Some("type mismatch")),
    ]);
}

#[test]
fn br_table_arity() {
    table(&[
        ("(func i32.const 0 br_table 0)", None),
        ("(func block i32.const 0 br_table 0 1 end)", None),
        ("(func block i32.const 0 br_table 0 2 end)", Some("unknown label")),
        ("(func block i32.const 0 br_table 2 0 end)", Some("unknown label")),
        ("(func block br_table 0 end)", Some("type mismatch")),
        ("(func block i64.const 0 br_table 0 end)", Some("type mismatch")),
        ("(func (result i32) block (result i32) block (result i32) i32.const 1 i32.const 0 br_table 0 1 end end)", None),
        // All targets must have the same arity.
        ("(func (result i32) block (result i32) block i32.const 1 i32.const 0 br_table 0 1 end end)", Some("type mismatch")),
        ("(func (result i32) block (result i32) block i32.const 0 br_table 1 0 end i32.const 1 end)", Some("type mismatch")),
        // Same arity but different types.
        (
            "(func (result i32) block (result i32) block (result i64) i32.const 1 i32.const 0 br_table 0 1 end drop i32.const 2 end)",
            Some("type mismatch"),
        ),
        (
            "(func (result i32) block (result i32) block (result i64) i64.const 1 i32.const 0 br_table 0 1 end drop i32.const 2 end)",
            Some("type mismatch"),
        ),
        // On a polymorphic stack the arity rule still holds.
        ("(func block (result i32) block unreachable br_table 0 1 end unreachable end drop)", Some("type mismatch")),
        ("(func block (result i32) block (result i32) unreachable br_table 0 1 end end drop)", None),
        // Unreachable code after br_table.
        ("(func i32.const 0 br_table 0 0 i64.add drop)", None),
        // The default label alone defines the arity.
        ("(func (result i32) block (result i32) i32.const 1 i32.const 0 br_table 0 end)", None),
        ("(func block (result i32) i32.const 0 br_table 0 end drop)", Some("type mismatch")),
    ]);
}

// ---------------------------------------------------------------------------
// Parametric and reference instructions
// ---------------------------------------------------------------------------

#[test]
fn select_typing() {
    table(&[
        ("(func (result i32) i32.const 1 i32.const 2 i32.const 0 select)", None),
        ("(func (result i64) i64.const 1 i64.const 2 i32.const 0 select)", None),
        ("(func (result f32) f32.const 1 f32.const 2 i32.const 0 select)", None),
        ("(func (result f64) f64.const 1 f64.const 2 i32.const 0 select)", None),
        ("(func (result i32) i32.const 1 i64.const 2 i32.const 0 select)", Some("type mismatch")),
        ("(func (result i32) i32.const 1 i32.const 2 i64.const 0 select)", Some("type mismatch")),
        ("(func (result i32) i32.const 1 i32.const 0 select)", Some("type mismatch")),
        ("(func (result i32) i32.const 0 select)", Some("type mismatch")),
        // Untyped select only works on numeric types.
        ("(func (result funcref) ref.null func ref.null func i32.const 0 select)", Some("type mismatch")),
        ("(func (result externref) ref.null extern ref.null extern i32.const 0 select)", Some("type mismatch")),
        ("(func (result funcref) ref.null func ref.null func i32.const 0 select (result funcref))", None),
        ("(func (result externref) ref.null extern ref.null extern i32.const 0 select (result externref))", None),
        ("(func (result i32) i32.const 1 i32.const 2 i32.const 0 select (result i32))", None),
        ("(func (result i32) i32.const 1 i32.const 2 i32.const 0 select (result i64))", Some("type mismatch")),
        ("(func (result funcref) ref.null func ref.null extern i32.const 0 select (result funcref))", Some("type mismatch")),
        // Unknown operands.
        ("(func (result i32) unreachable select)", None),
        ("(func (result i32) unreachable i32.const 0 select)", None),
        ("(func (result i32) unreachable i32.const 1 i32.const 0 select)", None),
        ("(func (result i64) unreachable i32.const 1 i32.const 0 select)", Some("type mismatch")),
        ("(func (result i32) unreachable ref.null func i32.const 0 select)", Some("type mismatch")),
        ("(func (result funcref) unreachable ref.null func i32.const 0 select (result funcref))", None),
        ("(func (result i32) unreachable i64.const 1 i32.const 0 select i32.eqz)", Some("type mismatch")),
    ]);
}

#[test]
fn reference_instructions() {
    table(&[
        ("(func (result funcref) ref.null func)", None),
        ("(func (result externref) ref.null extern)", None),
        ("(func (result externref) ref.null func)", Some("type mismatch")),
        ("(func (result i32) ref.null func ref.is_null)", None),
        ("(func (result i32) ref.null extern ref.is_null)", None),
        ("(func (result i32) i32.const 0 ref.is_null)", Some("type mismatch")),
        ("(func (result i32) ref.is_null)", Some("type mismatch")),
        ("(func (result i32) unreachable ref.is_null)", None),
        // ref.func needs a declared function.
        ("(func $f) (func (result funcref) ref.func $f)", Some("undeclared function reference")),
        ("(func (result funcref) ref.func 7)", Some("unknown function")),
        ("(func $f) (elem declare func $f) (func (result funcref) ref.func $f)", None),
        ("(func $f) (export \"f\" (func $f)) (func (result funcref) ref.func $f)", None),
        ("(func $f) (table 1 funcref) (elem (i32.const 0) $f) (func (result funcref) ref.func $f)", None),
        ("(func $f) (elem funcref (ref.func $f)) (func (result funcref) ref.func $f)", None),
        ("(func $f) (global funcref (ref.func $f)) (func (result funcref) ref.func $f)", None),
        ("(import \"m\" \"f\" (func $f)) (func (result funcref) ref.func $f)", Some("undeclared function reference")),
        ("(import \"m\" \"f\" (func $f)) (elem declare func $f) (func (result funcref) ref.func $f)", None),
        // A function may reference itself once declared.
        ("(func $f (result funcref) ref.func $f) (elem declare func $f)", None),
        ("(func $f (result funcref) ref.func $f)", Some("undeclared function reference")),
        ("(func $f) (elem declare func $f) (func (result externref) ref.func $f)", Some("type mismatch")),
    ]);
}

// ---------------------------------------------------------------------------
// Variables
// ---------------------------------------------------------------------------

#[test]
fn locals() {
    table(&[
        ("(func (param i32) (result i32) local.get 0)", None),
        ("(func (param i32) (result i64) (local i64) local.get 1)", None),
        ("(func (param i32) (result i32) (local i64) local.get 1)", Some("type mismatch")),
        ("(func (param i32) (result i32) local.get 1)", Some("unknown local")),
        ("(func local.get 0)", Some("unknown local")),
        ("(func (local i32) local.get 1)", Some("unknown local")),
        ("(func (local i32) i32.const 0 local.set 0)", None),
        ("(func (local i32) i64.const 0 local.set 0)", Some("type mismatch")),
        ("(func (local i32) local.set 0)", Some("type mismatch")),
        ("(func (local i32) i32.const 0 local.set 1)", Some("unknown local")),
        ("(func (result i32) (local i32) i32.const 0 local.tee 0)", None),
        ("(func (result i32) (local i32) i64.const 0 local.tee 0)", Some("type mismatch")),
        ("(func (result i64) (local i32) i32.const 0 local.tee 0)", Some("type mismatch")),
        ("(func (result i32) (local i32) i32.const 0 local.tee 1)", Some("unknown local")),
        ("(func (result funcref) (local funcref) local.get 0)", None),
        ("(func (result funcref) (local externref) local.get 0)", Some("type mismatch")),
        ("(func (local i32) local.get 4294967295 drop)", Some("unknown local")),
    ]);
}

#[test]
fn globals_in_function_bodies() {
    table(&[
        ("(global i32 (i32.const 0)) (func (result i32) global.get 0)", None),
        ("(global i32 (i32.const 0)) (func (result i64) global.get 0)", Some("type mismatch")),
        ("(global i32 (i32.const 0)) (func (result i32) global.get 1)", Some("unknown global")),
        ("(func (result i32) global.get 0)", Some("unknown global")),
        ("(global (mut i32) (i32.const 0)) (func i32.const 1 global.set 0)", None),
        ("(global (mut i32) (i32.const 0)) (func i64.const 1 global.set 0)", Some("type mismatch")),
        ("(global (mut i32) (i32.const 0)) (func global.set 0)", Some("type mismatch")),
        ("(global i32 (i32.const 0)) (func i32.const 1 global.set 0)", Some("global is immutable")),
        ("(global (mut i32) (i32.const 0)) (func i32.const 1 global.set 1)", Some("unknown global")),
        ("(import \"m\" \"g\" (global i32)) (func i32.const 1 global.set 0)", Some("global is immutable")),
        ("(import \"m\" \"g\" (global (mut i32))) (func i32.const 1 global.set 0)", None),
        ("(import \"m\" \"g\" (global (mut i32))) (global i64 (i64.const 0)) (func (result i64) global.get 1)", None),
        (
            "(import \"m\" \"g\" (global (mut i32))) (global i64 (i64.const 0)) (func (result i32) global.get 1)",
            Some("type mismatch"),
        ),
    ]);
}

// ---------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------

#[test]
fn calls() {
    table(&[
        ("(func $f (param i32) (result i64) i64.const 0) (func (result i64) i32.const 1 call $f)", None),
        ("(func $f (param i32) (result i64) i64.const 0) (func (result i64) call $f)", Some("type mismatch")),
        ("(func $f (param i32) (result i64) i64.const 0) (func (result i64) i64.const 1 call $f)", Some("type mismatch")),
        ("(func $f (param i32) (result i64) i64.const 0) (func (result i32) i32.const 1 call $f)", Some("type mismatch")),
        ("(func call 1)", Some("unknown function")),
        ("(import \"m\" \"f\" (func (param i32 i32) (result i32))) (func (result i32) i32.const 1 i32.const 2 call 0)", None),
        (
            "(import \"m\" \"f\" (func (param i32 i32) (result i32))) (func (result i32) i32.const 1 call 0)",
            Some("type mismatch"),
        ),
        ("(func $f (result i32 i64) i32.const 0 i64.const 0) (func (result i32 i64) call $f)", None),
        ("(func $f (result i32 i64) i32.const 0 i64.const 0) (func (result i32) call $f drop)", None),
        ("(func $f (result i32 i64) i32.const 0 i64.const 0) (func (result i64) call $f drop)", Some("type mismatch")),
    ]);
}

#[test]
fn call_indirect() {
    table(&[
        (
            "(type $t (func (param i32) (result i32))) (table 1 funcref) (func (result i32) i32.const 1 i32.const 0 call_indirect (type $t))",
            None,
        ),
        (
            "(type $t (func (param i32) (result i32))) (table 1 funcref) (func (result i32) i32.const 0 call_indirect (type $t))",
            Some("type mismatch"),
        ),
        (
            "(type $t (func (param i32) (result i32))) (table 1 funcref) (func (result i32) i32.const 1 i64.const 0 call_indirect (type $t))",
            Some("type mismatch"),
        ),
        (
            "(type $t (func (param i32) (result i32))) (table 1 funcref) (func (result i64) i32.const 1 i32.const 0 call_indirect (type $t))",
            Some("type mismatch"),
        ),
        ("(type $t (func)) (func i32.const 0 call_indirect (type $t))", Some("unknown table")),
        ("(type $t (func)) (table 1 funcref) (func i32.const 0 call_indirect 1 (type $t))", Some("unknown table")),
        ("(type $t (func)) (table 1 funcref) (func i32.const 0 call_indirect (type 1))", Some("unknown type")),
        ("(type $t (func)) (table 1 externref) (func i32.const 0 call_indirect (type $t))", Some("type mismatch")),
        ("(type $t (func)) (table 1 externref) (table $f 1 funcref) (func i32.const 0 call_indirect $f (type $t))", None),
        ("(type $t (func)) (import \"m\" \"t\" (table 1 funcref)) (func i32.const 0 call_indirect (type $t))", None),
        (
            "(type $t (func (result i32 i64))) (table 1 funcref) (func (result i32 i64) i32.const 0 call_indirect (type $t))",
            None,
        ),
    ]);
}

// ---------------------------------------------------------------------------
// Memory instructions
// ---------------------------------------------------------------------------

#[test]
fn memory_instructions() {
    table(&[
        ("(memory 1) (func (result i32) i32.const 0 i32.load)", None),
        ("(memory 1) (func (result i64) i32.const 0 i64.load)", None),
        ("(memory 1) (func (result f32) i32.const 0 f32.load)", None),
        ("(memory 1) (func (result f64) i32.const 0 f64.load)", None),
        ("(memory 1) (func (result i32) i32.const 0 i32.load8_s)", None),
        ("(memory 1) (func (result i64) i32.const 0 i64.load32_u)", None),
        ("(memory 1) (func (result i32) i64.const 0 i32.load)", Some("type mismatch")),
        ("(memory 1) (func (result i64) i32.const 0 i32.load)", Some("type mismatch")),
        ("(memory 1) (func (result i32) i32.load)", Some("type mismatch")),
        ("(func (result i32) i32.const 0 i32.load)", Some("unknown memory")),
        ("(memory 1) (func i32.const 0 i32.const 1 i32.store)", None),
        ("(memory 1) (func i32.const 0 i64.const 1 i64.store)", None),
        ("(memory 1) (func i32.const 0 f64.const 1 f64.store)", None),
        ("(memory 1) (func i32.const 0 i64.const 1 i64.store8)", None),
        ("(memory 1) (func i32.const 0 i64.const 1 i32.store)", Some("type mismatch")),
        ("(memory 1) (func i64.const 0 i32.const 1 i32.store)", Some("type mismatch")),
        ("(memory 1) (func i32.const 1 i32.store)", Some("type mismatch")),
        ("(func i32.const 0 i32.const 1 i32.store)", Some("unknown memory")),
        ("(memory 1) (func (result i32) memory.size)", None),
        ("(func (result i32) memory.size)", Some("unknown memory")),
        ("(memory 1) (func (result i32) i32.const 1 memory.grow)", None),
        ("(memory 1) (func (result i32) i64.const 1 memory.grow)", Some("type mismatch")),
        ("(memory 1) (func (result i32) memory.grow)", Some("type mismatch")),
        ("(func (result i32) i32.const 1 memory.grow)", Some("unknown memory")),
        ("(import \"m\" \"mem\" (memory 1)) (func (result i32) i32.const 0 i32.load)", None),
        ("(memory 1) (func (result i32) i32.const 0 i32.load offset=4294967295)", None),
    ]);
}

#[test]
fn alignment_must_not_exceed_natural() {
    table(&[
        ("(memory 1) (func (result i32) i32.const 0 i32.load align=4)", None),
        ("(memory 1) (func (result i32) i32.const 0 i32.load align=8)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func (result i64) i32.const 0 i64.load align=8)", None),
        ("(memory 1) (func (result i64) i32.const 0 i64.load align=16)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func (result f32) i32.const 0 f32.load align=4)", None),
        ("(memory 1) (func (result f32) i32.const 0 f32.load align=8)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func (result f64) i32.const 0 f64.load align=8)", None),
        ("(memory 1) (func (result f64) i32.const 0 f64.load align=16)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func (result i32) i32.const 0 i32.load8_s align=1)", None),
        ("(memory 1) (func (result i32) i32.const 0 i32.load8_u align=2)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func (result i32) i32.const 0 i32.load16_s align=2)", None),
        ("(memory 1) (func (result i32) i32.const 0 i32.load16_u align=4)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func (result i64) i32.const 0 i64.load8_s align=2)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func (result i64) i32.const 0 i64.load16_u align=4)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func (result i64) i32.const 0 i64.load32_s align=4)", None),
        ("(memory 1) (func (result i64) i32.const 0 i64.load32_u align=8)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func i32.const 0 i32.const 0 i32.store align=4)", None),
        ("(memory 1) (func i32.const 0 i32.const 0 i32.store align=8)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func i32.const 0 i64.const 0 i64.store align=16)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func i32.const 0 f32.const 0 f32.store align=8)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func i32.const 0 f64.const 0 f64.store align=16)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func i32.const 0 i32.const 0 i32.store8 align=2)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func i32.const 0 i32.const 0 i32.store16 align=4)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func i32.const 0 i64.const 0 i64.store8 align=2)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func i32.const 0 i64.const 0 i64.store16 align=4)", Some("alignment must not be larger than natural")),
        ("(memory 1) (func i32.const 0 i64.const 0 i64.store32 align=4)", None),
        ("(memory 1) (func i32.const 0 i64.const 0 i64.store32 align=8)", Some("alignment must not be larger than natural")),
        // The memory check comes first.
        ("(func (result i32) i32.const 0 i32.load align=8)", Some("unknown memory")),
    ]);
}

#[test]
fn bulk_memory() {
    table(&[
        ("(memory 1) (func i32.const 0 i32.const 0 i32.const 0 memory.copy)", None),
        ("(memory 1) (func i32.const 0 i32.const 0 memory.copy)", Some("type mismatch")),
        ("(memory 1) (func i32.const 0 i32.const 0 i64.const 0 memory.copy)", Some("type mismatch")),
        ("(func i32.const 0 i32.const 0 i32.const 0 memory.copy)", Some("unknown memory")),
        ("(memory 1) (func i32.const 0 i32.const 0 i32.const 0 memory.fill)", None),
        ("(memory 1) (func i32.const 0 i64.const 0 i32.const 0 memory.fill)", Some("type mismatch")),
        ("(func i32.const 0 i32.const 0 i32.const 0 memory.fill)", Some("unknown memory")),
        ("(memory 1) (data \"x\") (func i32.const 0 i32.const 0 i32.const 0 memory.init 0)", None),
        ("(memory 1) (data \"x\") (func i32.const 0 i32.const 0 i32.const 0 memory.init 1)", Some("unknown data segment")),
        ("(memory 1) (func i32.const 0 i32.const 0 i32.const 0 memory.init 0)", Some("unknown data segment")),
        ("(data \"x\") (func i32.const 0 i32.const 0 i32.const 0 memory.init 0)", Some("unknown memory")),
        ("(memory 1) (data \"x\") (func i32.const 0 i32.const 0 memory.init 0)", Some("type mismatch")),
        ("(memory 1) (data \"x\") (func data.drop 0)", None),
        ("(memory 1) (data \"x\") (func data.drop 1)", Some("unknown data segment")),
        ("(func data.drop 0)", Some("unknown data segment")),
        // data.drop does not need a memory.
        ("(data \"x\") (func data.drop 0)", None),
        // Active segments count too.
        ("(memory 1) (data (i32.const 0) \"x\") (data \"y\") (func data.drop 1)", None),
    ]);
}

// ---------------------------------------------------------------------------
// Table instructions
// ---------------------------------------------------------------------------

#[test]
fn table_instructions() {
    table(&[
        ("(table 1 funcref) (func (result funcref) i32.const 0 table.get 0)", None),
        ("(table 1 externref) (func (result externref) i32.const 0 table.get 0)", None),
        ("(table 1 externref) (func (result funcref) i32.const 0 table.get 0)", Some("type mismatch")),
        ("(table 1 funcref) (func (result funcref) i64.const 0 table.get 0)", Some("type mismatch")),
        ("(table 1 funcref) (func (result funcref) table.get 0)", Some("type mismatch")),
        ("(func (result funcref) i32.const 0 table.get 0)", Some("unknown table")),
        ("(table 1 funcref) (func (result funcref) i32.const 0 table.get 1)", Some("unknown table")),
        ("(table 1 funcref) (func i32.const 0 ref.null func table.set 0)", None),
        ("(table 1 externref) (func i32.const 0 ref.null extern table.set 0)", None),
        ("(table 1 funcref) (func i32.const 0 ref.null extern table.set 0)", Some("type mismatch")),
        ("(table 1 funcref) (func ref.null func table.set 0)", Some("type mismatch")),
        ("(table 1 funcref) (func i64.const 0 ref.null func table.set 0)", Some("type mismatch")),
        ("(func i32.const 0 ref.null func table.set 0)", Some("unknown table")),
        ("(table 1 funcref) (func (result i32) table.size 0)", None),
        ("(func (result i32) table.size 0)", Some("unknown table")),
        ("(table 1 funcref) (func (result i32) ref.null func i32.const 1 table.grow 0)", None),
        ("(table 1 externref) (func (result i32) ref.null extern i32.const 1 table.grow 0)", None),
        ("(table 1 funcref) (func (result i32) ref.null extern i32.const 1 table.grow 0)", Some("type mismatch")),
        ("(table 1 funcref) (func (result i32) i32.const 1 ref.null func table.grow 0)", Some("type mismatch")),
        ("(table 1 funcref) (func (result i32) i32.const 1 table.grow 0)", Some("type mismatch")),
        ("(func (result i32) ref.null func i32.const 1 table.grow 0)", Some("unknown table")),
        ("(table 1 funcref) (func i32.const 0 ref.null func i32.const 1 table.fill 0)", None),
        ("(table 1 funcref) (func i32.const 0 ref.null extern i32.const 1 table.fill 0)", Some("type mismatch")),
        ("(table 1 funcref) (func ref.null func i32.const 1 table.fill 0)", Some("type mismatch")),
        ("(table 1 funcref) (func i32.const 0 ref.null func i64.const 1 table.fill 0)", Some("type mismatch")),
        ("(func i32.const 0 ref.null func i32.const 1 table.fill 0)", Some("unknown table")),
        // Multiple tables, imported tables.
        ("(import \"m\" \"t\" (table 1 funcref)) (table 1 externref) (func (result externref) i32.const 0 table.get 1)", None),
        (
            "(import \"m\" \"t\" (table 1 funcref)) (table 1 externref) (func (result funcref) i32.const 0 table.get 1)",
            Some("type mismatch"),
        ),
    ]);
}

#[test]
fn bulk_table() {
    table(&[
        ("(table 1 funcref) (elem funcref (ref.null func)) (func i32.const 0 i32.const 0 i32.const 0 table.init 0)", None),
        ("(table 1 funcref) (elem func) (func i32.const 0 i32.const 0 i32.const 0 table.init 0)", None),
        (
            "(table 1 funcref) (elem funcref (ref.null func)) (func i32.const 0 i32.const 0 i32.const 0 table.init 1)",
            Some("unknown elem segment"),
        ),
        ("(table 1 funcref) (func i32.const 0 i32.const 0 i32.const 0 table.init 0)", Some("unknown elem segment")),
        ("(elem funcref (ref.null func)) (func i32.const 0 i32.const 0 i32.const 0 table.init 0)", Some("unknown table")),
        (
            "(table 1 funcref) (elem funcref (ref.null func)) (func i32.const 0 i32.const 0 i32.const 0 table.init 1 0)",
            Some("unknown table"),
        ),
        (
            "(table 1 externref) (elem funcref (ref.null func)) (func i32.const 0 i32.const 0 i32.const 0 table.init 0)",
            Some("type mismatch"),
        ),
        (
            "(table 1 externref) (elem externref (ref.null extern)) (func i32.const 0 i32.const 0 i32.const 0 table.init 0)",
            None,
        ),
        ("(table 1 funcref) (elem funcref (ref.null func)) (func i32.const 0 i32.const 0 table.init 0)", Some("type mismatch")),
        (
            "(table 1 funcref) (elem funcref (ref.null func)) (func i32.const 0 i64.const 0 i32.const 0 table.init 0)",
            Some("type mismatch"),
        ),
        ("(elem funcref (ref.null func)) (func elem.drop 0)", None),
        ("(elem funcref (ref.null func)) (func elem.drop 1)", Some("unknown elem segment")),
        ("(func elem.drop 0)", Some("unknown elem segment")),
        ("(elem declare func) (func elem.drop 0)", None),
        ("(table 1 funcref) (func i32.const 0 i32.const 0 i32.const 0 table.copy)", None),
        ("(table 1 funcref) (table 1 funcref) (func i32.const 0 i32.const 0 i32.const 0 table.copy 0 1)", None),
        (
            "(table 1 funcref) (table 1 externref) (func i32.const 0 i32.const 0 i32.const 0 table.copy 0 1)",
            Some("type mismatch"),
        ),
        ("(table 1 funcref) (func i32.const 0 i32.const 0 i32.const 0 table.copy 0 1)", Some("unknown table")),
        ("(table 1 funcref) (func i32.const 0 i32.const 0 i32.const 0 table.copy 1 0)", Some("unknown table")),
        ("(func i32.const 0 i32.const 0 i32.const 0 table.copy)", Some("unknown table")),
        ("(table 1 funcref) (func i32.const 0 i32.const 0 table.copy)", Some("type mismatch")),
        ("(table 1 funcref) (func i32.const 0 i32.const 0 f32.const 0 table.copy)", Some("type mismatch")),
    ]);
}

// ---------------------------------------------------------------------------
// Module-level: constant expressions, globals, elements, data
// ---------------------------------------------------------------------------

#[test]
fn global_initializers() {
    table(&[
        ("(global i32 (i32.const 1))", None),
        ("(global i64 (i64.const 1))", None),
        ("(global f32 (f32.const 1))", None),
        ("(global f64 (f64.const 1))", None),
        ("(global funcref (ref.null func))", None),
        ("(global externref (ref.null extern))", None),
        ("(global (mut i32) (i32.const 1))", None),
        ("(global i32 (i64.const 1))", Some("type mismatch")),
        ("(global i64 (i32.const 1))", Some("type mismatch")),
        ("(global f32 (f64.const 1))", Some("type mismatch")),
        ("(global funcref (ref.null extern))", Some("type mismatch")),
        ("(global externref (ref.null func))", Some("type mismatch")),
        ("(global i32 (i32.const 1) (i32.const 2))", Some("type mismatch")),
        ("(global i32 (nop))", Some("constant expression required")),
        ("(global i32 (i32.const 1) (nop))", Some("constant expression required")),
        ("(global i32 (i32.const 1) (i32.const 2) (i32.add))", Some("constant expression required")),
        ("(global i32 (unreachable))", Some("constant expression required")),
        ("(global i32 (local.get 0))", Some("constant expression required")),
        ("(global i32 (block (result i32) (i32.const 1)))", Some("constant expression required")),
        ("(func $f) (global funcref (ref.func $f))", None),
        ("(global funcref (ref.func 0))", Some("unknown function")),
        // global.get: imported, immutable globals only.
        ("(import \"m\" \"g\" (global i32)) (global i32 (global.get 0))", None),
        ("(import \"m\" \"g\" (global i64)) (global i32 (global.get 0))", Some("type mismatch")),
        ("(import \"m\" \"g\" (global (mut i32))) (global i32 (global.get 0))", Some("constant expression required")),
        ("(global i32 (i32.const 0)) (global i32 (global.get 0))", Some("unknown global")),
        ("(global i32 (global.get 0))", Some("unknown global")),
        ("(import \"m\" \"g\" (global i32)) (global i32 (global.get 1))", Some("unknown global")),
        ("(import \"m\" \"g\" (global i32)) (global i32 (global.get 0)) (global i32 (global.get 1))", Some("unknown global")),
        ("(import \"m\" \"g\" (global funcref)) (global funcref (global.get 0))", None),
    ]);
}

#[test]
fn element_segments() {
    table(&[
        ("(table 1 funcref) (func $f) (elem (i32.const 0) $f)", None),
        ("(table 1 funcref) (func $f) (elem (i32.const 0) func $f)", None),
        ("(table 1 funcref) (func $f) (elem (i32.const 0) funcref (ref.func $f) (ref.null func))", None),
        ("(table 1 externref) (elem (i32.const 0) externref (ref.null extern))", None),
        ("(table $t 1 funcref) (table 1 funcref) (func $f) (elem (table 1) (i32.const 0) func $f)", None),
        ("(table 1 funcref) (elem (i32.const 0) 0)", Some("unknown function")),
        ("(table 1 funcref) (elem (i32.const 0) funcref (ref.func 0))", Some("unknown function")),
        ("(func $f) (elem (i32.const 0) $f)", Some("unknown table")),
        ("(table 1 funcref) (func $f) (elem (table 1) (i32.const 0) func $f)", Some("unknown table")),
        ("(table 1 funcref) (func $f) (elem (i64.const 0) $f)", Some("type mismatch")),
        ("(table 1 funcref) (func $f) (elem (offset (i32.const 0) (i32.const 0)) $f)", Some("type mismatch")),
        ("(table 1 funcref) (func $f) (elem (nop) $f)", Some("constant expression required")),
        ("(table 1 funcref) (func $f) (elem (offset (i32.const 0) (nop)) $f)", Some("constant expression required")),
        ("(table 1 externref) (func $f) (elem (i32.const 0) $f)", Some("type mismatch")),
        ("(table 1 externref) (elem (i32.const 0) funcref (ref.null func))", Some("type mismatch")),
        ("(table 1 funcref) (elem (i32.const 0) externref (ref.null extern))", Some("type mismatch")),
        ("(table 1 funcref) (elem (i32.const 0) funcref (ref.null extern))", Some("type mismatch")),
        ("(table 1 funcref) (elem (i32.const 0) funcref (i32.const 0))", Some("type mismatch")),
        ("(table 1 funcref) (elem (i32.const 0) funcref (nop))", Some("constant expression required")),
        ("(import \"m\" \"g\" (global i32)) (table 1 funcref) (elem (global.get 0))", None),
        (
            "(import \"m\" \"g\" (global (mut i32))) (table 1 funcref) (elem (global.get 0))",
            Some("constant expression required"),
        ),
        ("(global i32 (i32.const 0)) (table 1 funcref) (elem (global.get 0))", Some("unknown global")),
        // Passive and declarative segments need no table.
        ("(elem funcref (ref.null func))", None),
        ("(elem externref (ref.null extern))", None),
        ("(func $f) (elem declare func $f)", None),
        ("(elem declare func 0)", Some("unknown function")),
        ("(elem funcref (ref.func 0))", Some("unknown function")),
        ("(elem funcref (ref.null extern))", Some("type mismatch")),
        ("(import \"m\" \"t\" (table 1 funcref)) (func $f) (elem (i32.const 0) $f)", None),
    ]);
}

#[test]
fn data_segments() {
    table(&[
        ("(memory 1) (data (i32.const 0) \"abc\")", None),
        ("(memory 1) (data (memory 0) (i32.const 0) \"abc\")", None),
        ("(memory 1) (data \"abc\")", None),
        ("(data \"abc\")", None),
        ("(data (i32.const 0) \"abc\")", Some("unknown memory")),
        ("(memory 1) (data (memory 1) (i32.const 0) \"abc\")", Some("unknown memory")),
        ("(memory 1) (data (i64.const 0) \"abc\")", Some("type mismatch")),
        ("(memory 1) (data (f32.const 0) \"abc\")", Some("type mismatch")),
        ("(memory 1) (data (offset (i32.const 0) (i32.const 0)) \"abc\")", Some("type mismatch")),
        ("(memory 1) (data (nop) \"abc\")", Some("constant expression required")),
        ("(memory 1) (data (offset (i32.const 0) (nop)) \"abc\")", Some("constant expression required")),
        ("(memory 1) (data (ref.null func) \"abc\")", Some("type mismatch")),
        ("(import \"m\" \"g\" (global i32)) (memory 1) (data (global.get 0) \"abc\")", None),
        ("(import \"m\" \"g\" (global i64)) (memory 1) (data (global.get 0) \"abc\")", Some("type mismatch")),
        (
            "(import \"m\" \"g\" (global (mut i32))) (memory 1) (data (global.get 0) \"abc\")",
            Some("constant expression required"),
        ),
        ("(global i32 (i32.const 0)) (memory 1) (data (global.get 0) \"abc\")", Some("unknown global")),
        ("(import \"m\" \"mem\" (memory 1)) (data (i32.const 0) \"abc\")", None),
    ]);
}

// ---------------------------------------------------------------------------
// Module-level: imports, exports, start, limits
// ---------------------------------------------------------------------------

#[test]
fn imports_and_exports() {
    table(&[
        ("(import \"m\" \"f\" (func (type 0))) (type (func))", None),
        ("(import \"m\" \"f\" (func (type 1))) (type (func))", Some("unknown type")),
        ("(type (func)) (func (type 1))", Some("unknown type")),
        ("(import \"m\" \"t\" (table 1 funcref)) (import \"m\" \"u\" (table 1 externref))", None),
        ("(import \"m\" \"g\" (global i32)) (import \"m\" \"h\" (global (mut f64)))", None),
        ("(func) (export \"f\" (func 0))", None),
        ("(func) (export \"f\" (func 1))", Some("unknown function")),
        ("(export \"f\" (func 0))", Some("unknown function")),
        ("(table 1 funcref) (export \"t\" (table 0))", None),
        ("(table 1 funcref) (export \"t\" (table 1))", Some("unknown table")),
        ("(memory 1) (export \"m\" (memory 0))", None),
        ("(memory 1) (export \"m\" (memory 1))", Some("unknown memory")),
        ("(export \"m\" (memory 0))", Some("unknown memory")),
        ("(global i32 (i32.const 0)) (export \"g\" (global 0))", None),
        ("(global i32 (i32.const 0)) (export \"g\" (global 1))", Some("unknown global")),
        ("(import \"m\" \"f\" (func)) (export \"f\" (func 0))", None),
        ("(import \"m\" \"g\" (global (mut i32))) (export \"g\" (global 0))", None),
        ("(func) (export \"a\" (func 0)) (export \"b\" (func 0))", None),
        ("(func) (export \"a\" (func 0)) (export \"a\" (func 0))", Some("duplicate export name")),
        ("(func) (memory 1) (export \"a\" (func 0)) (export \"a\" (memory 0))", Some("duplicate export name")),
        ("(func) (export \"\" (func 0)) (export \"\" (func 0))", Some("duplicate export name")),
        ("(func) (export \"a\" (func 0)) (export \"b\" (func 0)) (export \"a\" (func 0))", Some("duplicate export name")),
    ]);
}

#[test]
fn start_function() {
    table(&[
        ("(func $s) (start $s)", None),
        ("(import \"m\" \"s\" (func $s)) (start $s)", None),
        ("(func $s (param i32)) (start $s)", Some("start function")),
        ("(func $s (result i32) i32.const 0) (start $s)", Some("start function")),
        ("(func $s (param i32) (result i32) local.get 0) (start $s)", Some("start function")),
        ("(start 0)", Some("unknown function")),
        ("(func) (start 1)", Some("unknown function")),
    ]);
}

#[test]
fn limits_and_memory_count() {
    table(&[
        ("(memory 0)", None),
        ("(memory 1 1)", None),
        ("(memory 0 65536)", None),
        ("(memory 65536)", None),
        ("(memory 65536 65536)", None),
        ("(memory 1 0)", Some("size minimum must not be greater than maximum")),
        ("(memory 65537)", Some("memory size must be at most 65536 pages (4GiB)")),
        ("(memory 0 65537)", Some("memory size must be at most 65536 pages (4GiB)")),
        ("(memory 2147483648)", Some("memory size must be at most 65536 pages (4GiB)")),
        ("(memory 0 4294967295)", Some("memory size must be at most 65536 pages (4GiB)")),
        ("(import \"m\" \"mem\" (memory 1 0))", Some("size minimum must not be greater than maximum")),
        ("(import \"m\" \"mem\" (memory 65537))", Some("memory size must be at most 65536 pages (4GiB)")),
        ("(memory 0) (memory 0)", Some("multiple memories")),
        ("(import \"m\" \"mem\" (memory 0)) (memory 0)", Some("multiple memories")),
        ("(import \"m\" \"a\" (memory 0)) (import \"m\" \"b\" (memory 0))", Some("multiple memories")),
        ("(table 0 funcref)", None),
        ("(table 1 1 funcref)", None),
        ("(table 0 4294967295 funcref)", None),
        ("(table 4294967295 externref)", None),
        ("(table 1 0 funcref)", Some("size minimum must not be greater than maximum")),
        ("(table 2 1 externref)", Some("size minimum must not be greater than maximum")),
        ("(import \"m\" \"t\" (table 1 0 funcref))", Some("size minimum must not be greater than maximum")),
        // Several tables are fine (reference types).
        ("(table 1 funcref) (table 1 externref) (table 0 funcref)", None),
    ]);
}

// ---------------------------------------------------------------------------
// Error locations
// ---------------------------------------------------------------------------

#[test]
fn errors_carry_function_and_instruction() {
    invalid_at("(func i32.const 1 i64.const 2 i32.add)", "type mismatch", 0, 2);
    invalid_at("(func) (func nop local.get 0)", "unknown local", 1, 1);
    invalid_at(
        "(import \"m\" \"f\" (func)) (import \"m\" \"g\" (func)) (func) (func block block br 3 end end)",
        "unknown label",
        3,
        2,
    );
    invalid_at("(func (result i32) i32.const 0 call 4)", "unknown function", 0, 1);
    invalid_at("(global (mut i32) (i32.const 0)) (func nop nop i32.const 0 global.set 1)", "unknown global", 0, 3);
    invalid_at("(memory 1) (func nop i32.const 0 i32.load align=8 drop)", "alignment must not be larger than natural", 0, 2);
    // The function's own `end` is where a missing result is detected.
    invalid_at("(func (result i32) nop)", "type mismatch", 0, 1);
    invalid_at("(func (result i32) block (result i32) i64.const 0 end)", "type mismatch", 0, 2);

    // Module-level errors have no location.
    match check("(memory 1 0)") {
        Err(Error::Invalid { func, instr, .. }) => assert_eq!((func, instr), (None, None)),
        other => panic!("unexpected {other:?}"),
    }
    // A bad type index on a defined function names the function.
    match check("(type (func)) (import \"m\" \"f\" (func)) (func (type 1))") {
        Err(Error::Invalid { msg, func, instr, .. }) => {
            assert_eq!(msg, "unknown type");
            assert_eq!((func, instr), (Some(1), None));
        }
        other => panic!("unexpected {other:?}"),
    }

    let e = check("(func i32.const 1 i64.const 2 i32.add)").unwrap_err();
    assert_eq!(e.to_string(), "invalid module: type mismatch (function 0, instruction 2)");
}

// ---------------------------------------------------------------------------
// Realistic modules
// ---------------------------------------------------------------------------

#[test]
fn rust_fixture_validates() {
    let bytes = include_bytes!("fixtures/rust_fixture.wasm");
    Module::new(bytes).expect("the Rust-built fixture is valid");
}

#[test]
fn realistic_module_validates() {
    valid(
        r#"
        (type $t (func (param i32 i32) (result i32)))
        (import "env" "log" (func $log (param i32 i32)))
        (import "env" "g" (global $g (mut i32)))
        (memory 1 4)
        (table 2 funcref)
        (global $h i32 (i32.const 1024))
        (data (i32.const 0) "hello")
        (data "passive")
        (elem (i32.const 0) $add $fact)
        (elem declare func $add)
        (export "add" (func $add))
        (export "memory" (memory 0))
        (start $start)
        (func $add (type $t) local.get 0 local.get 1 i32.add)
        (func $fact (param i64) (result i64)
          (local i64)
          i64.const 1 local.set 1
          block
            loop
              local.get 0 i64.eqz br_if 1
              local.get 0 local.get 1 i64.mul local.set 1
              local.get 0 i64.const 1 i64.sub local.set 0
              br 0
            end
          end
          local.get 1)
        (func $start
          (local $i i32)
          i32.const 0 i32.const 5 i32.const 0 memory.init 1
          i32.const 16 i32.const 0 i32.const 8 memory.copy
          i32.const 32 i32.const 0 i32.const 8 memory.fill
          data.drop 1
          i32.const 1 i32.const 2
          i32.const 0 call_indirect (type $t)
          global.get $g i32.add
          local.tee $i
          i32.const 3
          i32.const 1 i32.const 2 i32.const 0 select
          i32.add
          global.set $g
          local.get $i
          ref.func $add ref.is_null
          i32.add
          i32.const 0 i32.store
          i32.const 0 i32.load8_u
          if (result i32) i32.const 1 else i32.const 2 end
          block i32.const 0 br_table 0 0 end
          i32.add drop
          i32.const 0 i32.const 1 i32.const 1 table.init 0
          i32.const 0 i32.const 0 i32.const 1 table.copy
          elem.drop 0
          ref.null func i32.const 1 table.grow 0 drop
          table.size 0 drop
          i32.const 0 ref.null func i32.const 1 table.fill 0
          i32.const 0 i32.const 1 table.get 0 table.set 0
          (if (local.get $i) (then (local.get $i) (local.get $i) (call $log)) (else nop))
          )
        "#,
    );
}

// ---------------------------------------------------------------------------
// Robustness: the validator never panics on hand-built modules
// ---------------------------------------------------------------------------

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_valtype(rng: &mut XorShift) -> ValType {
    match rng.below(6) {
        0 => ValType::I32,
        1 => ValType::I64,
        2 => ValType::F32,
        3 => ValType::F64,
        4 => ValType::FuncRef,
        _ => ValType::ExternRef,
    }
}

fn random_instr(rng: &mut XorShift) -> Instr {
    use Instr::*;
    let idx = rng.below(6) as u32;
    let big = (rng.next() >> 16) as u32;
    let bt = match rng.below(3) {
        0 => BlockType::Empty,
        1 => BlockType::Val(random_valtype(rng)),
        _ => BlockType::Type(idx),
    };
    let ma = MemArg { align: (rng.next() >> 60) as u32, offset: big };
    match rng.below(64) {
        0 => Unreachable,
        1 => Nop,
        2 => Block { ty: bt, end: big },
        3 => Loop { ty: bt, end: big },
        4 => If { ty: bt, else_: big, end: big },
        5 => Else { end: big },
        6..=8 => End,
        9 => Br(idx),
        10 => BrIf(idx),
        11 => BrTable(idx),
        12 => Return,
        13 => Call(idx),
        14 => CallIndirect { ty: idx, table: idx },
        15 => Drop,
        16 => Select,
        17 => SelectT(random_valtype(rng)),
        18 => LocalGet(idx),
        19 => LocalSet(idx),
        20 => LocalTee(big),
        21 => GlobalGet(idx),
        22 => GlobalSet(idx),
        23 => RefNull(RefType::FuncRef),
        24 => RefIsNull,
        25 => RefFunc(idx),
        26 => TableGet(idx),
        27 => TableSet(idx),
        28 => TableInit { elem: idx, table: idx },
        29 => ElemDrop(idx),
        30 => TableCopy { dst: idx, src: idx },
        31 => TableGrow(idx),
        32 => TableSize(idx),
        33 => TableFill(idx),
        34 => I32Load(ma),
        35 => I64Load8S(ma),
        36 => F64Store(ma),
        37 => I32Store16(ma),
        38 => MemorySize,
        39 => MemoryGrow,
        40 => MemoryInit(idx),
        41 => DataDrop(idx),
        42 => MemoryCopy,
        43 => MemoryFill,
        44..=46 => I32Const(big as i32),
        47 => I64Const(big as i64),
        48 => F32Const(big),
        49 => F64Const(big as u64),
        50 => I32Add,
        51 => I64Mul,
        52 => F32Neg,
        53 => F64Lt,
        54 => I32Eqz,
        55 => I32WrapI64,
        56 => I64TruncSatF64U,
        57 => F32DemoteF64,
        58 => I32Extend8S,
        59 => I64ReinterpretF64,
        60 => I32Load(MemArg { align: u32::MAX, offset: u32::MAX }),
        61 => Br(big),
        62 => Call(big),
        _ => LocalGet(big),
    }
}

/// Random instruction sequences, with random nesting and random immediates
/// (including out-of-range ones the decoder would never produce), fed
/// straight to the validator.
#[test]
fn fuzz_hand_built_bodies_never_panic() {
    let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
    let mut accepted = 0;
    for _ in 0..3000 {
        let nparams = rng.below(3) as usize;
        let nresults = rng.below(3) as usize;
        let ty = FuncType {
            params: (0..nparams).map(|_| random_valtype(&mut rng)).collect(),
            results: (0..nresults).map(|_| random_valtype(&mut rng)).collect(),
        };
        let len = rng.below(24) as usize;
        let mut instrs: Vec<Instr> = (0..len).map(|_| random_instr(&mut rng)).collect();
        if rng.below(4) != 0 {
            instrs.push(Instr::End);
        }
        let module = Module {
            types: vec![ty, FuncType::default()],
            imports: vec![Import {
                module: "m".into(),
                name: "g".into(),
                desc: ImportDesc::Global(GlobalType { ty: ValType::I32, mutable: rng.below(2) == 0 }),
            }],
            funcs: vec![0, 1],
            tables: vec![kalium_wasm::TableType {
                elem: RefType::FuncRef,
                limits: kalium_wasm::ResizableLimits { min: 1, max: None },
            }],
            memories: if rng.below(2) == 0 {
                vec![kalium_wasm::MemType { limits: kalium_wasm::ResizableLimits { min: 1, max: None } }]
            } else {
                vec![]
            },
            globals: vec![Global {
                ty: GlobalType { ty: ValType::I64, mutable: true },
                init: ConstExpr { instrs: vec![Instr::I64Const(0)] },
            }],
            exports: vec![Export { name: "f".into(), desc: ExportDesc::Func(1) }],
            start: None,
            elements: vec![],
            data: vec![kalium_wasm::Data { mode: kalium_wasm::DataMode::Passive, bytes: vec![] }],
            data_count: Some(1),
            code: vec![
                FuncBody {
                    locals: (0..rng.below(3)).map(|_| random_valtype(&mut rng)).collect(),
                    instrs,
                    br_tables: vec![BrTable { targets: vec![0, rng.below(4) as u32], default: rng.below(3) as u32 }],
                },
                FuncBody { locals: vec![], instrs: vec![Instr::End], br_tables: vec![] },
            ],
            program: Default::default(),
        };
        if validate(&module).is_ok() {
            accepted += 1;
        }
    }
    // Some random bodies are valid (e.g. `unreachable end`); the point is
    // that no shape panics.
    assert!(accepted > 0);
}

/// Random corruption of real binaries: the decoder may or may not accept
/// the result, and whatever it produces the validator must not panic on.
#[test]
fn fuzz_corrupted_binaries_never_panic() {
    let seeds = [
        "(module (func (param i32) (result i32) local.get 0 if (result i32) i32.const 1 else i32.const 2 end))",
        "(module (memory 1) (data \"a\") (data \"b\") (func i32.const 0 i32.const 0 i32.const 1 memory.init 1 data.drop 0))",
        "(module (type $t (func)) (table 2 funcref) (elem (i32.const 0) 0 0) (func (type $t) i32.const 0 call_indirect (type $t)) (func block block i32.const 0 br_table 0 1 0 end end))",
        "(module (import \"a\" \"b\" (func)) (import \"a\" \"c\" (global i64)) (global f64 (f64.const 1)) (global i64 (global.get 0)) (export \"e\" (func 0)) (start 0) (elem declare func 0) (elem funcref (ref.func 0)))",
        "(module (func (result i32 i64) (local i32 i64 f32 f64 funcref externref) loop (result i32) i32.const 0 br_if 0 i32.const 1 end i64.const 0))",
    ];
    let mut rng = XorShift(0x0123_4567_89AB_CDEF);
    for seed in seeds {
        let bytes = wasm(seed);
        Module::new(&bytes).expect("seed is valid");
        for _ in 0..400 {
            let mut m = bytes.clone();
            let flips = 1 + rng.below(3) as usize;
            for _ in 0..flips {
                let i = rng.below(m.len() as u64) as usize;
                m[i] = (rng.next() >> 24) as u8;
            }
            let _ = Module::new(&m);
        }
    }
}
