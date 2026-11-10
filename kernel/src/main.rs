//! Kalium kernel binary: the M1 prototype on QEMU `virt` (aarch64).
//!
//! `kmain` is called by the HAL boot code (`_start` in `kalium-hal`) on core
//! 0 at EL1 with the stack set and `.bss` zeroed. It brings the hardware up,
//! initializes the heap, loads the boot image, creates the `init` instance
//! with its two root capabilities and hands over to the run loop, which runs
//! until `init` exits and then leaves QEMU through semihosting.
//!
//! # Structure
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`boot_image`] | the Wasm modules embedded by `build.rs` from `target/wasm/` |
//! | [`kernel`] | the data model: object registry (`Object`: console, spawn authority, channels, memory objects, instances), instances (`Inst`: cap table, Wasm instance + execution, thread state), capability reference counting, parking/waking, instance creation and exit |
//! | [`host`] | the host ABI: one `kalium_wasm::Host` implementation dispatching on `HostFuncId` to every function in `kalium:core`, `kalium:ipc`, `kalium:memory` and `kalium:spawn` |
//! | [`wasi`] | the WASI preview1 compatibility layer: `wasi_snapshot_preview1` resolved for every instance, standard streams mapped onto the console and channel capabilities in slots 0 and 1 |
//! | [`run`] | the run loop: `pick_next` → fuel from the slice → `Execution::run` in chunks → `RunOutcome` → `account`; timer programming; fuel/time calibration; shutdown |
//!
//! # Concurrency model
//!
//! Single core, one thread per instance, no SMP. All kernel state is one
//! [`kernel::Kernel`] value owned by `kmain`'s stack frame and borrowed by
//! the run loop and, during host calls, by the `Host` implementation. The
//! only interrupt is the timer tick, whose handler sets an atomic preempt
//! flag; the run loop checks it between interpreter chunks. The console and
//! heap locks mask IRQs while held.
//!
//! # Blocking
//!
//! A host call that cannot complete parks the thread on the wait queue of
//! the object it needs (`kalium_ipc::WaitQueue`), tells the scheduler the
//! thread is blocked (or sleeping) and returns `HostOutcome::Block`. The
//! interpreter rewinds to the call, so when something wakes the thread
//! (a send, a channel closing, an instance exiting, the timer for a
//! sleeper) the call is re-issued and re-evaluated from scratch.
//!
//! # Boot-only mode and the HAL self-test
//!
//! With the `boot-only` cargo feature the kernel is the HAL boot test only:
//! bring the hardware up, take ten 1 ms timer ticks, exit 0. `cargo xtask
//! boot-test` builds and checks that. With `hal-selftest` the kernel runs
//! `kalium_hal::selftest::run` right after the heap is up (in either mode)
//! and exits 1 if it fails; `cargo xtask selftest` builds and checks that
//! together with `boot-only`.
#![no_std]
#![no_main]

#[cfg(not(target_arch = "aarch64"))]
// trunk-ignore(clippy/compiler): Trunk lints for the host; `cargo xtask clippy` lints the kernel for aarch64.
compile_error!("the kernel binary only builds for aarch64-unknown-none; use `cargo xtask build`");

#[cfg(target_arch = "aarch64")]
extern crate alloc;

#[cfg(all(target_arch = "aarch64", not(feature = "boot-only")))]
mod boot_image;
#[cfg(all(target_arch = "aarch64", not(feature = "boot-only")))]
mod host;
#[cfg(all(target_arch = "aarch64", not(feature = "boot-only")))]
mod kernel;
#[cfg(all(target_arch = "aarch64", not(feature = "boot-only")))]
mod run;
#[cfg(all(target_arch = "aarch64", not(feature = "boot-only")))]
mod wasi;

#[cfg(target_arch = "aarch64")]
mod imp {
    use kalium_hal::{println, timer};
    use kalium_mm::KernelHeap;

    #[global_allocator]
    static HEAP: KernelHeap = KernelHeap::new();

    /// Bring up the HAL in the order the modules require, then the heap.
    fn init_hal() {
        kalium_hal::console::init();
        kalium_hal::exceptions::init();
        kalium_hal::mmu::enable();
        kalium_hal::gic::init();
        kalium_hal::timer::init();
    }

    fn init_heap() -> usize {
        let heap = kalium_hal::heap_region();
        let size = heap.end - heap.start;
        // SAFETY: `[__heap_start, end of RAM)` is reserved for the heap by
        // the linker script and the HAL's RAM sizing; nothing else uses it,
        // and the MMU maps it as normal read-write memory.
        if let Err(e) = unsafe { HEAP.init(heap.start, size) } {
            panic!("heap init failed: {e:?}");
        }
        size
    }

    fn print_banner() {
        println!("kalium: boot");
        println!("kalium: timer frequency {} Hz", timer::frequency_hz());
        let ram = kalium_hal::ram_info();
        match ram.source {
            kalium_hal::RamSource::DeviceTree => println!(
                "kalium: entered at EL{}, dtb at {:#x} ({} bytes), RAM {:#x}..{:#x} ({} MiB) from the device tree",
                kalium_hal::boot_el(),
                ram.dtb,
                ram.dtb_size,
                ram.start,
                ram.end,
                ram.size() >> 20
            ),
            kalium_hal::RamSource::LinkerDefault => println!(
                "kalium: entered at EL{}, no device tree (x0 = {:#x}): assuming RAM {:#x}..{:#x} ({} MiB), the linker default",
                kalium_hal::boot_el(),
                kalium_hal::boot::boot_x0(),
                ram.start,
                ram.end,
                ram.size() >> 20
            ),
        }
        let heap = kalium_hal::heap_region();
        let image = kalium_hal::kernel_image_range();
        println!(
            "kalium: image {:#x}..{:#x} (W^X: text RX, rodata RO, data/stacks RW, guards unmapped), heap {:#x}..{:#x} ({} MiB)",
            image.start,
            image.end,
            heap.start,
            heap.end,
            (heap.end - heap.start) >> 20
        );
    }

    /// Run the HAL hardening self-test; exit 1 if it fails.
    #[cfg(feature = "hal-selftest")]
    fn selftest() {
        if !kalium_hal::selftest::run() {
            kalium_hal::exit(1);
        }
    }

    #[cfg(not(feature = "hal-selftest"))]
    fn selftest() {}

    #[cfg(feature = "boot-only")]
    mod boot_only {
        use core::sync::atomic::{AtomicU32, Ordering};

        use kalium_hal::{println, timer};

        /// Tick period of the boot-test timer.
        const TICK_NS: u64 = 1_000_000;
        /// Number of ticks the boot test waits for before exiting.
        const BOOT_TICKS: u32 = 10;

        static TICKS: AtomicU32 = AtomicU32::new(0);

        /// Runs in IRQ context on every timer interrupt; re-arms the timer
        /// until enough ticks have been counted.
        fn on_tick() {
            let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
            if n < BOOT_TICKS {
                timer::set_timer(TICK_NS);
            }
        }

        /// Arm a periodic 1 ms tick and sleep until `BOOT_TICKS` have arrived.
        pub fn run() -> ! {
            timer::set_tick_handler(on_tick);
            kalium_hal::enable_irqs();
            timer::set_timer(TICK_NS);
            while TICKS.load(Ordering::Relaxed) < BOOT_TICKS {
                kalium_hal::wait_for_interrupt();
            }
            kalium_hal::disable_irqs();
            println!("kalium: {} ticks", BOOT_TICKS);
            // Exercise the heap once so the boot test covers it.
            let v: alloc::vec::Vec<u32> = (0..1024).collect();
            println!("kalium: heap ok ({} words)", v.len());
            kalium_hal::exit(0)
        }
    }

    #[cfg(not(feature = "boot-only"))]
    mod full {
        use kalium_abi::rights;
        use kalium_cap::{CapTable, ObjectKind, Rights};
        use kalium_hal::{println, timer};
        use kalium_sched::Class;
        use kalium_wasm::Limits;

        use crate::boot_image::BOOT_IMAGE;
        use crate::kernel::{CAP_TABLE_LIMIT, HEAP_HEADROOM, Kernel};

        /// Instance limits; `max_memory_pages` shrinks on a small heap.
        fn limits(heap_bytes: usize) -> Limits {
            let pages_for_heap = (heap_bytes / 8 / kalium_wasm::PAGE_SIZE) as u32;
            Limits {
                max_memory_pages: 256.min(pages_for_heap.max(4)),
                max_stack_values: 65536,
                max_call_depth: 1024,
                max_table_elements: 65536,
            }
        }

        pub fn run(heap_bytes: usize) -> ! {
            if let Err(e) = crate::host::check_table() {
                panic!("{e}");
            }
            println!("kalium: boot image: {} modules", BOOT_IMAGE.len());
            for (i, m) in BOOT_IMAGE.iter().enumerate() {
                println!("kalium:   module {}: {} ({} bytes)", i, m.name, m.bytes.len());
            }
            let limits = limits(heap_bytes);
            println!(
                "kalium: instance limits: {} pages, {} stack values, call depth {}, {} cap slots",
                limits.max_memory_pages, limits.max_stack_values, limits.max_call_depth, CAP_TABLE_LIMIT
            );
            // What instances may take of the heap between them; the rest is
            // the kernel's (`kernel.rs`, "Quotas and the instance budget").
            let instance_bytes = heap_bytes.saturating_sub(HEAP_HEADROOM);
            println!(
                "kalium: instance quotas: {} MiB memory objects, {} MiB snapshots, {} channels; budget {} MiB for all instances ({} MiB kernel headroom)",
                kalium_abi::limits::MAX_MEMOBJ_BYTES_PER_INSTANCE >> 20,
                kalium_abi::limits::MAX_SNAPSHOT_BYTES_PER_INSTANCE >> 20,
                kalium_abi::limits::MAX_CHANNELS_PER_INSTANCE,
                instance_bytes >> 20,
                HEAP_HEADROOM >> 20
            );

            let mut k = Kernel::new(&BOOT_IMAGE, limits, instance_bytes);
            let mut caps = CapTable::new(CAP_TABLE_LIMIT);
            let console =
                k.mint_root_into(&mut caps, k.console, ObjectKind::Console, Rights::from_bits_truncate(rights::INIT_CONSOLE));
            let spawner =
                k.mint_root_into(&mut caps, k.spawner, ObjectKind::Spawn, Rights::from_bits_truncate(rights::INIT_SPAWN));
            if console.is_err() || spawner.is_err() {
                panic!("cannot create init's capabilities");
            }
            let now = timer::now_ns();
            match k.create_instance(0, caps, Class::Fair { weight: 1 }, now) {
                Ok(id) => println!("kalium: created instance {} (init)", id),
                Err((e, _)) => {
                    println!("kalium: cannot create init: {e:?}");
                    kalium_hal::exit(1);
                }
            }
            crate::run::run(&mut k)
        }
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn kmain() -> ! {
        init_hal();
        print_banner();
        let heap_bytes = init_heap();
        selftest();
        #[cfg(feature = "boot-only")]
        {
            let _ = heap_bytes;
            boot_only::run()
        }
        #[cfg(not(feature = "boot-only"))]
        {
            full::run(heap_bytes)
        }
    }
}
