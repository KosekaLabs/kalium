//! `greedy`: a hostile spawner. Floods the kernel with `park` instances,
//! dropping each `Instance` capability right away so that nothing in its own
//! capability table bounds the loop, until `spawn` is refused with `ENOMEM`
//! at the kernel-wide instance limit (`abi::limits::MAX_INSTANCES`). Then it
//! lets the children exit, spawns once more to show that their slots are
//! reclaimed, and exits 0.
//!
//! Each child blocks on a receive end derived from one channel; dropping the
//! sending end releases them all at once, so the count the kernel refuses
//! at is exact and nothing in this test waits on the clock.
//!
//! Prints `greedy: spawn refused (ENOMEM) after N instances` and
//! `greedy: slot reclaimed, park exited 0`.
//!
//! Initial capabilities: slot 0 `Console(WRITE)`, slot 1 `Spawn(SPAWN|TRANSFER)`.
#![no_std]

use kalium_sdk::abi::limits::MAX_INSTANCES;
use kalium_sdk::{
    cap_drop, exit, rights, spawn, yield_now, Cap, Channel, Class, Console, Errno, Instance, Line,
};

/// Boot image index of `park`.
const MOD_PARK: u32 = 6;
/// `init` and this instance are live too, so this many children fit.
const EXPECTED: u32 = MAX_INSTANCES as u32 - 2;
/// Well past the limit: the kernel is broken if `spawn` still succeeds.
const GIVE_UP: u32 = MAX_INSTANCES as u32 * 4;
/// Yields granted to the released children before giving up on a slot.
const RECLAIM_TRIES: u32 = 10_000;

#[no_mangle]
pub extern "C" fn _start() {
    let console = Console::from_cap(Cap::slot(0));
    let spawner = Cap::slot(1);

    // Phase 1: spawn and forget until the kernel says no.
    let (tx, rx) = Channel::create().unwrap_or_else(|e| fail(console, "channel_create", e));
    let mut spawned: u32 = 0;
    loop {
        let child_rx = rx
            .cap()
            .derive(rights::RECV | rights::TRANSFER)
            .unwrap_or_else(|e| fail(console, "cap_derive", e));
        match spawn(spawner, MOD_PARK, &[child_rx], Class::Fair, 0, 0) {
            Ok(child) => {
                // Hostile: forget the child. The kernel must bound the loop
                // without this capability.
                let _ = cap_drop(child.cap());
                spawned += 1;
                if spawned >= GIVE_UP {
                    let _ = console.println("greedy: no instance limit");
                    exit(1);
                }
            }
            Err(Errno::NoMem) => {
                // A failed spawn leaves the capabilities with the caller.
                let _ = cap_drop(child_rx);
                break;
            }
            Err(e) => fail(console, "spawn", e),
        }
    }
    let _ = Line::new()
        .str("greedy: spawn refused (ENOMEM) after ")
        .uint(spawned as u64)
        .str(" instances")
        .println(console);
    if spawned != EXPECTED {
        let _ = Line::new()
            .str("greedy: expected the limit after ")
            .uint(EXPECTED as u64)
            .println(console);
        exit(1);
    }

    // Phase 2: closing the channel releases every child; their slots come
    // back and one more spawn succeeds.
    let _ = cap_drop(tx.cap());
    let _ = cap_drop(rx.cap());
    let (tx2, rx2) = Channel::create().unwrap_or_else(|e| fail(console, "channel_create", e));
    let mut tries: u32 = 0;
    let last: Instance = loop {
        match spawn(spawner, MOD_PARK, &[rx2.cap()], Class::Fair, 0, 0) {
            Ok(child) => break child,
            Err(Errno::NoMem) if tries < RECLAIM_TRIES => {
                tries += 1;
                yield_now();
            }
            Err(e) => fail(console, "spawn (reclaim)", e),
        }
    };
    let _ = cap_drop(tx2.cap());
    match last.wait() {
        Ok(0) => {
            let _ = console.println("greedy: slot reclaimed, park exited 0");
        }
        Ok(code) => {
            let _ = Line::new()
                .str("greedy: park exited ")
                .int(code as i64)
                .println(console);
            exit(1);
        }
        Err(e) => fail(console, "instance_wait", e),
    }
    exit(0);
}

fn fail(console: Console, what: &str, e: Errno) -> ! {
    let _ = Line::new()
        .str("greedy: ")
        .str(what)
        .str(" failed: ")
        .errno(e)
        .println(console);
    exit(1)
}
