//! `hog`: a hostile service that takes everything the kernel will give it
//! and keeps it. It creates memory objects of the largest size until
//! `memobj_create` is refused, grows its own linear memory until
//! `memory.grow` is refused, creates channels until `channel_create` is
//! refused and fills every queue it has with the largest message until
//! `send` is refused; then it checks that the kernel still serves an
//! ordinary request (a 4096-byte `send` on a fresh channel, which must
//! succeed or fail cleanly with `ENOMEM`), fills the rest of that queue
//! with messages that carry no payload but four capabilities each (the
//! kernel's copy of such a message is capabilities and bookkeeping, which
//! its budget must charge like payload), takes half of the queue back
//! through `recv`, prints what it got and exits 0 with everything else
//! still allocated, so that the kernel's release of an exited instance's
//! objects, queued capabilities included, is exercised too. Without
//! quotas this is the service that fills the heap and makes the kernel's
//! next small allocation fail (a panic); with them it is stopped at
//! `MAX_MEMOBJ_BYTES_PER_INSTANCE`, the page limit,
//! `MAX_CHANNELS_PER_INSTANCE` and the queue depth, each with `ENOMEM`
//! or `EAGAIN`, and the kernel is unharmed.
//!
//! Before any of that it leaves the kernel a reference cycle: a channel
//! whose own queue holds a send capability on it, with both ends dropped
//! (`hog: cycle: a channel's send end sent over itself, both ends
//! dropped`). Nothing can ever receive that message, so the kernel must
//! collect the channel (it reports `kalium: collected 1 unreachable
//! objects: 1 queued messages, B budget bytes`), give the channel back
//! to this instance's quota and the message's cost back to the budget; a
//! kernel that only counts references keeps it for ever and the budget
//! never settles at zero.
//!
//! Prints `hog: memobjs N, pages P, channels C, messages M`,
//! `hog: send 4096 bytes on a fresh channel: ok` (or `ENOMEM`),
//! `hog: 15 cap-only messages sent, 8 received back` and
//! `hog: quota reached, kernel alive`. Any other refusal than `ENOMEM`
//! (or `EAGAIN` for a full queue) is reported and exits 1.
//!
//! Initial capabilities: slot 0 `Console(WRITE)`.
#![no_std]

use kalium_sdk::abi::limits::{
    CHANNEL_QUEUE_DEPTH, MAX_MEMOBJ_SIZE, MAX_MESSAGE_CAPS, MAX_MESSAGE_LEN,
};
use kalium_sdk::abi::rights;
use kalium_sdk::{cap_drop, exit, Cap, Channel, Console, Errno, Line, MemObj, Sender};

/// Well past any sane quota: the kernel is broken if it still says yes.
const GIVE_UP: u32 = 4096;
/// Pages asked for at once; halved on refusal until a single page is
/// refused, so that a 16 MiB memory is reached in a few calls rather than
/// one per page.
const GROW_STEP: usize = 64;

#[no_mangle]
pub extern "C" fn _start() {
    let console = Console::from_cap(Cap::slot(0));

    // A reference cycle for the kernel to collect: a send end of a channel
    // queued in that same channel, then both ends dropped. The queued
    // capability is the only reference left on the channel, so a kernel
    // that frees objects by reference count alone leaks it (and its
    // charge on the budget, and this instance's channel quota: the loop
    // below would then stop one channel short).
    let _ = console.println("hog: cycle: a channel's send end sent over itself, both ends dropped");
    let (tx, rx) = match Channel::create() {
        Ok(ends) => ends,
        Err(e) => fail(console, "channel_create (cycle)", e),
    };
    let own = match tx.cap().derive(rights::SEND | rights::TRANSFER) {
        Ok(c) => c,
        Err(e) => fail(console, "cap_derive (cycle)", e),
    };
    if let Err(e) = tx.send(&[], &[own]) {
        fail(console, "send (cycle)", e);
    }
    for c in [tx.cap(), rx.cap()] {
        if let Err(e) = cap_drop(c) {
            fail(console, "cap_drop (cycle)", e);
        }
    }

    // Memory objects of the largest size, kept.
    let mut memobjs: u32 = 0;
    loop {
        match MemObj::create(MAX_MEMOBJ_SIZE) {
            Ok(_) => memobjs += 1,
            Err(Errno::NoMem) => break,
            Err(e) => fail(console, "memobj_create", e),
        }
        if memobjs >= GIVE_UP {
            let _ = console.println("hog: no memory object quota");
            exit(1);
        }
    }

    // Linear memory up to the limit.
    let mut step = GROW_STEP;
    while step > 0 {
        if memory_grow(step) == usize::MAX {
            step /= 2;
        }
    }
    let pages = memory_size() as u64;

    // Channels until refused, each queue filled until refused.
    let big = [0x5Au8; MAX_MESSAGE_LEN];
    let mut channels: u32 = 0;
    let mut messages: u32 = 0;
    let mut last = None;
    // The send end of the first channel: the capabilities the cap-only
    // messages below carry are derived from it (it is kept, so they are
    // caps on a channel other than the one they are queued in).
    let mut first: Option<Sender> = None;
    loop {
        let (tx, rx) = match Channel::create() {
            Ok(ends) => ends,
            Err(Errno::NoMem) => break,
            Err(e) => fail(console, "channel_create", e),
        };
        channels += 1;
        if first.is_none() {
            first = Some(tx);
        }
        loop {
            match tx.send(&big, &[]) {
                Ok(()) => messages += 1,
                Err(Errno::Again) | Err(Errno::NoMem) => break,
                Err(e) => fail(console, "send", e),
            }
        }
        last = Some((tx, rx));
        if channels >= GIVE_UP {
            let _ = console.println("hog: no channel quota");
            exit(1);
        }
    }
    let _ = Line::new()
        .str("hog: memobjs ")
        .uint(memobjs as u64)
        .str(", pages ")
        .uint(pages)
        .str(", channels ")
        .uint(channels as u64)
        .str(", messages ")
        .uint(messages as u64)
        .println(console);

    // The kernel must still work: give one channel back, take a fresh one
    // and send an ordinary large message on it.
    if let Some((tx, rx)) = last {
        let _ = cap_drop(tx.cap());
        let _ = cap_drop(rx.cap());
    }
    let (fresh, verdict) = match Channel::create() {
        Ok((tx, rx)) => match tx.send(&big, &[]) {
            Ok(()) => (Some((tx, rx)), "ok"),
            Err(Errno::NoMem) => (Some((tx, rx)), "ENOMEM"),
            Err(e) => fail(console, "send (fresh channel)", e),
        },
        Err(Errno::NoMem) => (None, "ENOMEM"),
        Err(e) => fail(console, "channel_create (fresh)", e),
    };
    let _ = Line::new()
        .str("hog: send 4096 bytes on a fresh channel: ")
        .str(verdict)
        .println(console);

    // Messages with no payload but the most capabilities a message may
    // carry, each derived from a channel end this instance keeps: the
    // kernel's copy of such a message is its capability list and its
    // queue slot, which the budget charges like payload (a queue of them
    // costs nothing otherwise and would eat the kernel's headroom).
    // Fills what is left of the fresh queue, takes half of the queue back
    // with `recv` (the budget's release on delivery), and leaves the rest
    // for the kernel to drain at exit (its release on destruction).
    let mut sent: u32 = 0;
    let mut received: u32 = 0;
    if let (Some((tx, rx)), Some(subject)) = (fresh, first) {
        let mut caps = [Cap::NONE; MAX_MESSAGE_CAPS];
        loop {
            for c in caps.iter_mut() {
                *c = match subject.cap().derive(rights::TRANSFER) {
                    Ok(c) => c,
                    Err(e) => fail(console, "cap_derive (cap-only message)", e),
                };
            }
            match tx.send(&[], &caps) {
                Ok(()) => sent += 1,
                Err(Errno::Again) | Err(Errno::NoMem) => {
                    for c in caps {
                        let _ = cap_drop(c);
                    }
                    break;
                }
                Err(e) => fail(console, "send (cap-only message)", e),
            }
        }
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let mut got = [Cap::NONE; MAX_MESSAGE_CAPS];
        while received < (CHANNEL_QUEUE_DEPTH / 2) as u32 {
            match rx.try_recv(&mut buf, &mut got) {
                Ok(_) => received += 1,
                Err(Errno::Again) => break,
                Err(e) => fail(console, "recv (cap-only message)", e),
            }
        }
    }
    let _ = Line::new()
        .str("hog: ")
        .uint(sent as u64)
        .str(" cap-only messages sent, ")
        .uint(received as u64)
        .str(" received back")
        .println(console);
    let _ = console.println("hog: quota reached, kernel alive");
    exit(0);
}

/// `memory.grow` by `pages`; `usize::MAX` when refused.
#[cfg(target_arch = "wasm32")]
fn memory_grow(pages: usize) -> usize {
    core::arch::wasm32::memory_grow(0, pages)
}

/// `memory.size` in pages.
#[cfg(target_arch = "wasm32")]
fn memory_size() -> usize {
    core::arch::wasm32::memory_size(0)
}

#[cfg(not(target_arch = "wasm32"))]
fn memory_grow(_pages: usize) -> usize {
    usize::MAX
}

#[cfg(not(target_arch = "wasm32"))]
fn memory_size() -> usize {
    0
}

fn fail(console: Console, what: &str, e: Errno) -> ! {
    let _ = Line::new()
        .str("hog: ")
        .str(what)
        .str(" failed: ")
        .errno(e)
        .println(console);
    exit(1)
}
