//! Static opcode n-gram histogram over compiled modules: which instruction
//! sequences the interpreter's superinstructions should cover.
//!
//! ```text
//! cargo run -p kalium-wasm --release --example pairs -- [-n 2|3|4] file.wasm...
//! ```
//!
//! With no files, the Rust test fixture and the SDK examples under
//! `target/wasm/` (after `cargo xtask wasm`) are used. Counts are static
//! (one per occurrence in the code section), which is what a peephole pass
//! sees; sequences that cross a control instruction are not counted because
//! the interpreter never fuses across one (a jump target may sit there).

use std::collections::HashMap;

use kalium_wasm::{Instr, Module};

/// The variant name of an instruction, immediates stripped.
fn mnemonic(i: &Instr) -> String {
    let s = format!("{i:?}");
    s.split(['(', ' ']).next().unwrap_or("").to_string()
}

fn is_control(i: &Instr) -> bool {
    matches!(
        i,
        Instr::Block { .. }
            | Instr::Loop { .. }
            | Instr::If { .. }
            | Instr::Else { .. }
            | Instr::End
            | Instr::Br(_)
            | Instr::BrIf(_)
            | Instr::BrTable(_)
            | Instr::Return
            | Instr::Call(_)
            | Instr::CallIndirect { .. }
            | Instr::Unreachable
    )
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut n = 2usize;
    if args.len() >= 2 && args[0] == "-n" {
        n = args[1].parse().expect("-n takes a number");
        args.drain(..2);
    }
    if args.is_empty() {
        args.push("crates/kalium-wasm/tests/fixtures/rust_fixture.wasm".to_string());
        if let Ok(dir) = std::fs::read_dir("target/wasm") {
            for e in dir.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "wasm") {
                    args.push(p.display().to_string());
                }
            }
        }
    }
    let mut singles: HashMap<String, u64> = HashMap::new();
    let mut grams: HashMap<Vec<String>, u64> = HashMap::new();
    let mut total = 0u64;
    for path in &args {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{path}: {e}");
                continue;
            }
        };
        let module = match Module::new(&bytes) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{path}: {e}");
                continue;
            }
        };
        for body in &module.code {
            let names: Vec<String> = body.instrs.iter().map(mnemonic).collect();
            for name in &names {
                *singles.entry(name.clone()).or_default() += 1;
                total += 1;
            }
            for (i, w) in body.instrs.windows(n).enumerate() {
                // The last instruction may be a branch or call (the sequence
                // ends in it); the ones before must not be control.
                if w[..n - 1].iter().any(is_control) {
                    continue;
                }
                *grams.entry(names[i..i + n].to_vec()).or_default() += 1;
            }
        }
        eprintln!("{path}: {} functions", module.code.len());
    }
    println!("{total} instructions in {} files\n", args.len());
    let mut s: Vec<_> = singles.into_iter().collect();
    s.sort_by_key(|e| std::cmp::Reverse(e.1));
    println!("top instructions:");
    for (name, c) in s.iter().take(25) {
        println!("{c:>8} {:5.1}%  {name}", *c as f64 * 100.0 / total as f64);
    }
    let mut g: Vec<_> = grams.into_iter().collect();
    g.sort_by_key(|e| std::cmp::Reverse(e.1));
    println!("\ntop {n}-grams (not crossing control instructions):");
    for (seq, c) in g.iter().take(40) {
        println!("{c:>8} {:5.1}%  {}", *c as f64 * 100.0 / total as f64, seq.join(" "));
    }
}
