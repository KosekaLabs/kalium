//! Kalium kernel binary. See docs/prototype-plan.md.
#![no_std]
#![no_main]

#[cfg(not(target_arch = "aarch64"))]
compile_error!("the kernel binary only builds for aarch64-unknown-none; use `cargo xtask build`");

#[cfg(target_arch = "aarch64")]
mod imp {
    // Filled in by the HAL and integration work.
}
