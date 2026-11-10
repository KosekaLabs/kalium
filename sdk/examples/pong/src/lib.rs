//! `pong`: receives 100 messages from `ping`, checks each one, reads the
//! memory object carried by the last and prints
//! `pong: got memobj hello` and `pong: 100 messages`.
//!
//! Initial capabilities: slot 0 `Console`, slot 1 the channel's receiver.
#![no_std]

use kalium_sdk::{Cap, Console, Errno, Line, MemObj, Receiver, exit};

const MESSAGES: u32 = 100;

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let console = Console::from_cap(Cap::slot(0));
    let rx = Receiver::from_cap(Cap::slot(1));

    let mut buf = [0u8; 64];
    let mut caps = [Cap::NONE; kalium_sdk::ipc::MAX_MESSAGE_CAPS];
    let mut count: u32 = 0;
    while count < MESSAGES {
        let (len, ncaps) = rx.recv(&mut buf, &mut caps).unwrap_or_else(|e| fail(console, "recv", e));
        count += 1;

        // The payload must be exactly `ping <count>`.
        let expected = Line::new().str("ping ").uint(count as u64);
        if buf.get(..len) != Some(expected.as_bytes()) {
            let _ = Line::new().str("pong: unexpected message ").uint(count as u64).println(console);
            exit(1);
        }

        // A transferred capability is a memory object with `hello` in it.
        for &cap in caps.get(..ncaps).unwrap_or(&[]) {
            let mut hello = [0u8; 5];
            MemObj::from_cap(cap).read(0, &mut hello).unwrap_or_else(|e| fail(console, "memobj_read", e));
            if &hello == b"hello" {
                let _ = console.println("pong: got memobj hello");
            } else {
                let _ = console.println("pong: memobj has wrong contents");
                exit(1);
            }
        }
    }
    let _ = Line::new().str("pong: ").uint(count as u64).str(" messages").println(console);
    exit(0);
}

fn fail(console: Console, what: &str, e: Errno) -> ! {
    let _ = Line::new().str("pong: ").str(what).str(" failed: ").errno(e).println(console);
    exit(1)
}
