//! The boot image: the SDK example modules embedded at build time.
//!
//! `build.rs` generates `BOOT_IMAGE` from `target/wasm/<name>.wasm` in the
//! plan's fixed order (`init=0, ping=1, pong=2, rt=3, fair=4, faulty=5`),
//! followed by the hostile-service test pair (`park=6, greedy=7`).
//! Module indices passed to `spawn` index this table.

/// One module of the boot image.
#[derive(Clone, Copy)]
pub struct BootModule {
    /// The example's name (`init`, `ping`, ...), used in kernel messages.
    pub name: &'static str,
    /// The Wasm binary.
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/boot_image.rs"));
