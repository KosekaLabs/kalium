//! EL1 exception vectors.
//!
//! All sixteen vectors save the full general-purpose register file, the
//! FP/SIMD state (`q0`–`q31`, `FPCR`, `FPSR`), `ELR_EL1` and `SPSR_EL1` on
//! the current stack, then call [`kalium_exception`] with the vector index
//! and a pointer to the saved [`ExceptionFrame`]. The kernel runs on
//! `SP_EL0`, so an exception taken from it lands on `SP_EL1`, the dedicated
//! exception stack (see `boot.rs`).
//!
//! The FP/SIMD save matters: Rust's aarch64 codegen uses the `q` registers
//! for copies and the interpreter keeps `f32`/`f64` operands in them, so an
//! IRQ handler compiled by the same compiler would otherwise clobber the
//! interrupted computation.
//!
//! IRQs are dispatched through the GIC. `svc` from EL1 is a no-op (it is the
//! exception-round-trip probe of the HAL self-test). Any other synchronous
//! exception or SError is a kernel fault: the handler prints the decoded
//! exception class, fault type, `FAR`, `ELR`, the stack pointers and the
//! register file, then exits QEMU with code 2 so that a kernel fault is told
//! apart from a scenario failure (1).

use core::arch::global_asm;

use super::cpu::read_sysreg;
use super::{cpu, exit, gic};

/// Registers saved on exception entry, in the order the assembly stores them.
#[repr(C)]
#[derive(Debug)]
pub struct ExceptionFrame {
    /// `x0`–`x30`.
    pub regs: [u64; 31],
    /// `ELR_EL1`: the return address.
    pub elr: u64,
    /// `SPSR_EL1`: the saved state.
    pub spsr: u64,
    _pad: u64,
    /// `FPCR`.
    pub fpcr: u64,
    /// `FPSR`.
    pub fpsr: u64,
    /// `q0`–`q31`.
    pub vregs: [u128; 32],
}

const FRAME_SIZE: usize = core::mem::size_of::<ExceptionFrame>();
const _: () = assert!(FRAME_SIZE == 800 && FRAME_SIZE.is_multiple_of(16));
const _: () = assert!(core::mem::offset_of!(ExceptionFrame, elr) == 248);
const _: () = assert!(core::mem::offset_of!(ExceptionFrame, spsr) == 256);
const _: () = assert!(core::mem::offset_of!(ExceptionFrame, fpcr) == 272);
const _: () = assert!(core::mem::offset_of!(ExceptionFrame, fpsr) == 280);
const _: () = assert!(core::mem::offset_of!(ExceptionFrame, vregs) == 288);

global_asm!(
    r#"
    // Each vector slot is 0x80 bytes (32 instructions). The stub only makes
    // room for the frame, saves the two registers it needs as scratch and
    // branches to the common entry. `init` checks that the table is exactly
    // sixteen slots, so a stub that outgrows its slot (which `.balign` would
    // silently push into the next one) is caught at boot.
    .macro VECTOR kind
        .balign 0x80
        sub     sp, sp, #800
        stp     x0, x1,   [sp, #0]
        mov     x0, #\kind
        b       __kalium_exception_entry
    .endm

    .section .text.vectors, "ax"
    .balign 2048
    .global __kalium_vectors
__kalium_vectors:
    // Current EL with SP0: sync, IRQ, FIQ, SError.
    VECTOR 0
    VECTOR 1
    VECTOR 2
    VECTOR 3
    // Current EL with SPx.
    VECTOR 4
    VECTOR 5
    VECTOR 6
    VECTOR 7
    // Lower EL, AArch64.
    VECTOR 8
    VECTOR 9
    VECTOR 10
    VECTOR 11
    // Lower EL, AArch32.
    VECTOR 12
    VECTOR 13
    VECTOR 14
    VECTOR 15
    .balign 0x80
    .global __kalium_vectors_end
__kalium_vectors_end:

    // x0 = vector index, x0/x1 already saved, sp = frame.
__kalium_exception_entry:
    stp     x2, x3,   [sp, #16]
    stp     x4, x5,   [sp, #32]
    stp     x6, x7,   [sp, #48]
    stp     x8, x9,   [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    mrs     x1, elr_el1
    mrs     x2, spsr_el1
    stp     x30, x1,  [sp, #240]
    str     x2,       [sp, #256]
    mrs     x1, fpcr
    mrs     x2, fpsr
    stp     x1, x2,   [sp, #272]
    stp     q0, q1,   [sp, #288]
    stp     q2, q3,   [sp, #320]
    stp     q4, q5,   [sp, #352]
    stp     q6, q7,   [sp, #384]
    stp     q8, q9,   [sp, #416]
    stp     q10, q11, [sp, #448]
    stp     q12, q13, [sp, #480]
    stp     q14, q15, [sp, #512]
    stp     q16, q17, [sp, #544]
    stp     q18, q19, [sp, #576]
    stp     q20, q21, [sp, #608]
    stp     q22, q23, [sp, #640]
    stp     q24, q25, [sp, #672]
    stp     q26, q27, [sp, #704]
    stp     q28, q29, [sp, #736]
    stp     q30, q31, [sp, #768]

    mov     x1, sp
    bl      kalium_exception

    ldp     x1, x2,   [sp, #272]
    msr     fpcr, x1
    msr     fpsr, x2
    ldp     q0, q1,   [sp, #288]
    ldp     q2, q3,   [sp, #320]
    ldp     q4, q5,   [sp, #352]
    ldp     q6, q7,   [sp, #384]
    ldp     q8, q9,   [sp, #416]
    ldp     q10, q11, [sp, #448]
    ldp     q12, q13, [sp, #480]
    ldp     q14, q15, [sp, #512]
    ldp     q16, q17, [sp, #544]
    ldp     q18, q19, [sp, #576]
    ldp     q20, q21, [sp, #608]
    ldp     q22, q23, [sp, #640]
    ldp     q24, q25, [sp, #672]
    ldp     q26, q27, [sp, #704]
    ldp     q28, q29, [sp, #736]
    ldp     q30, q31, [sp, #768]
    ldr     x2,       [sp, #256]
    ldp     x30, x1,  [sp, #240]
    msr     elr_el1, x1
    msr     spsr_el1, x2
    ldp     x0, x1,   [sp, #0]
    ldp     x2, x3,   [sp, #16]
    ldp     x4, x5,   [sp, #32]
    ldp     x6, x7,   [sp, #48]
    ldp     x8, x9,   [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    add     sp, sp, #800
    eret
"#
);

unsafe extern "C" {
    static __kalium_vectors: u8;
    static __kalium_vectors_end: u8;
}

/// Sixteen vectors of 0x80 bytes.
const VECTOR_TABLE_SIZE: usize = 16 * 0x80;

/// Install the vector table in `VBAR_EL1`. Safe to call before the MMU is on.
///
/// Panics if the table is not 2 KiB aligned or not exactly sixteen 0x80-byte
/// slots (a stub that outgrew its slot would shift every vector after it).
pub fn init() {
    let vectors = core::ptr::addr_of!(__kalium_vectors) as usize;
    let end = core::ptr::addr_of!(__kalium_vectors_end) as usize;
    assert!(
        vectors.is_multiple_of(2048) && end - vectors == VECTOR_TABLE_SIZE,
        "exception vector table misplaced: {:#x}..{:#x}",
        vectors,
        end
    );
    super::cpu::write_sysreg!("vbar_el1", vectors as u64);
    cpu::isb();
}

/// Address of the installed vector table.
pub fn vector_table_address() -> usize {
    core::ptr::addr_of!(__kalium_vectors) as usize
}

/// Exit code QEMU reports for a kernel fault (an unhandled synchronous
/// exception or SError in kernel code). Panics exit with 1.
pub const KERNEL_FAULT_EXIT_CODE: u32 = 2;

/// Which vector fired, decoded from the vector index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExceptionKind {
    Synchronous,
    Irq,
    Fiq,
    SError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExceptionSource {
    CurrentElSp0,
    CurrentElSpx,
    LowerElAArch64,
    LowerElAArch32,
}

fn decode_vector(index: u64) -> (ExceptionSource, ExceptionKind) {
    let source = match index / 4 {
        0 => ExceptionSource::CurrentElSp0,
        1 => ExceptionSource::CurrentElSpx,
        2 => ExceptionSource::LowerElAArch64,
        _ => ExceptionSource::LowerElAArch32,
    };
    let kind = match index % 4 {
        0 => ExceptionKind::Synchronous,
        1 => ExceptionKind::Irq,
        2 => ExceptionKind::Fiq,
        _ => ExceptionKind::SError,
    };
    (source, kind)
}

/// `ESR_EL1.EC` values this module interprets.
pub mod ec {
    pub const SVC_AARCH64: u64 = 0x15;
    pub const INSTRUCTION_ABORT_LOWER_EL: u64 = 0x20;
    pub const INSTRUCTION_ABORT_CURRENT_EL: u64 = 0x21;
    pub const DATA_ABORT_LOWER_EL: u64 = 0x24;
    pub const DATA_ABORT_CURRENT_EL: u64 = 0x25;
}

/// Human-readable name of an `ESR_EL1.EC` exception class.
pub fn exception_class_name(ec: u64) -> &'static str {
    match ec {
        0x00 => "unknown reason",
        0x01 => "trapped WFI/WFE",
        0x07 => "trapped FP/SIMD access (CPACR_EL1.FPEN)",
        0x0e => "illegal execution state",
        0x15 => "SVC from AArch64",
        0x16 => "HVC from AArch64",
        0x17 => "SMC from AArch64",
        0x18 => "trapped system register access",
        0x20 => "instruction abort from lower EL",
        0x21 => "instruction abort from current EL",
        0x22 => "PC alignment fault",
        0x24 => "data abort from lower EL",
        0x25 => "data abort from current EL",
        0x26 => "SP alignment fault",
        0x2c => "trapped floating-point exception",
        0x2f => "SError",
        0x30 | 0x31 => "breakpoint",
        0x32 | 0x33 => "software step",
        0x34 | 0x35 => "watchpoint",
        0x3c => "BRK instruction",
        _ => "other",
    }
}

/// The kind of a data/instruction fault status code (`ESR.ISS.DFSC/IFSC`),
/// without the level.
pub fn fault_status_name(fsc: u64) -> &'static str {
    match fsc {
        0x00..=0x03 => "address size fault",
        0x04..=0x07 => "translation fault",
        0x09..=0x0b => "access flag fault",
        0x0d..=0x0f => "permission fault",
        0x10 => "synchronous external abort",
        0x11 => "synchronous tag check fault",
        0x14..=0x17 => "external abort on translation table walk",
        0x21 => "alignment fault",
        0x30 => "TLB conflict abort",
        _ => "other",
    }
}

/// A decoded synchronous exception or SError.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncFault {
    pub esr: u64,
    pub far: u64,
    pub elr: u64,
    /// `ESR_EL1.EC`.
    pub ec: u64,
    /// `ESR_EL1.ISS`.
    pub iss: u64,
}

impl SyncFault {
    /// Read the fault registers for the exception described by `frame`.
    pub fn current(frame: &ExceptionFrame) -> Self {
        let esr = read_sysreg!("esr_el1");
        SyncFault { esr, far: read_sysreg!("far_el1"), elr: frame.elr, ec: (esr >> 26) & 0x3f, iss: esr & 0x1ff_ffff }
    }

    pub fn is_data_abort(&self) -> bool {
        matches!(self.ec, ec::DATA_ABORT_LOWER_EL | ec::DATA_ABORT_CURRENT_EL)
    }

    pub fn is_instruction_abort(&self) -> bool {
        matches!(self.ec, ec::INSTRUCTION_ABORT_LOWER_EL | ec::INSTRUCTION_ABORT_CURRENT_EL)
    }

    /// The fault status code (`DFSC`/`IFSC`) for aborts.
    pub fn fault_status(&self) -> Option<u64> {
        (self.is_data_abort() || self.is_instruction_abort()).then_some(self.iss & 0x3f)
    }

    pub fn is_permission_fault(&self) -> bool {
        matches!(self.fault_status(), Some(0x0d..=0x0f))
    }

    pub fn is_translation_fault(&self) -> bool {
        matches!(self.fault_status(), Some(0x04..=0x07))
    }

    /// Translation table level of a translation/access-flag/permission fault.
    pub fn fault_level(&self) -> Option<u64> {
        match self.fault_status() {
            Some(fsc @ (0x04..=0x07 | 0x09..=0x0b | 0x0d..=0x0f)) => Some(fsc & 0b11),
            _ => None,
        }
    }

    /// For data aborts: was the faulting access a write (`ISS.WnR`)?
    pub fn is_write(&self) -> Option<bool> {
        self.is_data_abort().then_some(self.iss & (1 << 6) != 0)
    }
}

impl core::fmt::Display for SyncFault {
    /// `data abort (permission fault, level 3, write) at FAR_EL1 = 0x..,
    /// ELR_EL1 = 0x..`, or the exception class name for anything else.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(fsc) = self.fault_status() {
            let what = if self.is_data_abort() { "data abort" } else { "instruction abort" };
            write!(f, "{} ({}", what, fault_status_name(fsc))?;
            if let Some(level) = self.fault_level() {
                write!(f, ", level {}", level)?;
            }
            match self.is_write() {
                Some(true) => write!(f, ", write")?,
                Some(false) => write!(f, ", read")?,
                None => {}
            }
            write!(f, ") at FAR_EL1 = {:#x}, ELR_EL1 = {:#x}", self.far, self.elr)
        } else {
            write!(f, "{} at ELR_EL1 = {:#x}", exception_class_name(self.ec), self.elr)
        }
    }
}

/// Called from the vector stubs. `index` is the vector number (0..16).
#[unsafe(no_mangle)]
extern "C" fn kalium_exception(index: u64, frame: &mut ExceptionFrame) {
    let (source, kind) = decode_vector(index);
    match kind {
        ExceptionKind::Irq | ExceptionKind::Fiq => gic::handle_irq(),
        ExceptionKind::Synchronous | ExceptionKind::SError => {
            if exit::is_exiting() && kind == ExceptionKind::Synchronous {
                // The exit path executed an instruction the platform does
                // not implement (`hlt #0xf000` without semihosting, or a
                // PSCI call on the wrong conduit). Skip it so `exit` can try
                // the next method.
                frame.elr = frame.elr.wrapping_add(4);
                return;
            }
            if exit::is_exiting() {
                cpu::halt();
            }
            let fault = SyncFault::current(frame);
            if kind == ExceptionKind::Synchronous && fault.ec == ec::SVC_AARCH64 {
                // `svc` from EL1 is a no-op (the self-test's round-trip probe).
                return;
            }
            #[cfg(feature = "hal-selftest")]
            if kind == ExceptionKind::Synchronous && super::selftest::try_recover(frame, &fault) {
                return;
            }
            report_fatal(source, kind, frame, &fault);
        }
    }
}

fn report_fatal(source: ExceptionSource, kind: ExceptionKind, frame: &ExceptionFrame, fault: &SyncFault) -> ! {
    super::console::force_unlock();
    crate::println!("kalium: unhandled {:?} exception ({:?})", kind, source);
    crate::println!("kalium: kernel fault: {}", fault);
    crate::println!(
        "  ESR_EL1 = {:#018x}  EC = {:#04x} ({})  ISS = {:#09x}",
        fault.esr,
        fault.ec,
        exception_class_name(fault.ec),
        fault.iss
    );
    crate::println!("  ELR_EL1 = {:#018x}  FAR_EL1 = {:#018x}  SPSR_EL1 = {:#010x}", frame.elr, fault.far, frame.spsr);
    let sp_el0 = read_sysreg!("sp_el0");
    let frame_addr = frame as *const ExceptionFrame as u64;
    crate::println!(
        "  SP_EL0  = {:#018x}  SP_EL1 = {:#018x} ({})",
        sp_el0,
        frame_addr + FRAME_SIZE as u64,
        match source {
            ExceptionSource::CurrentElSp0 => "faulted on SP_EL0, reporting on the exception stack",
            ExceptionSource::CurrentElSpx => "faulted on SP_EL1: nested exception",
            _ => "from a lower EL",
        }
    );
    for (i, pair) in frame.regs.chunks(2).enumerate() {
        match pair {
            [a, b] => crate::println!("  x{:<2} = {:#018x}  x{:<2} = {:#018x}", 2 * i, a, 2 * i + 1, b),
            [a] => crate::println!("  x{:<2} = {:#018x}", 2 * i, a),
            _ => {}
        }
    }
    crate::println!("kalium: kernel fault, exiting with code {}", KERNEL_FAULT_EXIT_CODE);
    exit::exit(KERNEL_FAULT_EXIT_CODE)
}
