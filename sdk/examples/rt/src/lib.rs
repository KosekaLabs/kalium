//! `rt`: a periodic real-time task. Runs 400 periods of 2 ms; in each it
//! does a short fixed amount of work and sleeps until the next period
//! boundary, counting the periods it started on time. Prints
//! `rt: met X/400`.
//!
//! Initial capabilities: slot 0 `Console(WRITE)`.
#![no_std]

use core::hint::black_box;

use kalium_sdk::{Cap, Console, Line, exit, sleep, time_now};

/// Must match what `init` passes to `spawn`.
const PERIOD_NS: i64 = 2_000_000;
const PERIODS: u32 = 400;
/// Iterations of the inner work loop per period. Kept small: the budget is
/// 500 µs and the interpreter runs on emulated hardware.
const WORK_ITERS: u32 = 100;

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let console = Console::from_cap(Cap::slot(0));

    let start = time_now();
    let mut met: u32 = 0;
    for i in 0..PERIODS {
        // Period `i` is [start + i*P, start + (i+1)*P).
        let deadline = start + (i as i64 + 1) * PERIOD_NS;
        let began = time_now();
        if began < deadline {
            met += 1;
        }
        work();
        let now = time_now();
        if now < deadline {
            sleep(deadline - now);
        }
    }

    let _ = Line::new().str("rt: met ").uint(met as u64).str("/").uint(PERIODS as u64).println(console);
    exit(0);
}

/// A fixed amount of computation the optimizer cannot remove.
fn work() {
    let mut acc: u32 = black_box(1);
    for i in 0..WORK_ITERS {
        acc = black_box(acc.wrapping_mul(1_664_525).wrapping_add(i));
    }
    black_box(acc);
}
