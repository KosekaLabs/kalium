//! Kalium kernel binary. See docs/prototype-plan.md.
//!
//! `kmain` is called by the HAL boot code (`_start` in `kalium-hal`) on core 0
//! at EL1 with the stack set and `.bss` zeroed. The current scenario is the
//! HAL boot test: bring the hardware up, take ten timer ticks, exit 0.
#![no_std]
#![no_main]

#[cfg(not(target_arch = "aarch64"))]
compile_error!("the kernel binary only builds for aarch64-unknown-none; use `cargo xtask build`");

#[cfg(target_arch = "aarch64")]
mod imp {
    use core::sync::atomic::{AtomicU32, Ordering};

    use kalium_hal::{println, timer};

    /// Tick period of the boot-test timer.
    const TICK_NS: u64 = 1_000_000;
    /// Number of ticks the boot test waits for before exiting.
    const BOOT_TICKS: u32 = 10;

    static TICKS: AtomicU32 = AtomicU32::new(0);

    /// Runs in IRQ context on every timer interrupt; re-arms the timer until
    /// enough ticks have been counted.
    fn on_tick() {
        let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
        if n < BOOT_TICKS {
            timer::set_timer(TICK_NS);
        }
    }

    /// Bring up the HAL in the order the modules require.
    fn init_hal() {
        kalium_hal::console::init();
        kalium_hal::exceptions::init();
        kalium_hal::mmu::enable();
        kalium_hal::gic::init();
        kalium_hal::timer::init();
    }

    fn print_banner() {
        println!("kalium: boot");
        println!("kalium: timer frequency {} Hz", timer::frequency_hz());
        let heap = kalium_hal::heap_region();
        println!(
            "kalium: entered at EL{}, dtb at {:#x}, heap {:#x}..{:#x} ({} MiB)",
            kalium_hal::boot_el(),
            kalium_hal::dtb_address(),
            heap.start,
            heap.end,
            (heap.end - heap.start) >> 20
        );
    }

    /// Arm a periodic 1 ms tick and sleep until `BOOT_TICKS` have arrived.
    fn run_boot_test() {
        timer::set_tick_handler(on_tick);
        kalium_hal::enable_irqs();
        timer::set_timer(TICK_NS);
        while TICKS.load(Ordering::Relaxed) < BOOT_TICKS {
            kalium_hal::wait_for_interrupt();
        }
        kalium_hal::disable_irqs();
        println!("kalium: {} ticks", BOOT_TICKS);
    }

    #[no_mangle]
    pub extern "C" fn kmain() -> ! {
        init_hal();
        print_banner();
        run_boot_test();
        kalium_hal::exit(0)
    }
}
