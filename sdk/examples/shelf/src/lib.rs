//! `shelf`: a service with an expensive initialisation, kept on the
//! runtime shelf as a snapshot and started from it. The model is an
//! interpreter whose start-up (parsing its standard library, building its
//! tables) costs far more than the work it is then asked to do; the OS
//! takes a snapshot of it once, parked and initialised, and every later
//! instance is a restore of that snapshot instead of a fresh start.
//!
//! `_start` (the fresh start) fills a 256 KiB table with a deterministic
//! computation of several million instructions, records a checksum of it,
//! prints `shelf: initialised`, tells `init` it is ready and parks in
//! `recv`. `init` then takes a snapshot of it (`instance_snapshot`, which
//! needs the instance parked) and spawns instances from the snapshot with
//! entry `resume` (`spawn_snapshot`). `resume` runs on the restored
//! memory: it checks that the table is there (its checksum matches the
//! one recorded in the snapshot), prints `shelf: resumed ok` and exits 0;
//! a mismatch exits 1 and a restore that did not bring the state (the
//! `READY` flag) exits 3. The check reads the whole table, which is about
//! a tenth of building it, so that `init`'s comparison of the two start-up
//! times (`init: shelf init N us, restore M us`, `M < N`) is about the
//! snapshot and not about the measurement.
//!
//! Initial capabilities of `_start`: slot 0 `Console(WRITE)`, slot 1 a
//! channel `Sender` (ready), slot 2 a channel `Receiver` (go; a message or
//! the channel closing ends the service with exit 0). Of `resume`: slot 0
//! `Console(WRITE)`; the restored memory holds the slot number of the
//! console `_start` set as the global one, so the spawner passes the
//! console first, as it did for `_start`.
#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

use kalium_sdk::{exit, Cap, Console, Errno, Receiver, Sender};

/// The state: 65536 words, 256 KiB of linear memory.
const WORDS: usize = 65536;
/// Rounds of mixing per word; sets the cost of the initialisation (about
/// 75 instructions per word per round of 6) against the check (about 7
/// per word).
const ROUNDS: u32 = 6;

/// Exit codes: `resume` found the table intact.
pub const EXIT_RESUMED_OK: i32 = 0;
/// `resume` found a checksum mismatch.
pub const EXIT_MISMATCH: i32 = 1;
/// The setup in `_start` failed (console or channel).
pub const EXIT_SETUP: i32 = 2;
/// `resume` ran on memory that was never initialised.
pub const EXIT_NO_STATE: i32 = 3;

/// Plain loads and stores on `wasm32-unknown-unknown` (no atomics in the
/// target), but `static` without `unsafe`.
static TABLE: [AtomicU32; WORDS] = [const { AtomicU32::new(0) }; WORDS];
static CHECKSUM: AtomicU32 = AtomicU32::new(0);
static READY: AtomicU32 = AtomicU32::new(0);

/// The deterministic, deliberately slow value of word `i`.
fn mix(i: u32) -> u32 {
    let mut x = i.wrapping_mul(0x9E37_79B9) ^ 0xA5A5_5A5A;
    for _ in 0..ROUNDS {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x = x.wrapping_mul(0x2545_F491);
    }
    x
}

/// FNV-1a over the table.
fn checksum() -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for w in TABLE.iter() {
        h = (h ^ w.load(Ordering::Relaxed)).wrapping_mul(0x0100_0193);
    }
    h
}

#[no_mangle]
pub extern "C" fn _start() {
    let console = Console::from_cap(Cap::slot(0));
    console.set_global();
    let ready = Sender::from_cap(Cap::slot(1));
    let go = Receiver::from_cap(Cap::slot(2));

    for (i, w) in TABLE.iter().enumerate() {
        w.store(mix(i as u32), Ordering::Relaxed);
    }
    CHECKSUM.store(checksum(), Ordering::Relaxed);
    READY.store(1, Ordering::Relaxed);

    if console.println("shelf: initialised").is_err() {
        exit(EXIT_SETUP);
    }
    if ready.send(b"ready", &[]).is_err() {
        exit(EXIT_SETUP);
    }
    // Parked here is where the snapshot is taken.
    match go.recv(&mut [], &mut []) {
        Ok(_) | Err(Errno::Closed) => exit(EXIT_RESUMED_OK),
        Err(_) => exit(EXIT_SETUP),
    }
}

/// The entry point of an instance restored from the snapshot (`entry 1`
/// of `spawn_snapshot`).
#[no_mangle]
pub extern "C" fn resume() {
    let console = Console::from_cap(Cap::slot(0));
    if READY.load(Ordering::Relaxed) != 1 {
        let _ = console.println("shelf: resumed without state");
        exit(EXIT_NO_STATE);
    }
    if checksum() == CHECKSUM.load(Ordering::Relaxed) {
        let _ = console.println("shelf: resumed ok");
        exit(EXIT_RESUMED_OK);
    }
    let _ = console.println("shelf: checksum mismatch");
    exit(EXIT_MISMATCH);
}
