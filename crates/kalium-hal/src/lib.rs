//! Kalium hardware abstraction layer.
//!
//! The only supported platform in M1 is QEMU `virt` on aarch64 (cortex-a72,
//! GICv2, PL011 UART, generic timer, semihosting). Everything in this crate
//! is gated on `target_arch = "aarch64"` and `target_os = "none"`, so on the
//! host (and on aarch64 Linux) it compiles to an empty crate and the
//! workspace build and `cargo test` keep working.
//!
//! Initialization order expected by the kernel:
//!
//! 1. [`console::init`]       – PL011 UART, so panics can print.
//! 2. [`exceptions::init`]    – vector table, so faults are diagnosable.
//! 3. [`mmu::enable`]         – identity map with caches on. Required before
//!    any atomic or allocator use on real hardware.
//! 4. [`gic::init`]           – GICv2 distributor and CPU interface.
//! 5. [`timer::init`]         – generic timer, PPI 30 routed through the GIC.
//!
//! Then [`enable_irqs`], program the timer with [`timer::set_timer`] and sleep
//! with [`wait_for_interrupt`]. [`exit`] terminates QEMU with an exit code.
#![no_std]
#![cfg_attr(not(all(target_arch = "aarch64", target_os = "none")), allow(unused))]

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod aarch64;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use aarch64::{
    boot::{boot_el, dtb_address, heap_region, kernel_image_range},
    console, cpu,
    cpu::{disable_irqs, enable_irqs, irqs_enabled, wait_for_interrupt},
    exceptions, exit,
    exit::exit,
    gic, mmu, timer,
    timer::{now_ns, set_tick_handler, set_timer},
};
