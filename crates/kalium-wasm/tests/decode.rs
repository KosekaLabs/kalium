//! Decoder tests: every required feature through `wat`, a real Rust-built
//! module, error reporting, and a fuzz-style robustness test.

use kalium_wasm::decode::decode;
use kalium_wasm::instr::{BlockType, Instr, MemArg};
use kalium_wasm::module::{DataMode, ElemItems, ElemMode, ExportDesc, ImportDesc};
use kalium_wasm::types::{FuncType, GlobalType, RefType, ResizableLimits, ValType};
use kalium_wasm::{Error, Module};

const FIXTURE: &[u8] = include_bytes!("fixtures/rust_fixture.wasm");

fn wasm(wat: &str) -> Vec<u8> {
    wat::parse_str(wat).unwrap_or_else(|e| panic!("bad wat: {e}\n{wat}"))
}

/// Decodes without validating: these tests exercise the decoder on
/// instruction sequences that are not necessarily well-typed.
fn module(wat: &str) -> Module {
    let bytes = wasm(wat);
    decode(&bytes).unwrap_or_else(|e| panic!("decode failed: {e}\n{wat}"))
}

/// The instructions of the only defined function, without the trailing `end`.
fn body(wat: &str) -> Vec<Instr> {
    let m = module(wat);
    let b = m.code.last().expect("a function");
    let mut v = b.instrs.clone();
    assert_eq!(v.pop(), Some(Instr::End), "body ends with end");
    v
}

/// Builds a module from `(text, expected)` pairs and checks the decoded
/// sequence. `prelude` is spliced into the module before the function.
fn check_seq(prelude: &str, pairs: &[(&str, Instr)]) {
    let text: Vec<&str> = pairs.iter().map(|(t, _)| *t).collect();
    let wat = format!("(module {prelude} (func {}))", text.join(" "));
    let got = body(&wat);
    let want: Vec<Instr> = pairs.iter().map(|(_, i)| *i).collect();
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "instruction {i} ({})", pairs[i].0);
    }
    assert_eq!(got.len(), want.len());
}

fn ma(align: u32, offset: u32) -> MemArg {
    MemArg { align, offset }
}

// ---------------------------------------------------------------------------
// Instruction coverage
// ---------------------------------------------------------------------------

#[test]
fn numeric_i32_i64() {
    use Instr::*;
    check_seq(
        "",
        &[
            ("i32.const -5", I32Const(-5)),
            ("i64.const 1234567890123", I64Const(1234567890123)),
            ("i32.eqz", I32Eqz),
            ("i32.eq", I32Eq),
            ("i32.ne", I32Ne),
            ("i32.lt_s", I32LtS),
            ("i32.lt_u", I32LtU),
            ("i32.gt_s", I32GtS),
            ("i32.gt_u", I32GtU),
            ("i32.le_s", I32LeS),
            ("i32.le_u", I32LeU),
            ("i32.ge_s", I32GeS),
            ("i32.ge_u", I32GeU),
            ("i64.eqz", I64Eqz),
            ("i64.eq", I64Eq),
            ("i64.ne", I64Ne),
            ("i64.lt_s", I64LtS),
            ("i64.lt_u", I64LtU),
            ("i64.gt_s", I64GtS),
            ("i64.gt_u", I64GtU),
            ("i64.le_s", I64LeS),
            ("i64.le_u", I64LeU),
            ("i64.ge_s", I64GeS),
            ("i64.ge_u", I64GeU),
            ("i32.clz", I32Clz),
            ("i32.ctz", I32Ctz),
            ("i32.popcnt", I32Popcnt),
            ("i32.add", I32Add),
            ("i32.sub", I32Sub),
            ("i32.mul", I32Mul),
            ("i32.div_s", I32DivS),
            ("i32.div_u", I32DivU),
            ("i32.rem_s", I32RemS),
            ("i32.rem_u", I32RemU),
            ("i32.and", I32And),
            ("i32.or", I32Or),
            ("i32.xor", I32Xor),
            ("i32.shl", I32Shl),
            ("i32.shr_s", I32ShrS),
            ("i32.shr_u", I32ShrU),
            ("i32.rotl", I32Rotl),
            ("i32.rotr", I32Rotr),
            ("i64.clz", I64Clz),
            ("i64.ctz", I64Ctz),
            ("i64.popcnt", I64Popcnt),
            ("i64.add", I64Add),
            ("i64.sub", I64Sub),
            ("i64.mul", I64Mul),
            ("i64.div_s", I64DivS),
            ("i64.div_u", I64DivU),
            ("i64.rem_s", I64RemS),
            ("i64.rem_u", I64RemU),
            ("i64.and", I64And),
            ("i64.or", I64Or),
            ("i64.xor", I64Xor),
            ("i64.shl", I64Shl),
            ("i64.shr_s", I64ShrS),
            ("i64.shr_u", I64ShrU),
            ("i64.rotl", I64Rotl),
            ("i64.rotr", I64Rotr),
            ("i32.extend8_s", I32Extend8S),
            ("i32.extend16_s", I32Extend16S),
            ("i64.extend8_s", I64Extend8S),
            ("i64.extend16_s", I64Extend16S),
            ("i64.extend32_s", I64Extend32S),
        ],
    );
}

#[test]
fn numeric_float() {
    use Instr::*;
    check_seq(
        "",
        &[
            ("f32.const 1.5", F32Const(1.5f32.to_bits())),
            ("f64.const -2.25", F64Const((-2.25f64).to_bits())),
            ("f32.const nan:0x200000", F32Const(0x7fa0_0000)),
            ("f64.const -nan", F64Const(0xfff8_0000_0000_0000)),
            ("f32.eq", F32Eq),
            ("f32.ne", F32Ne),
            ("f32.lt", F32Lt),
            ("f32.gt", F32Gt),
            ("f32.le", F32Le),
            ("f32.ge", F32Ge),
            ("f64.eq", F64Eq),
            ("f64.ne", F64Ne),
            ("f64.lt", F64Lt),
            ("f64.gt", F64Gt),
            ("f64.le", F64Le),
            ("f64.ge", F64Ge),
            ("f32.abs", F32Abs),
            ("f32.neg", F32Neg),
            ("f32.ceil", F32Ceil),
            ("f32.floor", F32Floor),
            ("f32.trunc", F32Trunc),
            ("f32.nearest", F32Nearest),
            ("f32.sqrt", F32Sqrt),
            ("f32.add", F32Add),
            ("f32.sub", F32Sub),
            ("f32.mul", F32Mul),
            ("f32.div", F32Div),
            ("f32.min", F32Min),
            ("f32.max", F32Max),
            ("f32.copysign", F32Copysign),
            ("f64.abs", F64Abs),
            ("f64.neg", F64Neg),
            ("f64.ceil", F64Ceil),
            ("f64.floor", F64Floor),
            ("f64.trunc", F64Trunc),
            ("f64.nearest", F64Nearest),
            ("f64.sqrt", F64Sqrt),
            ("f64.add", F64Add),
            ("f64.sub", F64Sub),
            ("f64.mul", F64Mul),
            ("f64.div", F64Div),
            ("f64.min", F64Min),
            ("f64.max", F64Max),
            ("f64.copysign", F64Copysign),
        ],
    );
}

#[test]
fn conversions() {
    use Instr::*;
    check_seq(
        "",
        &[
            ("i32.wrap_i64", I32WrapI64),
            ("i32.trunc_f32_s", I32TruncF32S),
            ("i32.trunc_f32_u", I32TruncF32U),
            ("i32.trunc_f64_s", I32TruncF64S),
            ("i32.trunc_f64_u", I32TruncF64U),
            ("i64.extend_i32_s", I64ExtendI32S),
            ("i64.extend_i32_u", I64ExtendI32U),
            ("i64.trunc_f32_s", I64TruncF32S),
            ("i64.trunc_f32_u", I64TruncF32U),
            ("i64.trunc_f64_s", I64TruncF64S),
            ("i64.trunc_f64_u", I64TruncF64U),
            ("f32.convert_i32_s", F32ConvertI32S),
            ("f32.convert_i32_u", F32ConvertI32U),
            ("f32.convert_i64_s", F32ConvertI64S),
            ("f32.convert_i64_u", F32ConvertI64U),
            ("f32.demote_f64", F32DemoteF64),
            ("f64.convert_i32_s", F64ConvertI32S),
            ("f64.convert_i32_u", F64ConvertI32U),
            ("f64.convert_i64_s", F64ConvertI64S),
            ("f64.convert_i64_u", F64ConvertI64U),
            ("f64.promote_f32", F64PromoteF32),
            ("i32.reinterpret_f32", I32ReinterpretF32),
            ("i64.reinterpret_f64", I64ReinterpretF64),
            ("f32.reinterpret_i32", F32ReinterpretI32),
            ("f64.reinterpret_i64", F64ReinterpretI64),
            ("i32.trunc_sat_f32_s", I32TruncSatF32S),
            ("i32.trunc_sat_f32_u", I32TruncSatF32U),
            ("i32.trunc_sat_f64_s", I32TruncSatF64S),
            ("i32.trunc_sat_f64_u", I32TruncSatF64U),
            ("i64.trunc_sat_f32_s", I64TruncSatF32S),
            ("i64.trunc_sat_f32_u", I64TruncSatF32U),
            ("i64.trunc_sat_f64_s", I64TruncSatF64S),
            ("i64.trunc_sat_f64_u", I64TruncSatF64U),
        ],
    );
}

#[test]
fn memory_ops() {
    use Instr::*;
    check_seq(
        "(memory 1) (data \"abc\") (data \"def\")",
        &[
            ("i32.load", I32Load(ma(2, 0))),
            ("i64.load offset=8", I64Load(ma(3, 8))),
            ("f32.load align=1", F32Load(ma(0, 0))),
            ("f64.load offset=16 align=4", F64Load(ma(2, 16))),
            ("i32.load8_s", I32Load8S(ma(0, 0))),
            ("i32.load8_u offset=1", I32Load8U(ma(0, 1))),
            ("i32.load16_s", I32Load16S(ma(1, 0))),
            ("i32.load16_u", I32Load16U(ma(1, 0))),
            ("i64.load8_s", I64Load8S(ma(0, 0))),
            ("i64.load8_u", I64Load8U(ma(0, 0))),
            ("i64.load16_s", I64Load16S(ma(1, 0))),
            ("i64.load16_u", I64Load16U(ma(1, 0))),
            ("i64.load32_s", I64Load32S(ma(2, 0))),
            ("i64.load32_u offset=4294967295", I64Load32U(ma(2, u32::MAX))),
            ("i32.store", I32Store(ma(2, 0))),
            ("i64.store", I64Store(ma(3, 0))),
            ("f32.store", F32Store(ma(2, 0))),
            ("f64.store offset=3 align=1", F64Store(ma(0, 3))),
            ("i32.store8", I32Store8(ma(0, 0))),
            ("i32.store16", I32Store16(ma(1, 0))),
            ("i64.store8", I64Store8(ma(0, 0))),
            ("i64.store16", I64Store16(ma(1, 0))),
            ("i64.store32", I64Store32(ma(2, 0))),
            ("memory.size", MemorySize),
            ("memory.grow", MemoryGrow),
            ("memory.init 1", MemoryInit(1)),
            ("data.drop 0", DataDrop(0)),
            ("memory.copy", MemoryCopy),
            ("memory.fill", MemoryFill),
        ],
    );
}

#[test]
fn variable_parametric_reference_table_ops() {
    use Instr::*;
    check_seq(
        "(type (func)) (table 2 funcref) (table 3 externref) (global (mut i32) (i32.const 0)) \
         (elem func 0) (func $f)",
        &[
            ("local.get 0", LocalGet(0)),
            ("local.set 1", LocalSet(1)),
            ("local.tee 300", LocalTee(300)),
            ("global.get 0", GlobalGet(0)),
            ("global.set 0", GlobalSet(0)),
            ("drop", Drop),
            ("select", Select),
            ("select (result i64)", SelectT(ValType::I64)),
            ("select (result funcref)", SelectT(ValType::FuncRef)),
            ("ref.null func", RefNull(RefType::FuncRef)),
            ("ref.null extern", RefNull(RefType::ExternRef)),
            ("ref.is_null", RefIsNull),
            ("ref.func 0", RefFunc(0)),
            ("table.get 1", TableGet(1)),
            ("table.set 0", TableSet(0)),
            ("table.init 1 0", TableInit { elem: 0, table: 1 }),
            ("elem.drop 0", ElemDrop(0)),
            ("table.copy 0 1", TableCopy { dst: 0, src: 1 }),
            ("table.grow 1", TableGrow(1)),
            ("table.size 0", TableSize(0)),
            ("table.fill 1", TableFill(1)),
            ("call 0", Call(0)),
            ("call_indirect 1 (type 0)", CallIndirect { ty: 0, table: 1 }),
            ("return", Return),
            ("unreachable", Unreachable),
            ("nop", Nop),
        ],
    );
}

// ---------------------------------------------------------------------------
// Control flow resolution
// ---------------------------------------------------------------------------

#[test]
fn block_types_and_end_indices() {
    use Instr::*;
    let got = body(
        "(module (type $multi (func (param i32) (result i32 i64)))
           (func
             block end
             block (result i32) i32.const 1 end
             loop (result f64) f64.const 0 end
             block (type $multi) drop i64.const 2 end
             loop (param i32) drop end
           ))",
    );
    assert_eq!(
        got,
        vec![
            Block { ty: BlockType::Empty, end: 1 },
            End,
            Block { ty: BlockType::Val(ValType::I32), end: 4 },
            I32Const(1),
            End,
            Loop { ty: BlockType::Val(ValType::F64), end: 7 },
            F64Const(0),
            End,
            Block { ty: BlockType::Type(0), end: 11 },
            Drop,
            I64Const(2),
            End,
            // `(param i32)` with no result gets its own type index (1 is the
            // implicit type of the function itself).
            Loop { ty: BlockType::Type(2), end: 14 },
            Drop,
            End,
        ]
    );
}

#[test]
fn if_else_resolution() {
    use Instr::*;
    let got = body(
        "(module (func (param i32)
           local.get 0
           if (result i32)
             i32.const 1
           else
             i32.const 2
           end
           drop
           local.get 0
           if nop end
           local.get 0
           if
             local.get 0
             if nop else nop end
           else
             block br 0 end
           end
         ))",
    );
    assert_eq!(
        got,
        vec![
            LocalGet(0),
            If { ty: BlockType::Val(ValType::I32), else_: 3, end: 5 },
            I32Const(1),
            Else { end: 5 },
            I32Const(2),
            End,
            Drop,
            LocalGet(0),
            // No else arm: `else_ == end`.
            If { ty: BlockType::Empty, else_: 10, end: 10 },
            Nop,
            End,
            LocalGet(0),
            If { ty: BlockType::Empty, else_: 19, end: 23 },
            LocalGet(0),
            If { ty: BlockType::Empty, else_: 16, end: 18 },
            Nop,
            Else { end: 18 },
            Nop,
            End,
            Else { end: 23 },
            Block { ty: BlockType::Empty, end: 22 },
            Br(0),
            End,
            End,
        ]
    );
}

#[test]
fn branches_and_br_table() {
    use Instr::*;
    let m = module(
        "(module (func (param i32)
           block block block
             local.get 0
             br_table 0 1 2 1
             br 2
             local.get 0
             br_if 0
             local.get 0
             br_table 0
           end end end
         ))",
    );
    let b = &m.code[0];
    assert_eq!(b.instrs[4], BrTable(0));
    assert_eq!(b.instrs[5], Br(2));
    assert_eq!(b.instrs[7], BrIf(0));
    assert_eq!(b.instrs[9], BrTable(1));
    assert_eq!(b.br_table(0).unwrap().targets, vec![0, 1, 2]);
    assert_eq!(b.br_table(0).unwrap().default, 1);
    assert!(b.br_table(1).unwrap().targets.is_empty());
    assert_eq!(b.br_table(1).unwrap().default, 0);
    assert!(b.br_table(2).is_none());
}

#[test]
fn locals_are_expanded() {
    let m = module("(module (func (param i32 f32) (local i64 i64 i64) (local f64) (local) (local funcref)))");
    assert_eq!(m.code[0].locals, vec![ValType::I64, ValType::I64, ValType::I64, ValType::F64, ValType::FuncRef]);
    assert_eq!(m.func_type_of(0).unwrap().params, vec![ValType::I32, ValType::F32]);
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

#[test]
fn all_sections() {
    let m = module(
        r#"(module
          (type $t (func (param i32 i32) (result i32)))
          (type $u (func (result i32 i64)))
          (import "env" "log" (func $log (param i32 i32)))
          (import "env" "mem" (memory 1 4))
          (import "env" "tbl" (table 2 10 funcref))
          (import "env" "g" (global $g (mut i32)))
          (func $add (type $t) local.get 0 local.get 1 i32.add)
          (func $start)
          (func $two (type $u) i32.const 1 i64.const 2)
          (table 1 externref)
          (memory 0)
          (global $h f64 (f64.const 1.5))
          (global $i (mut funcref) (ref.null func))
          (global $j i32 (global.get $g))
          (export "add" (func $add))
          (export "memory" (memory 0))
          (export "tbl" (table 0))
          (export "g" (global $g))
          (start $start)
          (elem (i32.const 0) $add $start)
          (elem func $two)
          (elem declare func $add)
          (elem (table 1) (i32.const 0) externref (ref.null extern))
          (elem externref (ref.null extern) (ref.null extern))
          (elem (table 0) (i32.const 1) func $add)
          (elem (table 0) (i32.const 1) funcref (ref.func $add) (ref.null func))
          (elem declare funcref (ref.func $two))
          (data (i32.const 8) "hello")
          (data "passive")
          (data (memory 1) (i32.const 0) "x")
          (@custom "note" "ignored")
        )"#,
    );

    assert_eq!(m.types.len(), 4); // $t, $u and the implicit types of $log and $start
    assert_eq!(m.types[0], FuncType::new(vec![ValType::I32, ValType::I32], vec![ValType::I32]));

    assert_eq!(m.imports().len(), 4);
    assert_eq!(m.imports()[0].module, "env");
    assert_eq!(m.imports()[0].name, "log");
    assert!(matches!(m.imports()[0].desc, ImportDesc::Func(_)));
    assert_eq!(
        m.imports()[1].desc,
        ImportDesc::Memory(kalium_wasm::MemType { limits: ResizableLimits { min: 1, max: Some(4) } })
    );
    assert_eq!(
        m.imports()[2].desc,
        ImportDesc::Table(kalium_wasm::TableType { elem: RefType::FuncRef, limits: ResizableLimits { min: 2, max: Some(10) } })
    );
    assert_eq!(m.imports()[3].desc, ImportDesc::Global(GlobalType { ty: ValType::I32, mutable: true }));

    assert_eq!(m.num_imported_funcs(), 1);
    assert_eq!(m.num_funcs(), 4);
    assert_eq!(m.num_tables(), 2);
    assert_eq!(m.num_memories(), 2);
    assert_eq!(m.num_globals(), 4);
    assert_eq!(m.func_type_of(1), Some(&m.types[0]));
    assert_eq!(m.func_type(m.func_type_index(3).unwrap()).results, vec![ValType::I32, ValType::I64]);
    assert!(m.func_body(0).is_none());
    assert_eq!(m.func_body(1).unwrap().instrs.len(), 4);
    assert!(m.func_body(4).is_none());
    assert_eq!(m.table_type(1).unwrap().elem, RefType::ExternRef);
    assert_eq!(m.memory_type(1).unwrap().limits, ResizableLimits { min: 0, max: None });
    assert!(m.global_type(0).unwrap().mutable);
    assert_eq!(m.global_type(1).unwrap().ty, ValType::F64);
    assert!(m.global_type(4).is_none());

    assert_eq!(m.globals[0].init.single(), Some(Instr::F64Const(1.5f64.to_bits())));
    assert_eq!(m.globals[1].init.single(), Some(Instr::RefNull(RefType::FuncRef)));
    assert_eq!(m.globals[2].init.single(), Some(Instr::GlobalGet(0)));

    assert_eq!(m.exports().len(), 4);
    assert_eq!(m.export_func("add"), Some(1));
    assert_eq!(m.export("memory").unwrap().desc, ExportDesc::Memory(0));
    assert_eq!(m.export("tbl").unwrap().desc, ExportDesc::Table(0));
    assert_eq!(m.export("g").unwrap().desc, ExportDesc::Global(0));
    assert_eq!(m.export_func("nope"), None);
    assert_eq!(m.start, Some(2));

    // Element segments: all eight encodings.
    assert_eq!(m.elements.len(), 8);
    let e = &m.elements;
    assert_eq!(e[0].mode, ElemMode::Active { table: 0, offset: const_i32(0) });
    assert_eq!(e[0].items, ElemItems::Funcs(vec![1, 2]));
    assert_eq!(e[1].mode, ElemMode::Passive);
    assert_eq!(e[1].items, ElemItems::Funcs(vec![3]));
    assert_eq!(e[2].mode, ElemMode::Declarative);
    assert_eq!(e[3].mode, ElemMode::Active { table: 1, offset: const_i32(0) });
    assert_eq!(e[3].ty, RefType::ExternRef);
    assert_eq!(e[3].len(), 1);
    assert_eq!(e[4].mode, ElemMode::Passive);
    assert_eq!(e[4].ty, RefType::ExternRef);
    assert_eq!(e[4].len(), 2);
    assert_eq!(e[5].mode, ElemMode::Active { table: 0, offset: const_i32(1) });
    assert_eq!(e[5].items, ElemItems::Funcs(vec![1]));
    assert_eq!(e[6].mode, ElemMode::Active { table: 0, offset: const_i32(1) });
    match &e[6].items {
        ElemItems::Exprs(x) => {
            assert_eq!(x[0].single(), Some(Instr::RefFunc(1)));
            assert_eq!(x[1].single(), Some(Instr::RefNull(RefType::FuncRef)));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(e[7].mode, ElemMode::Declarative);
    assert!(matches!(e[7].items, ElemItems::Exprs(_)));

    assert_eq!(m.data.len(), 3);
    assert_eq!(m.data[0].mode, DataMode::Active { memory: 0, offset: const_i32(8) });
    assert_eq!(m.data[0].bytes, b"hello");
    assert_eq!(m.data[1].mode, DataMode::Passive);
    assert_eq!(m.data[1].bytes, b"passive");
    assert_eq!(m.data[2].mode, DataMode::Active { memory: 1, offset: const_i32(0) });
    assert_eq!(m.data_count, None);
}

fn const_i32(v: i32) -> kalium_wasm::ConstExpr {
    kalium_wasm::ConstExpr { instrs: vec![Instr::I32Const(v)] }
}

#[test]
fn data_count_section() {
    let m = module(r#"(module (memory 1) (data "a") (data "b") (func memory.init 1 data.drop 0))"#);
    assert_eq!(m.data_count, Some(2));
}

#[test]
fn empty_module() {
    let m = module("(module)");
    assert_eq!(m, Module::default());
}

// ---------------------------------------------------------------------------
// The Rust-built fixture
// ---------------------------------------------------------------------------

#[test]
fn rust_fixture_decodes() {
    let m = Module::new(FIXTURE).expect("fixture decodes");

    // One import: env.host_log(i32, i32).
    assert_eq!(m.imports().len(), 1);
    assert_eq!(m.imports()[0].module, "env");
    assert_eq!(m.imports()[0].name, "host_log");
    let log_ty = m.func_type_of(0).unwrap();
    assert_eq!(log_ty.params, vec![ValType::I32, ValType::I32]);
    assert!(log_ty.results.is_empty());

    // The exports named in rust-src/src/lib.rs.
    for name in ["greet", "fib", "dispatch", "bump", "fill_scratch", "to_int", "to_uint", "extend8", "extend16", "hypot", "mix"]
    {
        let idx = m.export_func(name).unwrap_or_else(|| panic!("export {name}"));
        assert!(m.func_body(idx).is_some(), "{name} is defined, not imported");
    }
    assert_eq!(m.func_type_of(m.export_func("fib").unwrap()).unwrap().results, vec![ValType::I64]);
    assert_eq!(
        m.func_type_of(m.export_func("hypot").unwrap()).unwrap(),
        &FuncType::new(vec![ValType::F64, ValType::F64], vec![ValType::F64])
    );
    assert!(matches!(m.export("memory").unwrap().desc, ExportDesc::Memory(0)));
    assert_eq!(m.num_memories(), 1);

    // A function table fed by an active element segment, used by dispatch.
    assert!(m.num_tables() >= 1);
    assert!(m.elements.iter().any(|e| matches!(e.mode, ElemMode::Active { .. }) && !e.is_empty()));

    // The greeting lives in a data segment.
    assert!(m.data.iter().any(|d| d.bytes.windows(18).any(|w| w == b"hello from fixture")));

    // Feature coverage in the emitted code.
    let all: Vec<Instr> = m.code.iter().flat_map(|b| b.instrs.iter().copied()).collect();
    let has = |p: fn(&Instr) -> bool| all.iter().any(p);
    assert!(has(|i| matches!(i, Instr::CallIndirect { .. })), "call_indirect");
    assert!(has(|i| matches!(i, Instr::MemoryFill)), "memory.fill");
    assert!(has(|i| matches!(i, Instr::MemoryCopy)), "memory.copy");
    assert!(has(|i| matches!(i, Instr::I32TruncSatF64S)), "i32.trunc_sat_f64_s");
    assert!(has(|i| matches!(i, Instr::I32TruncSatF32U)), "i32.trunc_sat_f32_u");
    assert!(has(|i| matches!(i, Instr::I32Extend8S)), "i32.extend8_s");
    assert!(has(|i| matches!(i, Instr::I64Extend16S)), "i64.extend16_s");
    assert!(has(|i| matches!(i, Instr::I64Rotl)), "i64.rotl");
    assert!(has(|i| matches!(i, Instr::I64Popcnt)), "i64.popcnt");
    assert!(has(|i| matches!(i, Instr::F64Mul)), "f64.mul");
    assert!(has(|i| matches!(i, Instr::Loop { .. })), "loop");
    assert!(has(|i| matches!(i, Instr::Call(_))), "call");

    // Every block/loop/if points at an End, every Else at an End.
    for b in &m.code {
        assert_eq!(b.instrs.last(), Some(&Instr::End));
        for (pc, i) in b.instrs.iter().enumerate() {
            match *i {
                Instr::Block { end, .. } | Instr::Loop { end, .. } => {
                    assert!(end as usize > pc);
                    assert_eq!(b.instrs[end as usize], Instr::End);
                }
                Instr::If { else_, end, .. } => {
                    assert!(end as usize > pc);
                    assert_eq!(b.instrs[end as usize], Instr::End);
                    if else_ != end {
                        assert!(matches!(b.instrs[else_ as usize], Instr::Else { end: e } if e == end));
                    }
                }
                Instr::Else { end } => assert_eq!(b.instrs[end as usize], Instr::End),
                Instr::BrTable(t) => assert!(b.br_table(t).is_some()),
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn section(id: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![id];
    leb(&mut v, payload.len() as u32);
    v.extend_from_slice(payload);
    v
}

fn leb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn raw(sections: &[Vec<u8>]) -> Vec<u8> {
    let mut v = b"\0asm\x01\0\0\0".to_vec();
    for s in sections {
        v.extend_from_slice(s);
    }
    v
}

fn malformed(bytes: &[u8]) -> (usize, &'static str) {
    match Module::new(bytes) {
        Err(Error::Malformed { offset, msg }) => (offset, msg),
        other => panic!("expected Malformed, got {other:?}"),
    }
}

#[test]
fn header_errors() {
    assert_eq!(malformed(b""), (0, "magic header not detected"));
    assert_eq!(malformed(b"\0asm"), (4, "unknown binary version"));
    assert_eq!(malformed(b"\0asm\x01\0\0"), (4, "unknown binary version"));
    assert_eq!(malformed(b"\0asm\x02\0\0\0"), (4, "unknown binary version"));
    assert_eq!(malformed(b"\x01asm\x01\0\0\0"), (0, "magic header not detected"));
}

#[test]
fn section_errors() {
    // Unknown id.
    assert_eq!(malformed(&raw(&[section(13, &[])])), (8, "malformed section id"));
    // Size runs past the end.
    assert_eq!(malformed(&raw(&[vec![1, 5, 0]])), (10, "length out of bounds"));
    // Content shorter than declared size.
    assert_eq!(malformed(&raw(&[section(1, &[0, 0])])), (11, "section size mismatch"));
    // Out of order: import (2) after function (3).
    let bytes = raw(&[section(3, &[0]), section(2, &[0])]);
    assert_eq!(malformed(&bytes), (11, "unexpected content after last section"));
    // Repeated section.
    let bytes = raw(&[section(1, &[0]), section(1, &[0])]);
    assert_eq!(malformed(&bytes), (11, "unexpected content after last section"));
    // Data count (12) must come before code (10), but after element (9).
    assert!(decode(&raw(&[section(9, &[0]), section(12, &[0]), section(10, &[0]), section(11, &[0])])).is_ok());
    assert_eq!(malformed(&raw(&[section(10, &[0]), section(12, &[0])])), (11, "unexpected content after last section"));
    // Custom sections go anywhere, but their names must be UTF-8.
    assert!(decode(&raw(&[section(0, b"\x01a"), section(1, &[0]), section(0, b"\x00xyz"), section(3, &[0])])).is_ok());
    assert_eq!(malformed(&raw(&[section(0, b"\x01\xff")])), (11, "malformed UTF-8 encoding"));
    // Truncated inside a section.
    assert_eq!(malformed(&raw(&[section(1, &[1])])), (11, "unexpected end of section or function"));
    // Truncated at top level.
    assert_eq!(malformed(&raw(&[vec![1]])), (9, "unexpected end"));
}

#[test]
fn function_code_consistency() {
    // Function section with one entry, no code section.
    let bytes = raw(&[section(1, &[1, 0x60, 0, 0]), section(3, &[1, 0])]);
    assert_eq!(malformed(&bytes).1, "function and code section have inconsistent lengths");
    // Code section with a different count.
    let bytes = raw(&[section(1, &[1, 0x60, 0, 0]), section(3, &[1, 0]), section(10, &[2, 2, 0, 0x0B, 2, 0, 0x0B])]);
    assert_eq!(malformed(&bytes).1, "function and code section have inconsistent lengths");
    // Code section without function section.
    let bytes = raw(&[section(10, &[1, 2, 0, 0x0B])]);
    assert_eq!(malformed(&bytes).1, "function and code section have inconsistent lengths");
    // Consistent.
    let bytes = raw(&[section(1, &[1, 0x60, 0, 0]), section(3, &[1, 0]), section(10, &[1, 2, 0, 0x0B])]);
    assert!(decode(&bytes).is_ok());
}

#[test]
fn data_count_consistency() {
    let bytes = raw(&[section(5, &[1, 0, 1]), section(12, &[2]), section(11, &[1, 1, 0])]);
    assert_eq!(malformed(&bytes).1, "data count and data section have inconsistent lengths");
    let bytes = raw(&[section(5, &[1, 0, 1]), section(12, &[1])]);
    assert_eq!(malformed(&bytes).1, "data count and data section have inconsistent lengths");
    // memory.init without a data count section.
    let bytes = raw(&[
        section(1, &[1, 0x60, 0, 0]),
        section(3, &[1, 0]),
        section(5, &[1, 0, 1]),
        section(10, &[1, 6, 0, 0xFC, 8, 0, 0, 0x0B]),
        section(11, &[1, 1, 0]),
    ]);
    assert_eq!(malformed(&bytes).1, "data count section required");
    let bytes = raw(&[
        section(1, &[1, 0x60, 0, 0]),
        section(3, &[1, 0]),
        section(5, &[1, 0, 1]),
        section(10, &[1, 5, 0, 0xFC, 9, 0, 0x0B]),
    ]);
    assert_eq!(malformed(&bytes).1, "data count section required");
}

#[test]
fn body_errors() {
    fn with_body(body: &[u8]) -> Vec<u8> {
        let mut code = vec![1];
        leb(&mut code, body.len() as u32);
        code.extend_from_slice(body);
        raw(&[section(1, &[1, 0x60, 0, 0]), section(3, &[1, 0]), section(10, &code)])
    }
    // Missing end.
    assert_eq!(malformed(&with_body(&[0, 0x01])).1, "unexpected end of section or function");
    // Bytes after the final end.
    assert_eq!(malformed(&with_body(&[0, 0x0B, 0x01])).1, "operators remaining after end of function");
    // Stray else.
    assert_eq!(malformed(&with_body(&[0, 0x05, 0x0B])).1, "unexpected else");
    // Two elses.
    assert_eq!(malformed(&with_body(&[0, 0x04, 0x40, 0x05, 0x05, 0x0B, 0x0B])).1, "unexpected else");
    // Else in a block.
    assert_eq!(malformed(&with_body(&[0, 0x02, 0x40, 0x05, 0x0B, 0x0B])).1, "unexpected else");
    // Illegal opcode.
    assert_eq!(malformed(&with_body(&[0, 0x27, 0x0B])).1, "illegal opcode");
    assert_eq!(malformed(&with_body(&[0, 0xFC, 18, 0x0B])).1, "illegal opcode");
    // memory.size needs its zero byte.
    assert_eq!(malformed(&with_body(&[0, 0x3F, 0x01, 0x0B])).1, "zero byte expected");
    assert_eq!(malformed(&with_body(&[0, 0xFC, 10, 0x00, 0x01, 0x0B])).1, "zero byte expected");
    // Typed select needs exactly one type.
    assert_eq!(malformed(&with_body(&[0, 0x1C, 0x00, 0x0B])).1, "invalid result arity");
    assert_eq!(malformed(&with_body(&[0, 0x1C, 0x02, 0x7F, 0x7F, 0x0B])).1, "invalid result arity");
    // Bad block type.
    assert_eq!(malformed(&with_body(&[0, 0x02, 0x7B - 1, 0x0B, 0x0B])).1, "malformed block type");
    // Bad value type in locals.
    assert_eq!(malformed(&with_body(&[1, 1, 0x7A, 0x0B])).1, "malformed value type");
    // Too many locals (sum overflows u32).
    assert_eq!(malformed(&with_body(&[2, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0x7F, 0x01, 0x7F, 0x0B])).1, "too many locals");
    // Too many locals (over the implementation limit).
    assert_eq!(malformed(&with_body(&[1, 0x80, 0x80, 0x80, 0x01, 0x7F, 0x0B])).1, "too many locals");
    // Body size larger than the section.
    assert_eq!(malformed(&with_body(&[])).1, "unexpected end of section or function");
    let mut big = raw(&[section(1, &[1, 0x60, 0, 0]), section(3, &[1, 0]), section(10, &[1, 50, 0, 0x0B])]);
    assert_eq!(malformed(&big).1, "length out of bounds");
    big.truncate(big.len() - 1);
    assert_eq!(malformed(&big).1, "length out of bounds");
    // Bad LEB in an immediate.
    assert_eq!(malformed(&with_body(&[0, 0x41, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0x0B])).1, "integer too large");
    assert_eq!(
        malformed(&with_body(&[0, 0x20, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00, 0x0B])).1,
        "integer representation too long"
    );
    // A truncated f64.const.
    assert_eq!(malformed(&with_body(&[0, 0x44, 1, 2, 3, 0x0B])).1, "unexpected end of section or function");
}

#[test]
fn type_section_errors() {
    assert_eq!(malformed(&raw(&[section(1, &[1, 0x5F, 0, 0])])).1, "malformed function type");
    assert_eq!(malformed(&raw(&[section(1, &[1, 0x60, 1, 0x7A, 0])])).1, "malformed value type");
    assert_eq!(malformed(&raw(&[section(2, &[1, 1, b'a', 1, b'b', 4])])).1, "malformed import kind");
    assert_eq!(malformed(&raw(&[section(2, &[1, 1, b'a', 1, b'b', 3, 0x7F, 2])])).1, "malformed mutability");
    assert_eq!(malformed(&raw(&[section(2, &[1, 1, b'a', 1, b'b', 1, 0x7F, 0, 0])])).1, "malformed reference type");
    assert_eq!(malformed(&raw(&[section(4, &[1, 0x70, 4, 0])])).1, "malformed limits flags");
    assert_eq!(malformed(&raw(&[section(7, &[1, 1, b'a', 4, 0])])).1, "malformed export kind");
    assert_eq!(malformed(&raw(&[section(7, &[1, 1, 0xC3, 0, 0])])).1, "malformed UTF-8 encoding");
    assert_eq!(malformed(&raw(&[section(9, &[1, 8])])).1, "malformed elements segment kind");
    assert_eq!(malformed(&raw(&[section(9, &[1, 1, 1, 0])])).1, "malformed element kind");
    assert_eq!(malformed(&raw(&[section(11, &[1, 3])])).1, "malformed data segment kind");
    assert_eq!(malformed(&raw(&[section(11, &[1, 1, 5, b'a'])])).1, "unexpected end of section or function");
    // A br_table cannot appear in a constant expression.
    assert_eq!(malformed(&raw(&[section(6, &[1, 0x7F, 0, 0x0E, 0, 0, 0x0B])])).1, "constant expression required");
}

#[test]
fn unsupported_features() {
    fn unsupported(bytes: &[u8]) -> &'static str {
        match Module::new(bytes) {
            Err(Error::Unsupported { msg, .. }) => msg,
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
    fn with_body(body: &[u8]) -> Vec<u8> {
        let mut code = vec![1];
        leb(&mut code, body.len() as u32);
        code.extend_from_slice(body);
        raw(&[section(1, &[1, 0x60, 0, 0]), section(3, &[1, 0]), section(5, &[1, 0, 1]), section(10, &code)])
    }
    // SIMD: v128.load, i8x16.splat.
    assert!(unsupported(&with_body(&[0, 0xFD, 0, 0, 0, 0x0B])).contains("SIMD"));
    assert!(unsupported(&with_body(&[0, 0xFD, 15, 0x0B])).contains("SIMD"));
    // v128 as a value type.
    assert!(unsupported(&raw(&[section(1, &[1, 0x60, 1, 0x7B, 0])])).contains("SIMD"));
    assert!(unsupported(&with_body(&[1, 1, 0x7B, 0x0B])).contains("SIMD"));
    // Atomics: memory.atomic.notify.
    assert!(unsupported(&with_body(&[0, 0xFE, 0, 2, 0, 0x0B])).contains("threads"));
    // Shared memory.
    assert!(unsupported(&raw(&[section(5, &[1, 3, 1, 1])])).contains("shared"));
    // Tail calls, exception handling, typed function references.
    assert!(unsupported(&with_body(&[0, 0x12, 0, 0x0B])).contains("tail"));
    assert!(unsupported(&with_body(&[0, 0x08, 0, 0x0B])).contains("exception"));
    assert!(unsupported(&with_body(&[0, 0x14, 0, 0x0B])).contains("typed function"));

    // The wat crate agrees these are SIMD/atomics.
    let simd = wasm("(module (func (result v128) v128.const i32x4 1 2 3 4))");
    assert!(matches!(Module::new(&simd), Err(Error::Unsupported { .. })));
    let atomics = wasm("(module (memory 1 1 shared) (func (result i32) i32.const 0 i32.atomic.load))");
    assert!(matches!(Module::new(&atomics), Err(Error::Unsupported { .. })));
    let atomics = wasm("(module (memory 1) (func (result i32) i32.const 0 i32.atomic.load))");
    assert!(matches!(Module::new(&atomics), Err(Error::Unsupported { .. })));
}

#[test]
fn error_display() {
    let e = Module::new(b"junk").unwrap_err();
    assert_eq!(e.to_string(), "malformed module at offset 0x0: magic header not detected");
    assert_eq!(e.message(), "magic header not detected");
    assert_eq!(kalium_wasm::Trap::OutOfBounds.to_string(), "out of bounds memory access");
}

// ---------------------------------------------------------------------------
// Robustness: no panics on any input
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
}

fn corpus() -> Vec<Vec<u8>> {
    let mut v = vec![FIXTURE.to_vec()];
    for wat in [
        "(module)",
        "(module (func (param i32) (result i32) local.get 0 if (result i32) i32.const 1 else i32.const 2 end))",
        r#"(module (memory 1) (data "a") (data "b") (func memory.init 1 data.drop 0 memory.copy memory.fill))"#,
        "(module (type $t (func)) (table 2 funcref) (elem (i32.const 0) 0 0) (func (type $t) call_indirect (type $t)) (func block block br_table 0 1 0 end end))",
        r#"(module (import "a" "b" (func)) (import "a" "c" (global (mut i64))) (global f64 (f64.const 1)) (export "e" (func 0)) (start 0) (elem declare func 0) (elem funcref (ref.func 0)))"#,
        "(module (func (local i32 i64 f32 f64 funcref externref) loop (result i32) i32.const 0 br_if 0 i32.const 1 end drop))",
    ] {
        v.push(wasm(wat));
    }
    v
}

#[test]
fn fuzz_truncation_never_panics() {
    for m in corpus() {
        for len in 0..m.len() {
            let _ = Module::new(&m[..len]);
        }
        // A sanity check that the untouched module still decodes.
        assert!(decode(&m).is_ok());
    }
}

#[test]
fn fuzz_bit_flips_never_panic() {
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    for m in corpus() {
        // Every single-bit flip.
        for pos in 0..m.len() {
            for bit in 0..8 {
                let mut x = m.clone();
                x[pos] ^= 1 << bit;
                let _ = Module::new(&x);
            }
        }
        // Random multi-byte corruption.
        for _ in 0..400 {
            let mut x = m.clone();
            let n = 1 + (rng.next() % 4) as usize;
            for _ in 0..n {
                let pos = (rng.next() as usize) % x.len();
                x[pos] = rng.next() as u8;
            }
            let _ = Module::new(&x);
        }
        // Random byte-run replacement with "interesting" values.
        for _ in 0..200 {
            let mut x = m.clone();
            let pos = (rng.next() as usize) % x.len();
            let val = [0x00, 0xFF, 0x7F, 0x80, 0x0B, 0xFC, 0xFD, 0xFE, 0x02, 0x04][(rng.next() % 10) as usize];
            let run = 1 + (rng.next() % 6) as usize;
            for i in pos..(pos + run).min(x.len()) {
                x[i] = val;
            }
            let _ = Module::new(&x);
        }
    }
}

#[test]
fn fuzz_random_garbage_never_panics() {
    let mut rng = XorShift(42);
    for _ in 0..2000 {
        let len = (rng.next() % 64) as usize;
        let mut x: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if rng.next().is_multiple_of(2) && x.len() >= 8 {
            x[..8].copy_from_slice(b"\0asm\x01\0\0\0");
        }
        let _ = Module::new(&x);
    }
}

#[test]
fn huge_counts_do_not_allocate() {
    // A type section claiming 2^32-1 entries in 5 bytes.
    let bytes = raw(&[section(1, &[0xFF, 0xFF, 0xFF, 0xFF, 0x0F])]);
    assert!(Module::new(&bytes).is_err());
    // A data segment claiming 2^32-1 bytes.
    let bytes = raw(&[section(11, &[1, 1, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0])]);
    assert!(Module::new(&bytes).is_err());
    // A name claiming 2^32-1 bytes.
    let bytes = raw(&[section(2, &[1, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F])]);
    assert!(Module::new(&bytes).is_err());
}

/// Locals are stored expanded, so a module must not be able to make the
/// decoder allocate far more than its own size: the locals of all functions
/// together are bounded by `MAX_LOCALS_PER_BYTE` per input byte (KW-3).
#[test]
fn locals_are_bounded_by_module_size() {
    use kalium_wasm::decode::{MAX_LOCALS, MAX_LOCALS_PER_BYTE};

    /// `funcs` functions of type `() -> ()`, each declaring `locals` i32
    /// locals in a single run.
    fn with_locals(funcs: u32, locals: u32) -> Vec<u8> {
        let mut body = vec![1];
        leb(&mut body, locals);
        body.extend_from_slice(&[0x7F, 0x0B]);
        let mut code = Vec::new();
        leb(&mut code, funcs);
        for _ in 0..funcs {
            leb(&mut code, body.len() as u32);
            code.extend_from_slice(&body);
        }
        let mut func_sec = Vec::new();
        leb(&mut func_sec, funcs);
        func_sec.extend(std::iter::repeat_n(0u8, funcs as usize));
        raw(&[section(1, &[1, 0x60, 0, 0]), section(3, &func_sec), section(10, &code)])
    }
    let over = Err(Error::Limit { msg: "too many locals in module" });

    // One function at the per-function limit: 8 bytes of locals in a module
    // of a few dozen bytes would expand to 1 MiB.
    let m = with_locals(1, MAX_LOCALS);
    assert!(m.len() < 64);
    assert_eq!(Module::new(&m), over);
    // Many functions just under the per-function limit: 256 MiB expanded
    // from a module of about 2 KiB.
    let m = with_locals(256, MAX_LOCALS - 1);
    assert!(m.len() < 4096);
    assert_eq!(Module::new(&m), over);

    // Exactly at the budget decodes; one more local is over it. Both counts
    // take two LEB bytes, so the two modules have the same size.
    let probe = with_locals(2, 128);
    let half = probe.len() as u32 * MAX_LOCALS_PER_BYTE / 2;
    assert!((128..16383).contains(&half));
    let at = with_locals(2, half);
    assert_eq!(at.len(), probe.len());
    let m = Module::new(&at).unwrap();
    let total: usize = m.code.iter().map(|b| b.locals.len()).sum();
    assert_eq!(total, 2 * half as usize);
    assert!(total <= at.len() * MAX_LOCALS_PER_BYTE as usize);
    let beyond = with_locals(2, half + 1);
    assert_eq!(beyond.len(), probe.len());
    assert_eq!(Module::new(&beyond), over);

    // A real toolchain's output is nowhere near the budget.
    let fixture = Module::new(FIXTURE).unwrap();
    let total: usize = fixture.code.iter().map(|b| b.locals.len()).sum();
    assert!(total < FIXTURE.len());
}
