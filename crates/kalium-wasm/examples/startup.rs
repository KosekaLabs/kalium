//! Host benchmark of instance start-up: instantiation (which shares the
//! compiled program of a `Module` between its instances) and restoring an
//! instance from a snapshot, against instantiating and running the
//! module's own initialisation (its entry point up to the first host
//! call). Columns: module decode (`Module::new`, which also compiles),
//! instantiation, instantiation plus the run to the first host call (and
//! the number of instructions that run executed), taking the snapshot, restoring it, and encoding and decoding it.
//!
//! Run with
//!
//! ```text
//! cargo run -p kalium-wasm --release --example startup
//! ```
//!
//! Two modules are measured: the Rust test fixture of `tests/exec.rs` and,
//! when `cargo xtask demos` has built it, the `hello_std` demo
//! (`target/wasm/hello_std.wasm`, a Rust `std` program for
//! `wasm32-wasip1`). Each measurement is the mean over a batch, best of
//! three batches. `KALIUM_BENCH_ONLY=<name>` runs one module.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kalium_wasm::{
    ExecResult, Execution, Host, HostCtx, HostFuncId, HostOutcome, Instance, Limits, Module, Trap,
    Value,
};

/// Blocks on the first host call, then answers every call with zero
/// results (which ends `_start` of a WASI program at its first `fd_write`
/// or `proc_exit`, and the fixture's `host_log` at once).
struct Blocking {
    calls: u32,
}

impl Host for Blocking {
    fn call(
        &mut self,
        _ctx: &mut HostCtx<'_>,
        _func: HostFuncId,
        _args: &[Value],
        _results: &mut [Value],
    ) -> Result<HostOutcome, Trap> {
        self.calls += 1;
        Ok(HostOutcome::Block)
    }
}

fn resolve_all(_: &str, _: &str, _: &kalium_wasm::FuncType) -> Result<HostFuncId, ()> {
    Ok(HostFuncId(0))
}

fn instantiate(module: &Arc<Module>) -> Instance {
    Instance::new(module.clone(), &mut resolve_all, &Limits::default())
        .unwrap_or_else(|e| panic!("instantiation failed: {e}"))
}

/// Best-of-three mean time of `f` over batches of `n`.
fn measure<T>(n: u32, mut f: impl FnMut() -> T) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        for _ in 0..n {
            std::hint::black_box(f());
        }
        best = best.min(start.elapsed() / n);
    }
    best
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn main() {
    let only = std::env::var("KALIUM_BENCH_ONLY").ok();
    let fixture: &[u8] = include_bytes!("../tests/fixtures/rust_fixture.wasm");
    let hello = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm/hello_std.wasm"
    ))
    .ok();
    let mut modules: Vec<(&str, &str, Vec<u8>)> = vec![("fixture", "greet", fixture.to_vec())];
    match hello {
        Some(bytes) => modules.push(("hello_std", "_start", bytes)),
        None => eprintln!("target/wasm/hello_std.wasm not found (cargo xtask demos); skipped"),
    }
    println!(
        "{:<10} {:>6} {:>6} {:>10} {:>12} {:>12} {:>10} {:>12} {:>12} {:>12} {:>13}",
        "module",
        "KB",
        "pages",
        "decode us",
        "instant. us",
        "1st-call us",
        "init instr",
        "snapshot us",
        "restore us",
        "to_bytes us",
        "from_bytes us"
    );
    for (name, entry, bytes) in &modules {
        if only.as_deref().is_some_and(|o| o != *name) {
            continue;
        }
        let decode = measure(5, || Module::new(bytes).expect("module"));
        let module = Arc::new(Module::new(bytes).expect("module"));
        let inst_t = measure(200, || instantiate(&module));
        // Instantiate and run the entry point up to its first host call:
        // the state a snapshot captures.
        let run_init = || {
            let mut inst = instantiate(&module);
            let f = inst.export_func(entry).expect("entry export");
            let mut exec = Execution::new(&inst, f, &[]).expect("Execution::new");
            let mut host = Blocking { calls: 0 };
            let r = exec.run(&mut inst, &mut host, u64::MAX);
            assert!(matches!(r, ExecResult::Blocked), "{name}: {r:?}");
            inst
        };
        let init_t = measure(100, run_init);
        let initialised = run_init();
        let init_instrs = {
            let mut inst = instantiate(&module);
            let f = inst.export_func(entry).expect("entry export");
            let mut exec = Execution::new(&inst, f, &[]).expect("Execution::new");
            exec.run(&mut inst, &mut Blocking { calls: 0 }, u64::MAX);
            exec.fuel_used()
        };
        let snapshot = initialised.snapshot().expect("snapshot");
        let snap_t = measure(100, || initialised.snapshot().expect("snapshot"));
        let restore = |snapshot: &kalium_wasm::Snapshot| {
            Instance::from_snapshot(
                module.clone(),
                snapshot,
                &mut |_, _, ty| match ty {
                    kalium_wasm::ExternType::Func(ft) => Some(kalium_wasm::Extern::Func {
                        id: HostFuncId(0),
                        ty: (*ft).clone(),
                        addr: None,
                    }),
                    _ => None,
                },
                &Limits::default(),
                0,
            )
            .expect("restore")
        };
        let restore_t = measure(200, || restore(&snapshot));
        let encoded = snapshot.to_bytes();
        let to_bytes_t = measure(100, || snapshot.to_bytes());
        let from_bytes_t = measure(100, || {
            kalium_wasm::Snapshot::from_bytes(&encoded).expect("decode")
        });
        // The restored instance holds exactly the captured state.
        let restored = restore(&snapshot);
        assert_eq!(restored.snapshot().expect("snapshot"), snapshot);
        println!(
            "{:<10} {:>6} {:>6} {:>10.1} {:>12.1} {:>12.1} {:>10} {:>12.1} {:>12.1} {:>12.1} {:>13.1}",
            name,
            bytes.len() / 1024,
            snapshot.memory_pages(),
            us(decode),
            us(inst_t),
            us(init_t),
            init_instrs,
            us(snap_t),
            us(restore_t),
            us(to_bytes_t),
            us(from_bytes_t)
        );
    }
}
