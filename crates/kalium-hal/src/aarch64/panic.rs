//! Panic handler: print location and message, then exit with code 1.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use super::{console, cpu, exit};

static PANICKING: AtomicBool = AtomicBool::new(false);

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    cpu::disable_irqs();
    if PANICKING.swap(true, Ordering::Relaxed) {
        // A panic while reporting a panic: give up printing.
        exit::exit(1);
    }
    // The panicking context may hold the console lock.
    console::force_unlock();
    match info.location() {
        Some(loc) => console::_print_unlocked(format_args!(
            "\nkalium: panic at {}:{}:{}: {}\n",
            loc.file(),
            loc.line(),
            loc.column(),
            info.message()
        )),
        None => console::_print_unlocked(format_args!("\nkalium: panic: {}\n", info.message())),
    }
    exit::exit(1)
}
