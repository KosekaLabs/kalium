# Running the prototype

The M1 prototype boots on QEMU `virt` (aarch64, cortex-a72, GICv2), runs the
boot-test scenario from [prototype-plan.md](prototype-plan.md) with the six
example services from `sdk/examples`, and exits QEMU through semihosting with
the result as the exit code.

## Prerequisites

- The pinned stable toolchain from `rust-toolchain.toml` with the
  `aarch64-unknown-none` and `wasm32-unknown-unknown` targets (`rustup show`
  installs them).
- `qemu-system-aarch64` (QEMU 8.x; Debian/Ubuntu package `qemu-system-arm`).

## Commands

| Command | What it does |
| --- | --- |
| `cargo xtask wasm` | Builds the SDK examples for `wasm32-unknown-unknown` and copies them to `target/wasm/<name>.wasm` in boot image order. |
| `cargo xtask build` | `wasm`, then the kernel for `aarch64-unknown-none` (release) to `target/aarch64-unknown-none/release/kalium`. |
| `cargo xtask run` | `build`, then boots the kernel in QEMU on the terminal. QEMU exits when `init` exits; `Ctrl-A X` quits early. |
| `cargo xtask test` | Host tests (`cargo test` and the SDK's), `build`, boots QEMU with the console captured, checks the scenario lines and the exit code. |
| `cargo xtask boot-test` | HAL-only check: the kernel built with the `boot-only` feature takes ten timer ticks and exits 0. No Wasm is embedded. It overwrites `target/aarch64-unknown-none/release/kalium`; run `cargo xtask build` again for the full kernel. |
| `cargo xtask clippy` | `clippy -D warnings` on the host crates, the kernel and the SDK. |
| `cargo xtask fmt-check` | `cargo fmt --check` on both workspaces. |

`KALIUM_QEMU_TIMEOUT=<seconds>` changes the 120 s limit on QEMU runs.

The QEMU command `xtask` uses (run it by hand after `cargo xtask build`):

```
qemu-system-aarch64 -M virt,gic-version=2 -cpu cortex-a72 -m 256M -nographic \
  -semihosting-config enable=on,target=native -no-reboot \
  -kernel target/aarch64-unknown-none/release/kalium
```

## Expected output

Lines starting with `kalium:` come from the kernel, the others from the
services through their console capability. The instance ids and the
`rt: met` count vary a little from run to run; `cargo xtask test` requires at
least 380 of 400.

```
kalium: boot
kalium: timer frequency 62500000 Hz
kalium: entered at EL1, dtb at 0x0, heap 0x40200000..0x48000000 (126 MiB)
kalium: boot image: 6 modules
kalium:   module 0: init (4094 bytes)
kalium:   module 1: ping (1945 bytes)
kalium:   module 2: pong (2668 bytes)
kalium:   module 3: rt (1059 bytes)
kalium:   module 4: fair (310 bytes)
kalium:   module 5: faulty (157 bytes)
kalium: instance limits: 252 pages, 65536 stack values, call depth 1024, 256 cap slots
kalium: created instance 0 (init)
hello from init
kalium: instance 0 (init) spawned instance 1 (ping, fair)
kalium: instance 0 (init) spawned instance 2 (pong, fair)
kalium: instance 1 (ping) exited 0
pong: got memobj hello
pong: 100 messages
kalium: instance 2 (pong) exited 0
init: ping exited 0
init: pong exited 0
kalium: instance 0 (init) spawned instance 3 (fair, fair)
kalium: instance 0 (init) spawned instance 4 (fair, fair)
kalium: instance 0 (init) spawned instance 5 (rt, rt period 2000 us budget 500 us)
rt: met 399/400
kalium: instance 5 (rt) rt periods 405 met 399 missed 6 budget-exhausted 0
kalium: instance 5 (rt) exited 0
init: rt exited 0
kalium: instance 3 (fair) exited 0
init: fair exited 0
kalium: instance 4 (fair) exited 0
init: fair exited 0
kalium: instance 0 (init) spawned instance 6 (faulty, fair)
kalium: instance 6 (faulty) trapped: out of bounds memory access
kalium: instance 6 (faulty) exited -1
init: faulty exited -1
init: done
kalium: instance 0 (init) exited 0
kalium: interpreter ran 12293331 instructions in 950 ms (12939814 instr/s)
kalium: init exited 0
```

QEMU's exit code is 0 when `init` exited 0 and every other instance had
exited; otherwise 1. A kernel panic prints `kalium: panic at ...` and exits 1;
a deadlock (every instance blocked, nothing for the timer to wake) prints
`kalium: deadlock: ...` and exits 1.

The `kalium: instance 5 (rt) rt periods ...` line is the scheduler's own
accounting: `met` counts periods in which the thread finished (slept) before
the period boundary, `budget-exhausted` periods in which it used its whole
500 µs budget. The first periods after `spawn` are slow because QEMU's TCG
translates the kernel's real-time paths on first use; the `rt` service's own
`met` count is the one the test checks.

## How the kernel runs the scenario

1. `kmain` brings up the HAL (UART, exception vectors, MMU, GICv2, generic
   timer), initializes the heap and decodes module 0 (`init`).
2. `init` is created with slot 0 `Console(WRITE|DERIVE|TRANSFER)` and slot 1
   `Spawn(SPAWN|TRANSFER)` and scheduled in the fair class.
3. The run loop asks the scheduler for the next thread and its slice, runs
   that instance's execution for a matching fuel budget in ~100 µs chunks,
   and reports the outcome. The timer interrupt only sets a flag; the loop
   checks it between chunks. Host calls that cannot complete park the thread
   and return `Block`; the call is re-issued when the thread is woken.
4. When `init` exits the kernel prints `kalium: init exited <code>` and
   exits QEMU.

## Writing and adding a service

A service is a `cdylib` crate for `wasm32-unknown-unknown` that depends on
`kalium-sdk` and exports `_start`. Its initial capabilities are slots `0..n`
in the order the spawner passed them. See [`sdk/README.md`](../sdk/README.md)
for the SDK API and the conventions that keep binaries small.

```rust
#![no_std]

use kalium_sdk::{exit, Cap, Console};

#[no_mangle]
pub extern "C" fn _start() {
    let console = Console::from_cap(Cap::slot(0));
    console.set_global();
    kalium_sdk::println!("hello from instance {}", kalium_sdk::instance_id());
    exit(0);
}
```

To add it to the boot image:

1. Create `sdk/examples/<name>/` with a `Cargo.toml` like the existing
   examples (`crate-type = ["cdylib"]`, `kalium-sdk.workspace = true`). The
   SDK workspace picks up everything under `examples/*`.
2. Append `<name>` to the module list in `xtask/src/main.rs`
   (`WASM_MODULES`) and in `kernel/build.rs` (`MODULES`). The position is the
   module index `spawn` takes; the existing indices (`init=0 ... faulty=5`)
   are hardcoded in `init`, so append rather than insert.
3. Spawn it from `init` (or any instance holding a `Spawn` capability):
   `spawn(spawner, INDEX, &[caps...], Class::Fair, 0, 0)`. Pass a console
   capability if it should print; derive one with
   `console.cap().derive(rights::WRITE | rights::TRANSFER)` so `init` keeps
   its own. An instance only gets the `kalium:spawn` imports if it holds a
   `Spawn` capability when it is created.
4. `cargo xtask run`.

Services are limited to 256 pages (16 MiB) of linear memory, 65536 value
stack entries, a call depth of 1024 and 256 capability slots. Memory objects
are at most 16 MiB; channel messages at most 4096 bytes and 4 capabilities,
with a queue depth of 16.
