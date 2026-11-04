//! A small Rust module compiled for `wasm32-unknown-unknown`, used by the
//! kalium-wasm decoder tests as a "real toolchain output" fixture.
//!
//! It is written so that the emitted binary exercises: an import, exported
//! functions and memory, a mutable global or memory-backed state, a function
//! table with `call_indirect`, data segments, bulk memory (`memory.copy`,
//! `memory.fill` through `ptr::copy`/`write_bytes`), sign extension, 64-bit
//! arithmetic, floats and non-trapping float-to-int (`as` casts).
#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

#[link(wasm_import_module = "env")]
extern "C" {
    fn host_log(ptr: *const u8, len: usize);
}

static GREETING: &[u8] = b"hello from fixture";
static mut COUNTER: u32 = 0;
static mut SCRATCH: [u8; 256] = [0; 256];

#[no_mangle]
pub extern "C" fn greet() {
    unsafe { host_log(GREETING.as_ptr(), GREETING.len()) }
}

#[no_mangle]
pub extern "C" fn fib(n: u32) -> u64 {
    let (mut a, mut b) = (0u64, 1u64);
    for _ in 0..n {
        let t = a.wrapping_add(b);
        a = b;
        b = t;
    }
    a
}

fn op_add(x: i32, y: i32) -> i32 {
    x.wrapping_add(y)
}
fn op_mul(x: i32, y: i32) -> i32 {
    x.wrapping_mul(y)
}
fn op_xor(x: i32, y: i32) -> i32 {
    x ^ y
}

static OPS: [fn(i32, i32) -> i32; 3] = [op_add, op_mul, op_xor];

#[no_mangle]
pub extern "C" fn dispatch(which: u32, x: i32, y: i32) -> i32 {
    // `call_indirect` through the function table.
    OPS[(which % 3) as usize](x, y)
}

#[no_mangle]
pub extern "C" fn bump() -> u32 {
    unsafe {
        COUNTER = COUNTER.wrapping_add(1);
        COUNTER
    }
}

#[no_mangle]
pub extern "C" fn fill_scratch(value: u8, len: usize) -> *const u8 {
    unsafe {
        let len = len.min(SCRATCH.len());
        core::ptr::write_bytes(SCRATCH.as_mut_ptr(), value, len); // memory.fill
        core::ptr::copy(SCRATCH.as_ptr(), SCRATCH.as_mut_ptr().add(len / 2), len / 2); // memory.copy
        SCRATCH.as_ptr()
    }
}

#[no_mangle]
pub extern "C" fn to_int(x: f64) -> i32 {
    x as i32 // i32.trunc_sat_f64_s
}

#[no_mangle]
pub extern "C" fn to_uint(x: f32) -> u32 {
    x as u32 // i32.trunc_sat_f32_u
}

#[no_mangle]
pub extern "C" fn extend8(x: i32) -> i32 {
    (x as i8) as i32 // i32.extend8_s
}

#[no_mangle]
pub extern "C" fn extend16(x: i64) -> i64 {
    (x as i16) as i64 // i64.extend16_s
}

#[no_mangle]
pub extern "C" fn hypot(x: f64, y: f64) -> f64 {
    let s = x * x + y * y;
    // Newton iterations instead of libm sqrt to keep the module dependency-free
    // (f64.sqrt is emitted for `f64::sqrt` only with std).
    let mut r = if s > 1.0 { s } else { 1.0 };
    for _ in 0..40 {
        r = 0.5 * (r + s / r);
    }
    r
}

#[no_mangle]
pub extern "C" fn mix(a: i64, b: i32) -> i64 {
    a.rotate_left(13) ^ (b as i64).wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as i64) ^ (a >> 7) ^ (a.count_ones() as i64)
}
