//! Host benchmark of the interpreter.
//!
//! Run with
//!
//! ```text
//! cargo run -p kalium-wasm --release --example bench
//! ```
//!
//! Every workload is a small module written in the text format, assembled
//! with the `wat` dev-dependency, instantiated once and run three times;
//! the best time of the three is reported together with the number of
//! instructions it executed (the interpreter's fuel accounting: one unit
//! per executed instruction) and the resulting rate. The last line measures
//! the overhead of fuel slicing: the loop workload run in slices of 1000
//! instructions against one unbounded run.
//!
//! `KALIUM_BENCH_SCALE=<percent>` scales the iteration counts (default 100)
//! for quick runs; `KALIUM_BENCH_ONLY=<name>` runs one workload.
//!
//! Run-to-run noise on a shared machine is around 10%, more than most
//! single optimizations buy. To compare two versions, keep both binaries
//! (`target/release/examples/bench`) and alternate them several times on one
//! workload with `KALIUM_BENCH_ONLY`, taking the best of each; for a
//! noise-free signal, count executed instructions with
//! `valgrind --tool=cachegrind --cache-sim=no` at `KALIUM_BENCH_SCALE=4`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kalium_wasm::{ExecResult, Execution, Host, HostCtx, HostFuncId, HostOutcome, Instance, Limits, Module, Trap, Value};

/// `env.inc(x) -> x + 1`, the host function of the host-call workload.
struct IncHost {
    calls: u64,
}

impl Host for IncHost {
    fn call(
        &mut self,
        _ctx: &mut HostCtx<'_>,
        _func: HostFuncId,
        args: &[Value],
        results: &mut [Value],
    ) -> Result<HostOutcome, Trap> {
        self.calls += 1;
        let x = args.first().and_then(Value::as_i32).ok_or(Trap::Host)?;
        if let Some(r) = results.first_mut() {
            *r = Value::I32(x.wrapping_add(1));
        }
        Ok(HostOutcome::Continue)
    }
}

struct Workload {
    name: &'static str,
    wat: String,
    args: Vec<Value>,
    /// Checked against the result, when the workload has one.
    expect: Option<Value>,
    /// Bytes moved per run, for the memory workloads.
    bytes: u64,
}

fn scaled(n: u64, scale: u64) -> u64 {
    (n * scale / 100).max(1)
}

fn workloads(scale: u64) -> Vec<Workload> {
    let mut v = Vec::new();

    // (a) recursive fib(27): calls, i32 arithmetic, if/else.
    let fib_n = if scale >= 100 { 27 } else { 22 };
    v.push(Workload {
        name: "fib",
        wat: r#"(module
            (func $fib (export "run") (param i32) (result i32)
                (if (result i32) (i32.lt_s (local.get 0) (i32.const 2))
                    (then (local.get 0))
                    (else (i32.add
                        (call $fib (i32.sub (local.get 0) (i32.const 1)))
                        (call $fib (i32.sub (local.get 0) (i32.const 2))))))))"#
            .to_string(),
        args: vec![Value::I32(fib_n)],
        expect: Some(Value::I32(if fib_n == 27 { 196_418 } else { 17_711 })),
        bytes: 0,
    });

    // (b) tight loop summing 50M i32 through locals.
    let loop_n = scaled(50_000_000, scale);
    v.push(Workload {
        name: "loop",
        wat: format!(
            r#"(module
            (func (export "run") (result i32)
                (local $i i32) (local $sum i32)
                (loop $l
                    (local.set $sum (i32.add (local.get $sum) (local.get $i)))
                    (local.set $i (i32.add (local.get $i) (i32.const 1)))
                    (br_if $l (i32.lt_u (local.get $i) (i32.const {loop_n}))))
                (local.get $sum)))"#
        ),
        args: vec![],
        expect: Some(Value::I32((0..loop_n).fold(0u32, |s, i| s.wrapping_add(i as u32)) as i32)),
        bytes: 0,
    });

    // (c) 64 KiB memcpy through i32 loads and stores, repeated.
    let memcpy_reps = scaled(200, scale);
    v.push(Workload {
        name: "memcpy",
        wat: format!(
            r#"(module
            (memory 2)
            (func (export "run") (result i32)
                (local $rep i32) (local $i i32)
                (loop $outer
                    (local.set $i (i32.const 0))
                    (loop $inner
                        (i32.store offset=65536 (local.get $i) (i32.load (local.get $i)))
                        (local.set $i (i32.add (local.get $i) (i32.const 4)))
                        (br_if $inner (i32.lt_u (local.get $i) (i32.const 65536))))
                    (local.set $rep (i32.add (local.get $rep) (i32.const 1)))
                    (br_if $outer (i32.lt_u (local.get $rep) (i32.const {memcpy_reps}))))
                (i32.load offset=65536 (i32.const 1024))))"#
        ),
        args: vec![],
        expect: Some(Value::I32(0)),
        bytes: memcpy_reps * 65_536,
    });

    // (d) bulk memory: fill one page, copy it to the next, repeated.
    let bulk_reps = scaled(20_000, scale);
    v.push(Workload {
        name: "bulk",
        wat: format!(
            r#"(module
            (memory 2)
            (func (export "run") (result i32)
                (local $rep i32)
                (loop $l
                    (memory.fill (i32.const 0) (local.get $rep) (i32.const 65536))
                    (memory.copy (i32.const 65536) (i32.const 0) (i32.const 65536))
                    (local.set $rep (i32.add (local.get $rep) (i32.const 1)))
                    (br_if $l (i32.lt_u (local.get $rep) (i32.const {bulk_reps}))))
                (i32.load8_u offset=65536 (i32.const 12345))))"#
        ),
        args: vec![],
        expect: Some(Value::I32(((bulk_reps - 1) & 0xff) as i32)),
        bytes: bulk_reps * 2 * 65_536,
    });

    // (e) mandelbrot-style f64 loop over a grid.
    let mandel_side = if scale >= 100 { 160 } else { 60 };
    v.push(Workload {
        name: "mandel",
        wat: format!(
            r#"(module
            (func (export "run") (result i32)
                (local $px i32) (local $py i32) (local $iter i32) (local $count i32)
                (local $cx f64) (local $cy f64) (local $x f64) (local $y f64) (local $xt f64)
                (local.set $py (i32.const 0))
                (loop $rows
                    (local.set $px (i32.const 0))
                    (loop $cols
                        (local.set $cx (f64.sub (f64.mul (f64.convert_i32_s (local.get $px))
                            (f64.const {step})) (f64.const 2.0)))
                        (local.set $cy (f64.sub (f64.mul (f64.convert_i32_s (local.get $py))
                            (f64.const {step})) (f64.const 1.5)))
                        (local.set $x (f64.const 0)) (local.set $y (f64.const 0))
                        (local.set $iter (i32.const 0))
                        (block $done
                            (loop $it
                                (br_if $done (f64.gt (f64.add (f64.mul (local.get $x) (local.get $x))
                                    (f64.mul (local.get $y) (local.get $y))) (f64.const 4.0)))
                                (local.set $xt (f64.add (f64.sub (f64.mul (local.get $x) (local.get $x))
                                    (f64.mul (local.get $y) (local.get $y))) (local.get $cx)))
                                (local.set $y (f64.add (f64.mul (f64.const 2.0)
                                    (f64.mul (local.get $x) (local.get $y))) (local.get $cy)))
                                (local.set $x (local.get $xt))
                                (local.set $iter (i32.add (local.get $iter) (i32.const 1)))
                                (br_if $it (i32.lt_u (local.get $iter) (i32.const 100)))))
                        (local.set $count (i32.add (local.get $count) (local.get $iter)))
                        (local.set $px (i32.add (local.get $px) (i32.const 1)))
                        (br_if $cols (i32.lt_u (local.get $px) (i32.const {side}))))
                    (local.set $py (i32.add (local.get $py) (i32.const 1)))
                    (br_if $rows (i32.lt_u (local.get $py) (i32.const {side}))))
                (local.get $count)))"#,
            step = 3.0 / mandel_side as f64,
            side = mandel_side,
        ),
        args: vec![],
        expect: None,
        bytes: 0,
    });

    // (f) call_indirect in a loop, alternating two targets.
    let ind_n = scaled(5_000_000, scale);
    v.push(Workload {
        name: "call_indirect",
        wat: format!(
            r#"(module
            (type $t (func (param i32) (result i32)))
            (table 2 funcref)
            (elem (i32.const 0) $inc $dec)
            (func $inc (type $t) (i32.add (local.get 0) (i32.const 3)))
            (func $dec (type $t) (i32.sub (local.get 0) (i32.const 1)))
            (func (export "run") (result i32)
                (local $i i32) (local $acc i32)
                (loop $l
                    (local.set $acc (call_indirect (type $t) (local.get $acc)
                        (i32.and (local.get $i) (i32.const 1))))
                    (local.set $i (i32.add (local.get $i) (i32.const 1)))
                    (br_if $l (i32.lt_u (local.get $i) (i32.const {ind_n}))))
                (local.get $acc)))"#
        ),
        args: vec![],
        expect: Some(Value::I32(((ind_n.div_ceil(2) * 3) as i64 - (ind_n / 2) as i64) as i32)),
        bytes: 0,
    });

    // (g) host call in a loop; the host returns Continue every time.
    let host_n = scaled(5_000_000, scale);
    v.push(Workload {
        name: "host_call",
        wat: format!(
            r#"(module
            (import "env" "inc" (func $inc (param i32) (result i32)))
            (func (export "run") (result i32)
                (local $i i32) (local $acc i32)
                (loop $l
                    (local.set $acc (call $inc (local.get $acc)))
                    (local.set $i (i32.add (local.get $i) (i32.const 1)))
                    (br_if $l (i32.lt_u (local.get $i) (i32.const {host_n}))))
                (local.get $acc)))"#
        ),
        args: vec![],
        expect: Some(Value::I32(host_n as i32)),
        bytes: 0,
    });

    v
}

fn instantiate(wat: &str) -> Instance {
    let bytes = wat::parse_str(wat).unwrap_or_else(|e| panic!("bad wat: {e}"));
    let module = Arc::new(Module::new(&bytes).unwrap_or_else(|e| panic!("bad module: {e}")));
    Instance::new(module, &mut |_, _, _| Ok(HostFuncId(0)), &Limits::default())
        .unwrap_or_else(|e| panic!("instantiation failed: {e}"))
}

/// Runs `run` to completion in slices of `fuel`; returns the result, the
/// elapsed time and the instructions executed.
fn timed_run(inst: &mut Instance, args: &[Value], fuel: u64) -> (ExecResult, Duration, u64) {
    let f = inst.export_func("run").expect("export run");
    let mut host = IncHost { calls: 0 };
    let mut exec = Execution::new(inst, f, args).expect("Execution::new");
    let start = Instant::now();
    let result = loop {
        match exec.run(inst, &mut host, fuel) {
            ExecResult::OutOfFuel => continue,
            other => break other,
        }
    };
    let elapsed = start.elapsed();
    (result, elapsed, exec.fuel_used())
}

fn rate(instrs: u64, d: Duration) -> f64 {
    instrs as f64 / d.as_secs_f64() / 1e6
}

fn main() {
    let scale: u64 = std::env::var("KALIUM_BENCH_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(100);
    let only = std::env::var("KALIUM_BENCH_ONLY").ok();
    let reps = 3;

    println!("{:<14} {:>14} {:>10} {:>12} {:>10}", "workload", "instructions", "best ms", "M instr/s", "MB/s");
    let mut total = Duration::ZERO;
    let mut loop_unbounded: Option<(Duration, u64)> = None;
    for w in workloads(scale) {
        if only.as_deref().is_some_and(|o| o != w.name) {
            continue;
        }
        let mut inst = instantiate(&w.wat);
        let mut best: Option<(Duration, u64)> = None;
        for _ in 0..reps {
            let (result, elapsed, instrs) = timed_run(&mut inst, &w.args, u64::MAX);
            match (&result, &w.expect) {
                (ExecResult::Done(got), Some(want)) => {
                    assert_eq!(got.as_slice(), &[*want], "{}: wrong result", w.name)
                }
                (ExecResult::Done(_), None) => {}
                (other, _) => panic!("{}: {other:?}", w.name),
            }
            if best.is_none_or(|(b, _)| elapsed < b) {
                best = Some((elapsed, instrs));
            }
        }
        let (d, instrs) = best.expect("at least one rep");
        total += d;
        let mbs =
            if w.bytes > 0 { format!("{:>10.1}", w.bytes as f64 / d.as_secs_f64() / 1e6) } else { format!("{:>10}", "-") };
        println!("{:<14} {:>14} {:>10.1} {:>12.1} {}", w.name, instrs, d.as_secs_f64() * 1e3, rate(instrs, d), mbs);
        if w.name == "loop" {
            loop_unbounded = Some((d, instrs));
        }
    }
    println!("total best-of-{reps} time: {:.1} ms", total.as_secs_f64() * 1e3);

    if let Some((unbounded, instrs)) = loop_unbounded {
        let w = workloads(scale).into_iter().find(|w| w.name == "loop").expect("loop workload");
        let mut inst = instantiate(&w.wat);
        let mut best = Duration::MAX;
        for _ in 0..reps {
            let (result, elapsed, _) = timed_run(&mut inst, &w.args, 1000);
            assert!(matches!(result, ExecResult::Done(_)));
            best = best.min(elapsed);
        }
        println!(
            "fuel slicing: loop with fuel 1000/run {:.1} ms ({:.1} M instr/s) vs unbounded {:.1} ms ({:.1} M instr/s): {:+.1}% overhead",
            best.as_secs_f64() * 1e3,
            rate(instrs, best),
            unbounded.as_secs_f64() * 1e3,
            rate(instrs, unbounded),
            (best.as_secs_f64() / unbounded.as_secs_f64() - 1.0) * 100.0
        );
    }
}
