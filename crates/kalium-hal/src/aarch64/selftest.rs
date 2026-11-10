//! HAL hardening self-test (`hal-selftest` feature).
//!
//! Runs from `kmain` after the MMU and the heap are up and checks that the
//! memory map and the exception path do what `mmu.rs`, `boot.rs` and
//! `exceptions.rs` promise:
//!
//! - the page attributes, asked of the MMU with `AT S1E1R/W` (no fault
//!   involved): `.text` and `.rodata` refuse writes, the device tree area is
//!   read-only, `.data`, the stacks and the heap are writable normal
//!   write-back memory, the stack guards do not translate, the first byte
//!   past the end of RAM (`ram_info().end`, not rounded to a block) does
//!   not translate either, the UART is device memory;
//! - real faults, each provoked on purpose and caught by the synchronous
//!   exception handler: a write to `.rodata` and to `.text` (data abort,
//!   permission fault), a jump into `.data`, into the stack and into the heap
//!   (instruction abort, permission fault), a read at the end of RAM
//!   (translation fault: nothing without backing memory may be mapped, so
//!   `cargo xtask selftest` also runs with a RAM size that is not a multiple
//!   of 2 MiB) and a kernel stack overflow by unbounded recursion (data
//!   abort, translation fault, inside the guard, reported from the exception
//!   stack);
//! - the cost of an exception round trip (`svc` from EL1 and back), in
//!   counter ticks, which is what the FP/SIMD save in the vectors adds to.
//!
//! # How a fault is survived
//!
//! A case runs through `__kalium_recover_call`, which saves the callee-saved
//! registers and the stack pointer in `KALIUM_RECOVERY_BUF` and calls it.
//! While the test is armed, the exception handler records the fault and
//! returns to `__kalium_recover_land` instead of reporting it; that restores
//! the saved registers and stack pointer and returns `1` from
//! `__kalium_recover_call`. This is `setjmp`/`longjmp` in two dozen
//! instructions: it is sound here because every case is a leaf that owns no
//! value with a destructor and holds no lock, and because the kernel is
//! single-core, so nothing else can observe the armed state.

use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::exceptions::{ExceptionFrame, SyncFault};
use super::{boot, mmu, timer};

/// Register save area for `__kalium_recover_call`: x19–x30 and sp.
#[unsafe(no_mangle)]
static mut KALIUM_RECOVERY_BUF: [u64; 16] = [0; 16];

global_asm!(
    r#"
    .section .text.selftest, "ax"
    .global __kalium_recover_call
    .type __kalium_recover_call, %function
__kalium_recover_call:
    adrp    x9, KALIUM_RECOVERY_BUF
    add     x9, x9, :lo12:KALIUM_RECOVERY_BUF
    stp     x19, x20, [x9, #0]
    stp     x21, x22, [x9, #16]
    stp     x23, x24, [x9, #32]
    stp     x25, x26, [x9, #48]
    stp     x27, x28, [x9, #64]
    stp     x29, x30, [x9, #80]
    mov     x10, sp
    str     x10, [x9, #96]
    blr     x0
    adrp    x9, KALIUM_RECOVERY_BUF
    add     x9, x9, :lo12:KALIUM_RECOVERY_BUF
    ldr     x30, [x9, #88]
    mov     x0, #0
    ret
    .size __kalium_recover_call, . - __kalium_recover_call

    .global __kalium_recover_land
    .type __kalium_recover_land, %function
__kalium_recover_land:
    adrp    x9, KALIUM_RECOVERY_BUF
    add     x9, x9, :lo12:KALIUM_RECOVERY_BUF
    ldr     x10, [x9, #96]
    mov     sp, x10
    ldp     x19, x20, [x9, #0]
    ldp     x21, x22, [x9, #16]
    ldp     x23, x24, [x9, #32]
    ldp     x25, x26, [x9, #48]
    ldp     x27, x28, [x9, #64]
    ldp     x29, x30, [x9, #80]
    mov     x0, #1
    ret
    .size __kalium_recover_land, . - __kalium_recover_land
"#
);

unsafe extern "C" {
    fn __kalium_recover_call(f: extern "C" fn()) -> u64;
    static __kalium_recover_land: u8;
}

static ARMED: AtomicBool = AtomicBool::new(false);
static CAUGHT_ESR: AtomicU64 = AtomicU64::new(0);
static CAUGHT_FAR: AtomicU64 = AtomicU64::new(0);
static CAUGHT_ELR: AtomicU64 = AtomicU64::new(0);
static CAUGHT_SP: AtomicU64 = AtomicU64::new(0);

/// `SPSR_EL1.M` for EL1 with `SP_EL0` (EL1t): the mode the kernel runs in.
const SPSR_M_EL1T: u64 = 0b0100;

/// Called by the exception handler for every synchronous exception it would
/// otherwise report. Returns `true` if a self-test case was armed and the
/// exception has been redirected to the recovery landing pad.
pub(crate) fn try_recover(frame: &mut ExceptionFrame, fault: &SyncFault) -> bool {
    if !ARMED.swap(false, Ordering::Relaxed) || frame.spsr & 0xf != SPSR_M_EL1T {
        return false;
    }
    CAUGHT_ESR.store(fault.esr, Ordering::Relaxed);
    CAUGHT_FAR.store(fault.far, Ordering::Relaxed);
    CAUGHT_ELR.store(fault.elr, Ordering::Relaxed);
    CAUGHT_SP.store(super::cpu::read_sysreg!("sp_el0"), Ordering::Relaxed);
    // The handler's own report of what it saw, from the exception stack.
    crate::println!("kalium: selftest: handler caught {}", fault);
    frame.elr = core::ptr::addr_of!(__kalium_recover_land) as u64;
    true
}

/// What a case produced when it faulted.
#[derive(Clone, Copy, Debug)]
struct Caught {
    fault: SyncFault,
    /// `SP_EL0` at the time of the fault.
    sp: u64,
}

/// Run `case`; `Some` with the fault if it faulted, `None` if it returned.
fn run_case(case: extern "C" fn()) -> Option<Caught> {
    ARMED.store(true, Ordering::Relaxed);
    // SAFETY: see the module documentation; `case` is a leaf with no
    // destructors, and nothing else is armed concurrently.
    let faulted = unsafe { __kalium_recover_call(case) };
    ARMED.store(false, Ordering::Relaxed);
    if faulted == 0 {
        return None;
    }
    let esr = CAUGHT_ESR.load(Ordering::Relaxed);
    Some(Caught {
        fault: SyncFault {
            esr,
            far: CAUGHT_FAR.load(Ordering::Relaxed),
            elr: CAUGHT_ELR.load(Ordering::Relaxed),
            ec: (esr >> 26) & 0x3f,
            iss: esr & 0x1ff_ffff,
        },
        sp: CAUGHT_SP.load(Ordering::Relaxed),
    })
}

// ---- The faulting cases ----------------------------------------------------

/// Lives in `.rodata`.
static RODATA_PROBE: [u32; 4] = [0x6b61_6c69, 0x756d_0000, 1, 2];

/// `ret` twice; lives in `.data` because it is mutable and non-zero.
static mut DATA_CODE: [u32; 2] = [0xd65f_03c0, 0xd65f_03c0];

extern "C" fn write_rodata() {
    let p = core::hint::black_box(RODATA_PROBE.as_ptr()) as *mut u32;
    // SAFETY: deliberately invalid: the write must fault.
    unsafe { core::ptr::write_volatile(p, 0) };
}

extern "C" fn write_text() {
    let p = core::hint::black_box(write_text as *const () as usize) as *mut u32;
    // SAFETY: deliberately invalid: the write must fault.
    unsafe { core::ptr::write_volatile(p, 0) };
}

extern "C" fn exec_data() {
    let p = core::hint::black_box(core::ptr::addr_of!(DATA_CODE) as usize);
    // SAFETY: the bytes are valid `ret` instructions, but the page is XN so
    // the fetch must fault.
    let f: extern "C" fn() = unsafe { core::mem::transmute(p) };
    f();
}

extern "C" fn exec_stack() {
    let code: [u32; 4] = [0xd65f_03c0; 4];
    let p = core::hint::black_box(code.as_ptr() as usize);
    // SAFETY: as in `exec_data`; the stack is XN.
    let f: extern "C" fn() = unsafe { core::mem::transmute(p) };
    f();
}

extern "C" fn exec_heap() {
    // The last word of the heap; whatever it holds is never fetched because
    // the page is XN.
    let p = core::hint::black_box((boot::heap_region().end - 16) & !0xf);
    // SAFETY: as in `exec_data`.
    let f: extern "C" fn() = unsafe { core::mem::transmute(p) };
    f();
}

extern "C" fn read_past_ram() {
    let p = core::hint::black_box(boot::ram_info().end) as *const u32;
    // SAFETY: deliberately invalid: nothing is mapped there.
    let v = unsafe { core::ptr::read_volatile(p) };
    core::hint::black_box(v);
}

#[inline(never)]
#[allow(unconditional_recursion)] // the point: it recurses until the guard
extern "C" fn recurse(depth: u64) -> u64 {
    let mut buf = [0u8; 256];
    buf[(depth % 256) as usize] = depth as u8;
    let b = core::hint::black_box(&mut buf);
    recurse(depth + 1).wrapping_add(u64::from(b[0]))
}

extern "C" fn stack_overflow() {
    core::hint::black_box(recurse(0));
}

// ---- Checks -----------------------------------------------------------------

struct Report {
    passed: u32,
    failed: u32,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: core::fmt::Arguments<'_>) {
        if ok {
            self.passed += 1;
            crate::println!("kalium: selftest: {}: ok ({})", name, detail);
        } else {
            self.failed += 1;
            crate::println!("kalium: selftest: {}: FAIL ({})", name, detail);
        }
    }
}

/// Expect `case` to raise a data abort permission fault on a write to an
/// address inside `range`.
fn expect_write_fault(r: &mut Report, name: &str, case: extern "C" fn(), range: core::ops::Range<usize>) {
    match run_case(case) {
        None => r.check(name, false, format_args!("no fault")),
        Some(c) => {
            let f = c.fault;
            let ok =
                f.is_data_abort() && f.is_permission_fault() && f.is_write() == Some(true) && range.contains(&(f.far as usize));
            r.check(
                name,
                ok,
                format_args!(
                    "{}{}",
                    f,
                    if range.contains(&(f.far as usize)) { "" } else { ", FAR outside the expected range" }
                ),
            );
        }
    }
}

/// Expect `case` to raise an instruction abort permission fault at an
/// address inside `range`.
fn expect_exec_fault(r: &mut Report, name: &str, case: extern "C" fn(), range: core::ops::Range<usize>) {
    match run_case(case) {
        None => r.check(name, false, format_args!("no fault")),
        Some(c) => {
            let f = c.fault;
            let ok = f.is_instruction_abort() && f.is_permission_fault() && range.contains(&(f.far as usize)) && f.elr == f.far;
            r.check(name, ok, format_args!("{}", f));
        }
    }
}

fn attr_name(attr: u8) -> &'static str {
    match u64::from(attr) {
        mmu::MAIR_NORMAL_WB => "normal write-back",
        mmu::MAIR_DEVICE_NGNRNE => "device-nGnRnE",
        _ => "other",
    }
}

/// Check the attributes of one address with `AT`: readable, writable and
/// of the given memory type.
fn expect_mapping(r: &mut Report, name: &str, va: usize, readable: bool, writable: bool, attr: Option<u64>) {
    let read = mmu::translate(va, false);
    let write = mmu::translate(va, true);
    let read_ok = read.is_ok() == readable;
    let write_ok = write.is_ok() == writable;
    let attr_ok = match (attr, read) {
        (Some(a), Ok(t)) => u64::from(t.memory_attr) == a,
        (Some(_), Err(_)) => false,
        (None, _) => true,
    };
    let detail = match read {
        Ok(t) => format_args!(
            "{:#x}: read {}, write {}, {}",
            va,
            "ok",
            if write.is_ok() { "ok" } else { "refused" },
            attr_name(t.memory_attr)
        ),
        Err(fst) => format_args!(
            "{:#x}: read refused ({}), write {}, {}",
            va,
            super::exceptions::fault_status_name(fst),
            if write.is_ok() { "ok" } else { "refused" },
            "unmapped"
        ),
    };
    r.check(name, read_ok && write_ok && attr_ok, detail);
}

/// Number of `svc` round trips the timing probe averages over.
const ROUND_TRIPS: u64 = 20_000;

/// Average cost of one exception round trip (`svc` from EL1, vector entry
/// with the full register and FP/SIMD save, dispatch, restore, `eret`) in
/// counter ticks, averaged over [`ROUND_TRIPS`] iterations. Hundredths.
pub fn exception_round_trip_centiticks() -> u64 {
    let t0 = timer::counter();
    for _ in 0..ROUND_TRIPS {
        // SAFETY: `svc` from EL1 is a no-op in the exception handler.
        unsafe { core::arch::asm!("svc #0", options(nomem, nostack)) };
    }
    let t1 = timer::counter();
    (t1 - t0) * 100 / ROUND_TRIPS
}

/// Run every check, printing one `kalium: selftest: ...` line per check and
/// a `kalium: selftest PASS`/`FAIL` summary. Returns `true` on PASS.
///
/// Must run with the MMU on and the heap initialized, from thread context
/// (`SP_EL0`), with IRQs in any state.
pub fn run() -> bool {
    let mut r = Report { passed: 0, failed: 0 };
    let text = boot::text_range();
    let rodata = boot::rodata_range();
    let data = boot::data_range();
    let stack = boot::boot_stack_range();
    let guard = boot::boot_stack_guard_range();
    let exc_stack = boot::exception_stack_range();
    let exc_guard = boot::exception_stack_guard_range();
    let heap = boot::heap_region();
    let dtb_area = boot::dtb_area_range();
    let ram = boot::ram_info();
    const UART: usize = 0x0900_0000;

    crate::println!("kalium: selftest: memory map");
    crate::println!("kalium: selftest:   dtb area  {:#x}..{:#x}  RO  XN", dtb_area.start, dtb_area.end);
    crate::println!("kalium: selftest:   .text     {:#x}..{:#x}  RO  X", text.start, text.end);
    crate::println!("kalium: selftest:   .rodata   {:#x}..{:#x}  RO  XN", rodata.start, rodata.end);
    crate::println!("kalium: selftest:   .data/bss {:#x}..{:#x}  RW  XN", data.start, data.end);
    crate::println!("kalium: selftest:   guard     {:#x}..{:#x}  unmapped", guard.start, guard.end);
    crate::println!("kalium: selftest:   stack     {:#x}..{:#x}  RW  XN  (SP_EL0)", stack.start, stack.end);
    crate::println!("kalium: selftest:   guard     {:#x}..{:#x}  unmapped", exc_guard.start, exc_guard.end);
    crate::println!("kalium: selftest:   exc stack {:#x}..{:#x}  RW  XN  (SP_EL1)", exc_stack.start, exc_stack.end);
    crate::println!(
        "kalium: selftest:   heap      {:#x}..{:#x}  RW  XN  (RAM ends at {:#x}, mapped to {:#x})",
        heap.start,
        heap.end,
        ram.end,
        mmu::ram_mapped_end()
    );

    // 1. Attributes as the MMU reports them.
    let normal = Some(mmu::MAIR_NORMAL_WB);
    expect_mapping(&mut r, "at: .text", text.start, true, false, normal);
    expect_mapping(&mut r, "at: .rodata", rodata.start, true, false, normal);
    expect_mapping(&mut r, "at: dtb area", dtb_area.start, true, false, normal);
    expect_mapping(&mut r, "at: .data", data.start, true, true, normal);
    expect_mapping(&mut r, "at: stack", stack.end - 16, true, true, normal);
    expect_mapping(&mut r, "at: stack guard", guard.start, false, false, None);
    expect_mapping(&mut r, "at: exc stack", exc_stack.end - 16, true, true, normal);
    expect_mapping(&mut r, "at: exc stack guard", exc_guard.end - 16, false, false, None);
    expect_mapping(&mut r, "at: heap start", heap.start, true, true, normal);
    expect_mapping(&mut r, "at: heap end", heap.end - 16, true, true, normal);
    // The mapping must end where RAM ends, whatever the alignment of the
    // RAM size: the last page of RAM is heap, the first page past it is not
    // mapped at all (not rounded up to a 2 MiB block).
    r.check(
        "mapping ends at RAM end",
        mmu::ram_mapped_end() == ram.end && heap.end == ram.end,
        format_args!(
            "RAM ends at {:#x}, mapped to {:#x}, heap to {:#x}{}",
            ram.end,
            mmu::ram_mapped_end(),
            heap.end,
            if ram.end.is_multiple_of(2 << 20) { "" } else { ", last block partial" }
        ),
    );
    expect_mapping(&mut r, "at: past RAM", ram.end, false, false, None);
    expect_mapping(&mut r, "at: uart", UART, true, true, Some(mmu::MAIR_DEVICE_NGNRNE));

    // 2. Faults taken for real.
    expect_write_fault(&mut r, "write .rodata", write_rodata, rodata.clone());
    expect_write_fault(&mut r, "write .text", write_text, text.clone());
    expect_exec_fault(&mut r, "exec .data", exec_data, data.clone());
    expect_exec_fault(&mut r, "exec stack", exec_stack, stack.clone());
    expect_exec_fault(&mut r, "exec heap", exec_heap, heap.clone());
    match run_case(read_past_ram) {
        None => r.check("read past RAM", false, format_args!("no fault")),
        Some(c) => {
            let f = c.fault;
            r.check(
                "read past RAM",
                f.is_data_abort() && f.is_translation_fault() && f.far as usize == ram.end,
                format_args!("{}", f),
            );
        }
    }
    match run_case(stack_overflow) {
        None => r.check("stack overflow", false, format_args!("no fault")),
        Some(c) => {
            let f = c.fault;
            let ok = f.is_data_abort()
                && f.is_translation_fault()
                && guard.contains(&(f.far as usize))
                && guard.contains(&(c.sp as usize));
            r.check(
                "stack overflow",
                ok,
                format_args!("{}, SP_EL0 = {:#x} in the guard, reported from the exception stack", f, c.sp),
            );
        }
    }

    // 3. The stack is intact after all that: a function call still works and
    //    the recovery restored SP.
    let sp: u64;
    // SAFETY: reading SP.
    unsafe { core::arch::asm!("mov {0}, sp", out(reg) sp, options(nomem, nostack)) };
    r.check("stack restored", stack.contains(&(sp as usize)), format_args!("SP = {:#x}", sp));

    // 4. Exception cost.
    let centiticks = exception_round_trip_centiticks();
    let ns = centiticks * 1_000_000_000 / (timer::frequency_hz() * 100);
    r.check(
        "exception round trip",
        centiticks > 0,
        format_args!(
            "{}.{:02} ticks ({} ns) per svc, averaged over {} ({} Hz counter)",
            centiticks / 100,
            centiticks % 100,
            ns,
            ROUND_TRIPS,
            timer::frequency_hz()
        ),
    );

    let ok = r.failed == 0;
    crate::println!("kalium: selftest {}: {} passed, {} failed", if ok { "PASS" } else { "FAIL" }, r.passed, r.failed);
    ok
}
