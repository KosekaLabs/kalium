//! `fair`: a CPU hog. Busy-loops for about one second of `time_now` to
//! load the fair class while `rt` measures its periods.
//!
//! Initial capabilities: none.
#![no_std]

use core::hint::black_box;

use kalium_sdk::{exit, time_now};

const RUN_NS: i64 = 1_000_000_000;
/// Work between clock reads, to keep host calls a small fraction of the
/// loop.
const BURST: u32 = 256;

#[no_mangle]
pub extern "C" fn _start() {
    let end = time_now().saturating_add(RUN_NS);
    let mut acc: u32 = black_box(0);
    while time_now() < end {
        for i in 0..BURST {
            acc = black_box(acc.wrapping_mul(1_103_515_245).wrapping_add(i));
        }
    }
    black_box(acc);
    exit(0);
}
