//! `revoked`: the victim of a capability revocation. Shows that a revoked
//! capability fails with `EBADCAP` on its next use, in whichever table it
//! ended up, and that a revoked receive end closes its channel for the
//! sender.
//!
//! The protocol with `init`, deterministic and without a clock:
//!
//! 1. print one line through the console in slot 0 (derived by `init` from
//!    a handle it keeps), then send `ready` on the channel in slot 2;
//! 2. block on the channel in slot 1 until `init` sends `go`; by then
//!    `init` has revoked the console and the receive end in slot 3;
//! 3. the next `console_write` must fail with `EBADCAP` and so must a
//!    `recv` on slot 3; then exit with [`EXIT_REVOKED`].
//!
//! Any other outcome exits with a different code, which `init` reports and
//! the scenario check rejects. Nothing can be printed after the revocation.
//!
//! Initial capabilities: slot 0 `Console(WRITE)`, slot 1 a channel
//! `Receiver` (go), slot 2 a channel `Sender` (ready), slot 3 a channel
//! `Receiver` whose revocation is under test.
#![no_std]

use kalium_sdk::{Cap, Console, Errno, Receiver, Sender, exit};

/// Both uses of the revoked capabilities failed with `EBADCAP`.
pub const EXIT_REVOKED: i32 = 42;
/// `console_write` after the revocation did not fail with `EBADCAP`.
pub const EXIT_CONSOLE_STILL_WORKS: i32 = 43;
/// `recv` on the revoked receive end did not fail with `EBADCAP`.
pub const EXIT_RECV_STILL_WORKS: i32 = 44;
/// The setup before the revocation failed.
pub const EXIT_SETUP: i32 = 45;

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let console = Console::from_cap(Cap::slot(0));
    let go = Receiver::from_cap(Cap::slot(1));
    let ready = Sender::from_cap(Cap::slot(2));
    let data = Receiver::from_cap(Cap::slot(3));

    if console.println("revoked: hello through a derived console").is_err() {
        exit(EXIT_SETUP);
    }
    if ready.send(b"ready", &[]).is_err() {
        exit(EXIT_SETUP);
    }
    // init revokes the console and the receive end, then sends go.
    if go.recv(&mut [0u8; 4], &mut []).is_err() {
        exit(EXIT_SETUP);
    }
    match console.println("revoked: this line must not appear") {
        Err(Errno::BadCap) => {}
        _ => exit(EXIT_CONSOLE_STILL_WORKS),
    }
    match data.recv(&mut [0u8; 4], &mut []) {
        Err(Errno::BadCap) => {}
        _ => exit(EXIT_RECV_STILL_WORKS),
    }
    exit(EXIT_REVOKED);
}
