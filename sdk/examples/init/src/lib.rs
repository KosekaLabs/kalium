//! `init`: the first instance. Runs the M1 boot-test scenario from
//! `docs/prototype-plan.md` (plus the WASI demos of module 8 and 9) and
//! reports every child's exit code.
//!
//! Initial capabilities: slot 0 `Console(WRITE|DERIVE|TRANSFER)`, slot 1
//! `Spawn(SPAWN|TRANSFER)`.
#![no_std]

use kalium_sdk::{
    cap_drop, exit, rights, spawn, Cap, Channel, Class, Console, Errno, Instance, Line,
};

/// Boot image module indices (the order `cargo xtask wasm` lays out and
/// the kernel's `build.rs` embeds).
const MOD_PING: u32 = 1;
const MOD_PONG: u32 = 2;
const MOD_RT: u32 = 3;
const MOD_FAIR: u32 = 4;
const MOD_FAULTY: u32 = 5;
const MOD_GREEDY: u32 = 7;
/// The WASI demos (`demos/`): a Rust `std` program and a C program.
const MOD_HELLO_STD: u32 = 8;
const MOD_HELLO_C: u32 = 9;

/// `rt` parameters: period 2 ms, budget 500 µs.
const RT_PERIOD_NS: i64 = 2_000_000;
const RT_BUDGET_NS: i64 = 500_000;

#[no_mangle]
pub extern "C" fn _start() {
    let console = Console::from_cap(Cap::slot(0));
    console.set_global();
    let spawner = Cap::slot(1);

    say("hello from init");

    // Phase 1: ping and pong exchange 100 messages over a channel; the last
    // one carries a memory object.
    let (tx, rx) = Channel::create().unwrap_or_else(|e| fail("channel_create", e));
    // ping gets a write-only console (no DERIVE); pong gets the full set.
    let ping_console = console
        .cap()
        .derive(rights::WRITE | rights::TRANSFER)
        .unwrap_or_else(|e| fail("cap_derive (ping console)", e));
    let pong_console = console
        .cap()
        .derive(rights::WRITE | rights::DERIVE | rights::TRANSFER)
        .unwrap_or_else(|e| fail("cap_derive (pong console)", e));
    let ping = spawn_fair(spawner, MOD_PING, &[ping_console, tx.cap()], "ping");
    let pong = spawn_fair(spawner, MOD_PONG, &[pong_console, rx.cap()], "pong");
    report("ping", ping);
    report("pong", pong);

    // Phase 2: one real-time instance under the load of two fair busy
    // loops. The fair ones start first so that rt runs entirely under load.
    let fair_a = spawn_fair(spawner, MOD_FAIR, &[], "fair");
    let fair_b = spawn_fair(spawner, MOD_FAIR, &[], "fair");
    let rt_console = console
        .cap()
        .derive(rights::WRITE | rights::TRANSFER)
        .unwrap_or_else(|e| fail("cap_derive (rt console)", e));
    let rt = spawn(
        spawner,
        MOD_RT,
        &[rt_console],
        Class::RealTime,
        RT_PERIOD_NS,
        RT_BUDGET_NS,
    )
    .unwrap_or_else(|e| fail("spawn rt", e));
    report("rt", rt);
    report("fair", fair_a);
    report("fair", fair_b);

    // Phase 3: a faulting instance; the kernel must report the trap (-1)
    // and carry on.
    let faulty = spawn_fair(spawner, MOD_FAULTY, &[], "faulty");
    report("faulty", faulty);

    // Phase 3b: the WASI demos. Ordinary wasm32-wasip1 programs that know
    // nothing of Kalium; their console capability in slot 0 is their
    // stdout/stderr. hello_std exits 3.
    let demo_console = derive_demo_console(console);
    let hello_std = spawn_fair(spawner, MOD_HELLO_STD, &[demo_console], "hello_std");
    report("hello_std", hello_std);

    // hello_c also reads its stdin: the receive end of a channel in slot 1,
    // each message one chunk of input, filled before the spawn with a line,
    // a word and an empty message (end of file). init keeps the sending end
    // until hello_c has exited, so a read that blocked on the empty channel
    // instead of returning (a zero-length read must not) would deadlock the
    // kernel rather than pass as end of file. hello_c exits 5.
    let (stdin_tx, stdin_rx) =
        Channel::create().unwrap_or_else(|e| fail("channel_create (hello_c stdin)", e));
    for msg in [&b"hello\n"[..], b"world", b""] {
        stdin_tx
            .send(msg, &[])
            .unwrap_or_else(|e| fail("send (hello_c stdin)", e));
    }
    let demo_console = derive_demo_console(console);
    let hello_c = spawn_fair(
        spawner,
        MOD_HELLO_C,
        &[demo_console, stdin_rx.cap()],
        "hello_c",
    );
    report("hello_c", hello_c);
    cap_drop(stdin_tx.cap()).unwrap_or_else(|e| fail("cap_drop (hello_c stdin)", e));

    // Phase 4: a hostile spawner. `greedy` floods the kernel with `park`
    // instances, dropping their capabilities, until the kernel-wide instance
    // limit refuses it, then shows the slots come back. It takes init's
    // Spawn capability with it (init has nothing left to spawn), so this is
    // the last phase.
    let greedy_console = console
        .cap()
        .derive(rights::WRITE | rights::TRANSFER)
        .unwrap_or_else(|e| fail("cap_derive (greedy console)", e));
    let greedy = spawn_fair(spawner, MOD_GREEDY, &[greedy_console, spawner], "greedy");
    report("greedy", greedy);

    say("init: done");
    exit(0);
}

fn console() -> Console {
    // `_start` sets the global console before anything else runs.
    Console::global().unwrap_or(Console::from_cap(Cap::slot(0)))
}

fn say(s: &str) {
    let _ = console().println(s);
}

/// A write-only console for a WASI demo (its stdout and stderr).
fn derive_demo_console(console: Console) -> Cap {
    console
        .cap()
        .derive(rights::WRITE | rights::TRANSFER)
        .unwrap_or_else(|e| fail("cap_derive (demo console)", e))
}

fn spawn_fair(spawner: Cap, module: u32, caps: &[Cap], name: &str) -> Instance {
    spawn(spawner, module, caps, Class::Fair, 0, 0).unwrap_or_else(|e| {
        let _ = Line::new()
            .str("init: spawn ")
            .str(name)
            .str(" failed: ")
            .errno(e)
            .println(console());
        exit(1)
    })
}

/// Waits for `instance` and prints `init: <name> exited <code>`.
fn report(name: &str, instance: Instance) {
    match instance.wait() {
        Ok(code) => {
            let _ = Line::new()
                .str("init: ")
                .str(name)
                .str(" exited ")
                .int(code as i64)
                .println(console());
        }
        Err(e) => {
            let _ = Line::new()
                .str("init: instance_wait ")
                .str(name)
                .str(" failed: ")
                .errno(e)
                .println(console());
            exit(1)
        }
    }
}

fn fail(what: &str, e: Errno) -> ! {
    let _ = Line::new()
        .str("init: ")
        .str(what)
        .str(" failed: ")
        .errno(e)
        .println(console());
    exit(1)
}
