//! EL1 exception vectors.
//!
//! All sixteen vectors save the full general-purpose register file plus
//! `ELR_EL1` and `SPSR_EL1` on the current stack, then call
//! [`kalium_exception`] with the vector index and a pointer to the saved
//! [`ExceptionFrame`]. IRQs are dispatched through the GIC; synchronous
//! exceptions and SErrors print `ESR`, `ELR` and `FAR` and panic.

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
}

const FRAME_SIZE: usize = core::mem::size_of::<ExceptionFrame>();
const _: () = assert!(FRAME_SIZE == 272 && FRAME_SIZE.is_multiple_of(16));

global_asm!(
    r#"
    .macro SAVE_CONTEXT
        sub     sp, sp, #272
        stp     x0, x1,   [sp, #0]
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
    .endm

    .macro VECTOR kind
        .balign 0x80
        SAVE_CONTEXT
        mov     x0, #\kind
        b       __kalium_exception_common
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

    .balign 16
__kalium_exception_common:
    mov     x1, sp
    bl      kalium_exception
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
    add     sp, sp, #272
    eret
"#
);

extern "C" {
    static __kalium_vectors: u8;
}

/// Install the vector table in `VBAR_EL1`. Safe to call before the MMU is on.
pub fn init() {
    let vectors = core::ptr::addr_of!(__kalium_vectors) as u64;
    debug_assert_eq!(vectors % 2048, 0);
    super::cpu::write_sysreg!("vbar_el1", vectors);
    cpu::isb();
}

/// Address of the installed vector table.
pub fn vector_table_address() -> usize {
    core::ptr::addr_of!(__kalium_vectors) as usize
}

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

/// Describe the data/instruction fault status code (`ESR.ISS.DFSC/IFSC`).
fn fault_status_name(fsc: u64) -> &'static str {
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

/// Called from the vector stubs. `index` is the vector number (0..16).
#[no_mangle]
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
            report_fatal(source, kind, frame);
        }
    }
}

fn report_fatal(source: ExceptionSource, kind: ExceptionKind, frame: &ExceptionFrame) -> ! {
    let esr = read_sysreg!("esr_el1");
    let far = read_sysreg!("far_el1");
    let ec = (esr >> 26) & 0x3f;
    let iss = esr & 0x1ff_ffff;

    super::console::force_unlock();
    crate::println!("kalium: unhandled {:?} exception ({:?})", kind, source);
    crate::println!(
        "  ESR_EL1 = {:#018x}  EC = {:#04x} ({})  ISS = {:#09x}",
        esr,
        ec,
        exception_class_name(ec),
        iss
    );
    if matches!(ec, 0x20 | 0x21 | 0x24 | 0x25) {
        crate::println!(
            "  fault: {} at FAR_EL1 = {:#018x}{}",
            fault_status_name(iss & 0x3f),
            far,
            if ec == 0x24 || ec == 0x25 {
                if iss & (1 << 6) != 0 {
                    " (write)"
                } else {
                    " (read)"
                }
            } else {
                ""
            }
        );
    } else {
        crate::println!("  FAR_EL1 = {:#018x}", far);
    }
    crate::println!(
        "  ELR_EL1 = {:#018x}  SPSR_EL1 = {:#010x}",
        frame.elr,
        frame.spsr
    );
    for (i, pair) in frame.regs.chunks(2).enumerate() {
        match pair {
            [a, b] => crate::println!(
                "  x{:<2} = {:#018x}  x{:<2} = {:#018x}",
                2 * i,
                a,
                2 * i + 1,
                b
            ),
            [a] => crate::println!("  x{:<2} = {:#018x}", 2 * i, a),
            _ => {}
        }
    }
    panic!("unhandled {:?} exception at {:#x}", kind, frame.elr);
}
