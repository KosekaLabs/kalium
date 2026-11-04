//! Interpreter tests beyond the spec suite: a real Rust-built module driven
//! through the `Host` trait (with fuel-sliced execution), and a fuzz-style
//! robustness test that runs *unvalidated* modules through instantiation
//! and execution, which must end in an error or a trap, never a panic.

use kalium_wasm::instr::{BlockType, ConstExpr, FuncBody, Instr, MemArg};
use kalium_wasm::module::{
    Data, DataMode, ElemItems, ElemMode, Element, Export, ExportDesc, Global,
};
use kalium_wasm::types::{
    FuncType, GlobalType, MemType, RefType, ResizableLimits, TableType, ValType,
};
use kalium_wasm::{
    ExecResult, Execution, Host, HostCtx, HostFuncId, HostOutcome, Instance, Limits, Module, Trap,
    Value,
};
use std::sync::Arc;

const FIXTURE: &[u8] = include_bytes!("fixtures/rust_fixture.wasm");

/// Implements `env.host_log(ptr, len)`: copies the bytes out of linear
/// memory, bounds-checked through `HostCtx`.
#[derive(Default)]
struct LogHost {
    log: Vec<Vec<u8>>,
}

impl Host for LogHost {
    fn call(
        &mut self,
        ctx: &mut HostCtx<'_>,
        func: HostFuncId,
        args: &[Value],
        _results: &mut [Value],
    ) -> Result<HostOutcome, Trap> {
        assert_eq!(func, HostFuncId(7));
        let ptr = args[0].as_i32().ok_or(Trap::Host)? as u32;
        let len = args[1].as_i32().ok_or(Trap::Host)? as u32;
        let mut buf = vec![0u8; len as usize];
        ctx.memory
            .try_borrow()
            .map_err(|_| Trap::Host)?
            .read(ptr, &mut buf)?;
        self.log.push(buf);
        Ok(HostOutcome::Continue)
    }
}

fn fixture() -> Instance {
    let module = Arc::new(Module::new(FIXTURE).expect("fixture decodes and validates"));
    Instance::new(
        module,
        &mut |m, n, ty| {
            assert_eq!((m, n), ("env", "host_log"));
            assert_eq!(ty.params, [ValType::I32, ValType::I32]);
            assert!(ty.results.is_empty());
            Ok(HostFuncId(7))
        },
        &Limits::default(),
    )
    .expect("fixture instantiates")
}

fn call(inst: &mut Instance, host: &mut LogHost, name: &str, args: &[Value]) -> Vec<Value> {
    let f = inst
        .export_func(name)
        .unwrap_or_else(|| panic!("export {name}"));
    let mut e = Execution::new(inst, f, args).unwrap();
    match e.run(inst, host, u64::MAX) {
        ExecResult::Done(v) => v,
        other => panic!("{name}{args:?}: {other:?}"),
    }
}

#[test]
fn rust_fixture_runs() {
    let mut inst = fixture();
    let mut host = LogHost::default();
    let i = &mut inst;
    let h = &mut host;

    assert_eq!(call(i, h, "greet", &[]), vec![]);
    assert_eq!(host.log, vec![b"hello from fixture".to_vec()]);
    let h = &mut host;

    assert_eq!(
        call(i, h, "fib", &[Value::I32(50)]),
        vec![Value::I64(12586269025)]
    );
    assert_eq!(call(i, h, "fib", &[Value::I32(0)]), vec![Value::I64(0)]);

    // call_indirect through the function table.
    let d = |i: &mut Instance, h: &mut LogHost, w: i32, x: i32, y: i32| {
        call(
            i,
            h,
            "dispatch",
            &[Value::I32(w), Value::I32(x), Value::I32(y)],
        )[0]
    };
    assert_eq!(d(i, h, 0, 20, 22), Value::I32(42));
    assert_eq!(d(i, h, 1, 6, 7), Value::I32(42));
    assert_eq!(d(i, h, 2, 0xff, 0x0f), Value::I32(0xf0));
    assert_eq!(d(i, h, 4, 6, 7), Value::I32(42));

    // State in linear memory persists across calls.
    assert_eq!(call(i, h, "bump", &[]), vec![Value::I32(1)]);
    assert_eq!(call(i, h, "bump", &[]), vec![Value::I32(2)]);

    // memory.fill / memory.copy, observed by the host.
    let ptr = call(i, h, "fill_scratch", &[Value::I32(0xab), Value::I32(16)])[0]
        .as_i32()
        .unwrap() as u32;
    let mut buf = [0u8; 17];
    inst.memory().read(ptr, &mut buf).unwrap();
    assert_eq!(&buf[..16], &[0xab; 16]);
    assert_eq!(buf[16], 0);
    let h = &mut host;
    let i = &mut inst;

    // Saturating conversions, sign extension, floats, 64-bit arithmetic.
    assert_eq!(
        call(i, h, "to_int", &[Value::F64(1e300)]),
        vec![Value::I32(i32::MAX)]
    );
    assert_eq!(
        call(i, h, "to_int", &[Value::F64(f64::NAN)]),
        vec![Value::I32(0)]
    );
    assert_eq!(
        call(i, h, "to_int", &[Value::F64(-2.9)]),
        vec![Value::I32(-2)]
    );
    assert_eq!(
        call(i, h, "to_uint", &[Value::F32(-1.0)]),
        vec![Value::I32(0)]
    );
    assert_eq!(
        call(i, h, "to_uint", &[Value::F32(4.0e9)]),
        vec![Value::I32(4_000_000_000u32 as i32)]
    );
    assert_eq!(
        call(i, h, "extend8", &[Value::I32(0x1ff)]),
        vec![Value::I32(-1)]
    );
    assert_eq!(
        call(i, h, "extend16", &[Value::I64(0x18000)]),
        vec![Value::I64(-32768)]
    );
    let r = call(i, h, "hypot", &[Value::F64(3.0), Value::F64(4.0)])[0]
        .as_f64()
        .unwrap();
    assert!((r - 5.0).abs() < 1e-12, "{r}");
    let a = 0x0123_4567_89ab_cdefi64;
    let b = 0x7eadbeefi32;
    let expected = a.rotate_left(13)
        ^ (b as i64).wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as i64)
        ^ (a >> 7)
        ^ (a.count_ones() as i64);
    assert_eq!(
        call(i, h, "mix", &[Value::I64(a), Value::I32(b)]),
        vec![Value::I64(expected)]
    );

    // Fuel-sliced execution gives the same answer as one run, and every
    // slice stops promptly.
    let fib = inst.export_func("fib").unwrap();
    for &chunk in &[1u64, 3, 17, 100] {
        let mut e = Execution::new(&inst, fib, &[Value::I32(60)]).unwrap();
        let mut slices = 0;
        let result = loop {
            match e.run(&mut inst, &mut host, chunk) {
                ExecResult::OutOfFuel => slices += 1,
                other => break other,
            }
        };
        assert_eq!(result, ExecResult::Done(vec![Value::I64(1548008755920)]));
        assert!(
            slices > 60 / chunk as usize,
            "chunk {chunk}: {slices} slices"
        );
    }
}

#[test]
fn rust_fixture_snapshot_restores_its_state() {
    // The fixture's state (its counter, scratch buffer and function table)
    // after a few calls, restored through the byte encoding into an
    // instance at another function address range, continues identically.
    let mut host = LogHost::default();
    let mut original = fixture();
    let h = &mut host;
    let i = &mut original;
    assert_eq!(call(i, h, "bump", &[]), vec![Value::I32(1)]);
    assert_eq!(call(i, h, "bump", &[]), vec![Value::I32(2)]);
    let ptr = call(i, h, "fill_scratch", &[Value::I32(0x5a), Value::I32(32)])[0];
    assert_eq!(call(i, h, "greet", &[]), vec![]);
    assert_eq!(host.log, [b"hello from fixture".to_vec()]);

    let snapshot = original.snapshot().expect("fixture objects are unshared");
    assert_eq!(
        snapshot.memory_pages(),
        original.memory().size_pages(),
        "the whole memory is captured"
    );
    let bytes = snapshot.to_bytes();
    assert_eq!(bytes.len(), snapshot.encoded_len());
    let decoded = kalium_wasm::Snapshot::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, snapshot);

    let mut restored = Instance::from_snapshot(
        original.module().clone(),
        &decoded,
        &mut |m, n, ty| match ty {
            kalium_wasm::ExternType::Func(ft) if (m, n) == ("env", "host_log") => {
                Some(kalium_wasm::Extern::Func {
                    id: HostFuncId(7),
                    ty: (*ft).clone(),
                    addr: None,
                })
            }
            _ => None,
        },
        &Limits::default(),
        10_000,
    )
    .expect("restore");
    assert_eq!(restored.func_addr_base(), 10_000);
    let mut host2 = LogHost::default();
    let (h, i) = (&mut host2, &mut restored);
    assert_eq!(call(i, h, "bump", &[]), vec![Value::I32(3)]);
    let mut buf = [0u8; 33];
    restored
        .memory()
        .read(ptr.as_i32().unwrap() as u32, &mut buf)
        .unwrap();
    assert_eq!(&buf[..32], &[0x5a; 32]);
    assert_eq!(buf[32], 0);
    let (h, i) = (&mut host2, &mut restored);
    // The function table's references were relocated to the new range.
    assert_eq!(
        call(
            i,
            h,
            "dispatch",
            &[Value::I32(1), Value::I32(6), Value::I32(7)]
        ),
        vec![Value::I32(42)]
    );
    assert_eq!(call(i, h, "greet", &[]), vec![]);
    assert_eq!(host2.log, [b"hello from fixture".to_vec()]);
    // The original went the same way.
    let (h, i) = (&mut host, &mut original);
    assert_eq!(call(i, h, "bump", &[]), vec![Value::I32(3)]);
    assert_eq!(restored.snapshot().unwrap(), original.snapshot().unwrap());
}

// ---------------------------------------------------------------------------
// Robustness: unvalidated modules
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

fn random_const(rng: &mut XorShift) -> Instr {
    match rng.below(5) {
        0 => Instr::I32Const(rng.next() as i32),
        1 => Instr::I64Const(rng.next() as i64),
        2 => Instr::F32Const(rng.next() as u32),
        3 => Instr::F64Const(rng.next()),
        _ => Instr::RefFunc(rng.below(4) as u32),
    }
}

/// A constant of type `ty` most of the time, a random one otherwise.
fn const_of(rng: &mut XorShift, ty: ValType) -> ConstExpr {
    let instr = if rng.below(4) == 0 {
        random_const(rng)
    } else {
        match ty {
            ValType::I32 => Instr::I32Const(rng.below(16) as i32),
            ValType::I64 => Instr::I64Const(rng.next() as i64),
            ValType::F32 => Instr::F32Const(rng.next() as u32),
            ValType::F64 => Instr::F64Const(rng.next()),
            ValType::FuncRef => Instr::RefFunc(rng.below(4) as u32),
            ValType::ExternRef => Instr::RefNull(RefType::ExternRef),
        }
    };
    ConstExpr {
        instrs: vec![instr],
    }
}

/// Random instructions with random immediates, many of them out of range
/// or ill-typed: the interpreter must stop with `Trap::Invalid` (or any
/// other trap), never panic or run away.
fn random_instr(rng: &mut XorShift, len: u32) -> Instr {
    use Instr::*;
    let idx = rng.below(6) as u32;
    let pos = rng.below(len as u64 + 2) as u32;
    let big = (rng.next() >> 16) as u32;
    let bt = match rng.below(3) {
        0 => BlockType::Empty,
        1 => BlockType::Val(random_valtype(rng)),
        _ => BlockType::Type(idx),
    };
    let ma = MemArg {
        align: rng.below(4) as u32,
        offset: if rng.below(2) == 0 {
            big
        } else {
            rng.below(64) as u32
        },
    };
    match rng.below(80) {
        0 => Unreachable,
        1 => Nop,
        2 => Block { ty: bt, end: pos },
        3 => Loop { ty: bt, end: pos },
        4 => If {
            ty: bt,
            else_: pos,
            end: pos,
        },
        5 => Else { end: pos },
        6..=8 => End,
        9 => Br(idx),
        10 => BrIf(idx),
        11 => BrTable(idx),
        12 => Return,
        13 => Call(idx),
        14 => CallIndirect {
            ty: idx,
            table: idx,
        },
        15 => Drop,
        16 => Select,
        17 => SelectT(random_valtype(rng)),
        18..=20 => LocalGet(idx),
        21 => LocalSet(idx),
        22 => LocalTee(idx),
        23 => GlobalGet(idx),
        24 => GlobalSet(idx),
        25 => RefNull(RefType::FuncRef),
        26 => RefIsNull,
        27 => RefFunc(idx),
        28 => TableGet(idx),
        29 => TableSet(idx),
        30 => TableInit {
            elem: idx,
            table: idx,
        },
        31 => ElemDrop(idx),
        32 => TableCopy { dst: idx, src: idx },
        33 => TableGrow(idx),
        34 => TableSize(idx),
        35 => TableFill(idx),
        36 => I32Load(ma),
        37 => I64Load8S(ma),
        38 => F64Store(ma),
        39 => I32Store16(ma),
        40 => I64Load(ma),
        41 => I32Store(ma),
        42 => MemorySize,
        43 => MemoryGrow,
        44 => MemoryInit(idx),
        45 => DataDrop(idx),
        46 => MemoryCopy,
        47 => MemoryFill,
        48..=52 => random_const(rng),
        53 => I32Add,
        54 => I64Mul,
        55 => F32Neg,
        56 => F64Lt,
        57 => I32Eqz,
        58 => I32WrapI64,
        59 => I64TruncSatF64U,
        60 => F32DemoteF64,
        61 => I32Extend8S,
        62 => I64ReinterpretF64,
        63 => I32DivS,
        64 => I64RemU,
        65 => I32TruncF32S,
        66 => I64TruncF64U,
        67 => I32Shl,
        68 => F64Sqrt,
        69 => F32Max,
        70 => I64ExtendI32U,
        71 => I32LtU,
        72 => F64ConvertI64S,
        73 => I32Load(MemArg {
            align: u32::MAX,
            offset: u32::MAX,
        }),
        74 => Br(big),
        75 => Call(big),
        76 => I32Const(0),
        77 => I32Const(-1),
        78 => I64Const(i64::MIN),
        _ => LocalGet(big),
    }
}

fn random_body(rng: &mut XorShift) -> FuncBody {
    let len = rng.below(40) as u32;
    let mut instrs: Vec<Instr> = (0..len).map(|_| random_instr(rng, len)).collect();
    if rng.below(5) != 0 {
        instrs.push(Instr::End);
    }
    FuncBody {
        locals: (0..rng.below(4)).map(|_| random_valtype(rng)).collect(),
        instrs,
        br_tables: (0..rng.below(2))
            .map(|_| kalium_wasm::BrTable {
                targets: (0..rng.below(4)).map(|_| rng.below(5) as u32).collect(),
                default: rng.below(5) as u32,
            })
            .collect(),
    }
}

fn random_module(rng: &mut XorShift) -> Module {
    let nfuncs = 1 + rng.below(4) as usize;
    let types: Vec<FuncType> = (0..3)
        .map(|_| FuncType {
            params: (0..rng.below(3)).map(|_| random_valtype(rng)).collect(),
            results: (0..rng.below(3)).map(|_| random_valtype(rng)).collect(),
        })
        .collect();
    let funcs: Vec<u32> = (0..nfuncs).map(|_| rng.below(3) as u32).collect();
    let code: Vec<FuncBody> = (0..nfuncs).map(|_| random_body(rng)).collect();
    Module {
        types,
        imports: vec![],
        funcs,
        tables: (0..rng.below(3))
            .map(|_| TableType {
                elem: if rng.below(2) == 0 {
                    RefType::FuncRef
                } else {
                    RefType::ExternRef
                },
                limits: ResizableLimits {
                    min: rng.below(5) as u32,
                    max: if rng.below(2) == 0 {
                        None
                    } else {
                        Some(rng.below(8) as u32)
                    },
                },
            })
            .collect(),
        memories: if rng.below(3) == 0 {
            vec![]
        } else {
            vec![MemType {
                limits: ResizableLimits {
                    min: rng.below(2) as u32,
                    max: None,
                },
            }]
        },
        globals: (0..rng.below(3))
            .map(|_| {
                let ty = random_valtype(rng);
                Global {
                    ty: GlobalType {
                        ty,
                        mutable: rng.below(2) == 0,
                    },
                    init: const_of(rng, ty),
                }
            })
            .collect(),
        exports: (0..nfuncs)
            .map(|i| Export {
                name: format!("f{i}"),
                desc: ExportDesc::Func(i as u32),
            })
            .collect(),
        start: if rng.below(4) == 0 {
            Some(rng.below(5) as u32)
        } else {
            None
        },
        elements: (0..rng.below(3))
            .map(|_| Element {
                ty: RefType::FuncRef,
                items: ElemItems::Funcs((0..rng.below(4)).map(|_| rng.below(6) as u32).collect()),
                mode: match rng.below(3) {
                    0 => ElemMode::Passive,
                    1 => ElemMode::Declarative,
                    _ => ElemMode::Active {
                        table: rng.below(3) as u32,
                        offset: const_of(rng, ValType::I32),
                    },
                },
            })
            .collect(),
        data: (0..rng.below(3))
            .map(|_| Data {
                mode: if rng.below(2) == 0 {
                    DataMode::Passive
                } else {
                    DataMode::Active {
                        memory: rng.below(4) as u32 / 3,
                        offset: const_of(rng, ValType::I32),
                    }
                },
                bytes: (0..rng.below(70)).map(|_| rng.next() as u8).collect(),
            })
            .collect(),
        data_count: None,
        code,
        program: Default::default(),
    }
}

struct RefusingHost;

impl Host for RefusingHost {
    fn call(
        &mut self,
        _: &mut HostCtx<'_>,
        _: HostFuncId,
        _: &[Value],
        _: &mut [Value],
    ) -> Result<HostOutcome, Trap> {
        Err(Trap::Host)
    }
}

#[test]
fn fuzz_unvalidated_modules_never_panic() {
    let mut rng = XorShift(0xD1B5_4A32_D192_ED03);
    let limits = Limits {
        max_memory_pages: 4,
        max_stack_values: 512,
        max_call_depth: 16,
        max_table_elements: 64,
    };
    let mut instantiated = 0;
    let mut ran = 0;
    let mut invalid_traps = 0;
    for _ in 0..4000 {
        let module = random_module(&mut rng);
        // Validation itself must not panic either (and will reject nearly
        // everything); the module is executed regardless of its verdict.
        let _ = kalium_wasm::validate::validate(&module);
        let module = Arc::new(module);
        let Ok(mut inst) = Instance::new(module.clone(), &mut |_, _, _| Err(()), &limits) else {
            continue;
        };
        instantiated += 1;
        if let Ok(Some(mut start)) = Execution::start(&inst) {
            let _ = start.run(&mut inst, &mut RefusingHost, 200);
        }
        for f in 0..inst.num_funcs() as u32 {
            let Some(ty) = inst.func_type(f) else {
                continue;
            };
            let args: Vec<Value> = ty.params.iter().map(|&t| Value::default_for(t)).collect();
            let Ok(mut e) = Execution::new(&inst, f, &args) else {
                continue;
            };
            // Resumption after OutOfFuel must be just as robust.
            let mut budget = 3;
            loop {
                match e.run(&mut inst, &mut RefusingHost, 50) {
                    ExecResult::OutOfFuel if budget > 0 => budget -= 1,
                    ExecResult::Trap(Trap::Invalid) => {
                        invalid_traps += 1;
                        break;
                    }
                    _ => break,
                }
            }
            ran += 1;
        }
    }
    assert!(
        instantiated > 500,
        "only {instantiated} modules instantiated"
    );
    assert!(ran > 1000, "only {ran} executions");
    assert!(
        invalid_traps > 100,
        "only {invalid_traps} invalid-state traps"
    );
}
