//! An ordinary Rust program: `std`, `println!`, collections, formatting,
//! floats, arguments, environment, a sleep, and an exit code. Nothing in it
//! knows about Kalium; it is built for `wasm32-wasip1` and talks to the
//! kernel's WASI preview1 layer through `std`.
//!
//! Boot image module 10 (`hello_std`); `init` spawns it with a console
//! capability in slot 0 (its stdout/stderr) and expects exit code 3.

use std::collections::HashMap;
use std::time::{Duration, Instant};

fn main() {
    println!("hello_std: hello from Rust std on Kalium");

    // A Vec and iterator adaptors.
    let v: Vec<u32> = (1..=10).collect();
    let squares: Vec<u32> = v.iter().take(5).map(|x| x * x).collect();
    println!("hello_std: vec sum {}, len {}, squares {:?}", v.iter().sum::<u32>(), v.len(), squares);

    // String formatting.
    let name = String::from("Kalium");
    let banner = format!("{name:>8}|{name:<8}|{:04}|{:#x}|{:+}", 42, 255, 7);
    println!("hello_std: fmt {banner}");

    // A HashMap (its hasher is seeded through `random_get`); printed sorted,
    // since iteration order is random by design.
    let mut map: HashMap<&str, u32> = HashMap::new();
    for (k, n) in [("one", 1), ("two", 2), ("three", 3)] {
        map.insert(k, n);
    }
    let mut entries: Vec<(&&str, &u32)> = map.iter().collect();
    entries.sort();
    let total: u32 = map.values().sum();
    println!("hello_std: map {entries:?} total {total}");

    // Floating point.
    let x = 2.0f64;
    let mut pi = 0.0f64;
    for k in 0..50_000u32 {
        let term = 4.0 / (2 * k + 1) as f64;
        pi += if k % 2 == 0 { term } else { -term };
    }
    println!("hello_std: sqrt(2) = {:.6}, leibniz pi ~ {:.4}, 1e3 ln = {:.3}", x.sqrt(), pi, 1000f64.ln());

    // Arguments and environment: the kernel passes none.
    let args: Vec<String> = std::env::args().collect();
    let env_count = std::env::vars().count();
    println!("hello_std: args {:?}, env vars {env_count}, HOME {:?}", args, std::env::var("HOME").ok());

    // A sleep goes through `poll_oneoff` with a clock subscription.
    let before = Instant::now();
    std::thread::sleep(Duration::from_millis(10));
    let slept = before.elapsed();
    println!(
        "hello_std: slept 10 ms ({})",
        if slept >= Duration::from_millis(10) { "clock advanced" } else { "clock did not advance" }
    );

    println!("hello_std: exiting with 3");
    std::process::exit(3);
}
