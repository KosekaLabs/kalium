//! `faulty`: traps on purpose with an out-of-bounds load, so the kernel's
//! trap reporting and `instance_wait`'s `-1` can be checked.
//!
//! Initial capabilities: none.
#![no_std]

use core::hint::black_box;

use kalium_sdk::exit;

/// Far beyond any linear memory the kernel grants (the limit is well below
/// 4 GiB), so the load is out of bounds and traps.
const BAD_ADDR: usize = 0xFFFF_FFF0;

#[no_mangle]
pub extern "C" fn _start() {
    let p = black_box(BAD_ADDR) as *const i32;
    // SAFETY: this is deliberately *not* a valid read; on Kalium it traps
    // (out-of-bounds linear memory access) and the instance ends with exit
    // code -1. A volatile read is never elided or folded by the compiler.
    let v = unsafe { core::ptr::read_volatile(p) };
    // Not reached on a correct kernel: distinct codes make a missing trap
    // visible in `init`'s report.
    exit(if v == 0 { 7 } else { 8 });
}
