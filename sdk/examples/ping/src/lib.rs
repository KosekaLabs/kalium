//! `ping`: sends 100 messages `ping 1` ... `ping 100` on a channel. The
//! 100th carries a memory object holding `hello`.
//!
//! Initial capabilities: slot 0 `Console(WRITE)` (write-only, derived by
//! init), slot 1 the channel's sender.
#![no_std]

use kalium_sdk::{Cap, Console, Errno, Line, MemObj, Sender, exit, yield_now};

const MESSAGES: u32 = 100;
const MEMOBJ_SIZE: u64 = 64;

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let console = Console::from_cap(Cap::slot(0));
    let tx = Sender::from_cap(Cap::slot(1));

    let memobj = MemObj::create(MEMOBJ_SIZE).unwrap_or_else(|e| fail(console, "memobj_create", e));
    memobj.write(0, b"hello").unwrap_or_else(|e| fail(console, "memobj_write", e));

    for n in 1..=MESSAGES {
        let msg = Line::new().str("ping ").uint(n as u64);
        let caps: &[Cap] = if n == MESSAGES { &[memobj.cap()] } else { &[] };
        // `send` never blocks: when the queue (depth 16) is full, yield so
        // the receiver can drain it, then retry.
        loop {
            match tx.send(msg.as_bytes(), caps) {
                Ok(()) => break,
                Err(Errno::Again) => yield_now(),
                Err(e) => fail(console, "send", e),
            }
        }
    }
    exit(0);
}

fn fail(console: Console, what: &str, e: Errno) -> ! {
    let _ = Line::new().str("ping: ").str(what).str(" failed: ").errno(e).println(console);
    exit(1)
}
