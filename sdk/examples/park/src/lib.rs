//! `park`: blocks on its receive capability until a message arrives or the
//! channel closes, then exits 0. It is the victim of `greedy`'s spawn flood:
//! it stays alive, and so holds an instance slot, exactly as long as its
//! spawner keeps the channel's sending end.
//!
//! Initial capabilities: slot 0 a channel `Receiver`.
#![no_std]

use kalium_sdk::{Cap, Errno, Receiver, exit};

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let rx = Receiver::from_cap(Cap::slot(0));
    match rx.recv(&mut [], &mut []) {
        // A message, or the last sender gone: the spawner is done with us.
        Ok(_) | Err(Errno::Closed) => exit(0),
        Err(_) => exit(2),
    }
}
