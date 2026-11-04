//! Terminating the machine.
//!
//! [`exit`] first tries ARM semihosting `SYS_EXIT` (`0x18`) with an
//! `ADP_Stopped_ApplicationExit` parameter block, which makes QEMU run with
//! `-semihosting-config enable=on,target=native` exit with the given code.
//! If that returns (semihosting disabled), it falls back to PSCI
//! `SYSTEM_OFF` through the conduit matching the boot EL (HVC when started
//! at EL1 under QEMU's built-in PSCI, SMC when started at EL2), and finally
//! parks the core.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use super::{boot, console, cpu};

const SYS_EXIT: u64 = 0x18;
const ADP_STOPPED_APPLICATION_EXIT: u64 = 0x20026;
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;

static EXITING: AtomicBool = AtomicBool::new(false);

/// True once [`exit`] has started; the exception handler uses it to avoid
/// panicking when a fallback instruction turns out to be undefined.
pub fn is_exiting() -> bool {
    EXITING.load(Ordering::Relaxed)
}

/// Stop the machine with `code`. Never returns.
pub fn exit(code: u32) -> ! {
    cpu::disable_irqs();
    EXITING.store(true, Ordering::Relaxed);
    console::flush();

    let block: [u64; 2] = [ADP_STOPPED_APPLICATION_EXIT, u64::from(code)];
    // SAFETY: `hlt #0xf000` is the AArch64 semihosting trap. QEMU handles it
    // and exits; without semihosting it raises an exception, which the
    // vector code recognizes via `is_exiting` and parks the core.
    unsafe {
        asm!(
            "hlt #0xf000",
            in("w0") SYS_EXIT as u32,
            in("x1") block.as_ptr(),
            options(nostack)
        );
    }

    // Semihosting returned: fall back to PSCI SYSTEM_OFF.
    // SAFETY: PSCI calls only clobber x0-x3 and never return on success.
    unsafe {
        if boot::boot_el() >= 2 {
            asm!("smc #0", inout("x0") PSCI_SYSTEM_OFF => _, out("x1") _, out("x2") _, out("x3") _, options(nostack));
        } else {
            asm!("hvc #0", inout("x0") PSCI_SYSTEM_OFF => _, out("x1") _, out("x2") _, out("x3") _, options(nostack));
        }
    }

    cpu::halt()
}
