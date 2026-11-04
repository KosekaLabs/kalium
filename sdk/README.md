# Kalium SDK

This directory is a separate Cargo workspace that targets
`wasm32-unknown-unknown`. It holds `kalium-sdk`, the safe Rust wrapper over
the kernel's host ABI, and the example services in `examples/` that make up
the M1 boot-test scenario (`init`, `ping`, `pong`, `rt`, `fair`, `faulty`)
plus the hostile pair that checks the kernel's instance limit (`greedy`,
`park`).

The ABI itself (import names, errno values, rights, kinds, wire layouts) is
defined once, in `crates/kalium-abi`, and both the kernel and this SDK are
written against it. The SDK re-exports it as `kalium_sdk::abi`.

## Building

From the repository root:

```
cargo xtask wasm
```

builds every example in release mode (`opt-level = "s"`, LTO, `panic =
"abort"`, stripped) and copies them to `target/wasm/<name>.wasm` in boot
image order (`init=0, ping=1, pong=2, rt=3, fair=4, faulty=5, park=6,
greedy=7`), then builds the WASI demo programs from `demos/` after them
(`hello_std=8, hello_c=9`; they do not use the SDK, see
`docs/running.md`, "The WASI demos"). The kernel's `build.rs` embeds them
all from there. To build the SDK examples by hand:

```
cargo build --release --target wasm32-unknown-unknown --manifest-path sdk/Cargo.toml
```

The SDK's own unit tests run on the host (the raw imports are stubbed there):

```
cargo test --manifest-path sdk/Cargo.toml -p kalium-sdk
```

## Writing a service

A service is a `cdylib` crate with `#![no_std]`, no allocator, and a single
exported entry point `_start`:

```toml
# Cargo.toml
[package]
name = "my-service"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
kalium-sdk = { path = "../../kalium-sdk" }   # or the workspace dependency
```

```rust
// src/lib.rs
#![no_std]

use kalium_sdk::{exit, Cap, Console};

#[no_mangle]
pub extern "C" fn _start() {
    // Initial capabilities are slots 0..n, in the order the spawner
    // passed them to `spawn`. This service expects a console in slot 0.
    let console = Console::from_cap(Cap::slot(0));
    console.set_global();

    kalium_sdk::println!("hello from instance {}", kalium_sdk::instance_id());
    exit(0);
}
```

Add the crate to `members` in `sdk/Cargo.toml` (anything under `examples/`
is picked up automatically) and, for it to be part of the boot image, to the
module list in `xtask` and the kernel's `build.rs`.

### What a service can do

Everything goes through capabilities: `i32` indices into the instance's
capability table, wrapped as `Cap`. A service has no ambient authority; it
can only use the capabilities it was spawned with and the ones it creates
or receives.

| Need | API |
| --- | --- |
| Print | `Console::write`, `Console::println`, `println!` / `cprintln!`, `text::Line` |
| Capabilities | `Cap::slot`, `cap_derive` (weaker rights), `cap_info`, `cap_drop` |
| Time | `time_now()` (ns since boot), `sleep(ns)`, `yield_now()` |
| Exit | `exit(code) -> !` |
| Messaging | `Channel::create() -> (Sender, Receiver)`, `Sender::send(bytes, &[Cap])`, `Receiver::recv` / `try_recv`, `ipc::wait_any` |
| Shared memory | `MemObj::create(size)`, `read`, `write`, `size` |
| Processes | `spawn(spawn_cap, module, &[Cap], Class, period_ns, budget_ns)`, `Instance::wait`, `Instance::kill`, `module_count` |

Every fallible call returns `Result<_, Errno>`; `Errno` names the ABI's
errno values (`BadCap`, `Perm`, `Again`, `Closed`, ...). Nothing in the SDK
panics on what the kernel or a peer sends.

Capability transfer: `Sender::send` and `spawn` *move* the capabilities
they are given (each needs the `TRANSFER` right); the `Cap` values are
stale afterwards. To hand out a console without giving it away, derive a
copy first: `console.cap().derive(rights::WRITE | rights::TRANSFER)`.

`send` never blocks: when the channel's queue (depth 16) is full it fails
with `Errno::Again`; `yield_now()` and retry. `recv` blocks until a message
arrives and reports `Errno::Closed` once the queue is empty and no sender
exists.

The `kalium:spawn` imports (`spawn`, `instance_wait`, `instance_kill`,
`module_count`) only resolve for an instance that holds a `Spawn`
capability when it is created. Since the linker only keeps imports that
are referenced, a service that never calls them does not import them; one
that does and is spawned without a `Spawn` capability fails to instantiate
(`Errno::NoEnt` to its spawner).

### Keeping binaries small

The examples are between 150 bytes and 4 KiB. What keeps them that way:

- `text::Line` and `text::fmt_i64` format strings and integers on the
  stack without `core::fmt`. `println!` is convenient but links the
  formatting machinery (a few KiB).
- No code path that can panic. The SDK's panic handler prints the panic
  message and exits with `101`, but it is only linked when something can
  panic. Avoid `unwrap`, `expect` and slicing with lengths that come from
  outside; use `get`, `unwrap_or_else(|e| fail(e))` and the `(len, ncaps)`
  that `recv` returns (already clamped to your buffers).
- The release profile in `sdk/Cargo.toml`: `opt-level = "s"`, `lto = true`,
  `codegen-units = 1`, `panic = "abort"`, `strip = true`.

### Panics

A service that panics prints `panic: <message> at <file>:<line>:<col>` to
the global console (if one was set with `Console::set_global`) and exits
with code `101`. Prefer returning errors and calling `exit` with a
meaningful code.

## Layout

```
sdk/
  Cargo.toml          workspace and release profile
  kalium-sdk/         the library
    src/raw.rs          extern "C" imports, checked against kalium-abi at compile time
    src/lib.rs          Errno, Cap, time/exit, panic handler
    src/console.rs      Console, Writer, print!/println!/cprint!/cprintln!
    src/text.rs         Line builder and integer formatting without core::fmt
    src/ipc.rs          Channel, Sender, Receiver, wait_any
    src/memobj.rs       MemObj
    src/spawn.rs        spawn, Instance, Class
  examples/           the boot-test services, one crate each
```

## The boot-test scenario

`init` (slot 0 console, slot 1 spawn) prints `hello from init`, creates a
channel and spawns `ping` (write-only console, sender) and `pong` (console,
receiver). `ping` sends `ping 1` ... `ping 100`; the last message carries a
memory object containing `hello`, which `pong` reads back (`pong: got
memobj hello`, `pong: 100 messages`). Then `init` spawns two `fair`
instances that busy-loop for a second and `rt`, a real-time instance
(period 2 ms, budget 500 µs) that runs 400 periods and prints `rt: met
X/400`. Then `faulty` performs an out-of-bounds load and traps. Finally
`init` hands its `Spawn` capability to `greedy`, which spawns `park`
instances and drops their capabilities until the kernel refuses with
`ENOMEM` at its instance limit (`greedy: spawn refused (ENOMEM) after 30
instances`), releases them by closing the channel they block on, and spawns
once more to show the slots are reused (`greedy: slot reclaimed, park exited
0`). Between `faulty` and `greedy`, `init` runs the two WASI demos
(`hello_std`, exit 3, and `hello_c`, exit 5) with a console capability each,
`hello_c` also with the receive end of a channel as its stdin.
`init` waits for each instance, prints `init: <name> exited <code>` for
each, prints `init: done` and exits with 0.
