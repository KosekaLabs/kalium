# Running the prototype

The M1 prototype boots on QEMU `virt` (aarch64, cortex-a72, GICv2), runs the
boot-test scenario from [prototype-plan.md](prototype-plan.md) with the
example services from `sdk/examples` (the six of the plan plus the hostile
`greedy`/`park` pair, see [Testing hostile services](#testing-hostile-services)),
and exits QEMU through semihosting with the result as the exit code.

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
services through their console capability. The `rt: met` count varies a
little from run to run; `cargo xtask test` requires at least 380 of 400.
Instance ids are slot numbers and are reused once an instance has exited
(like process ids), so `fair`, `faulty` and `greedy` all show up as
instance 1 after `ping` is gone.

```
kalium: boot
kalium: timer frequency 62500000 Hz
kalium: entered at EL1, dtb at 0x0, heap 0x40200000..0x48000000 (126 MiB)
kalium: boot image: 8 modules
kalium:   module 0: init (4258 bytes)
kalium:   module 1: ping (1945 bytes)
kalium:   module 2: pong (2668 bytes)
kalium:   module 3: rt (1059 bytes)
kalium:   module 4: fair (310 bytes)
kalium:   module 5: faulty (157 bytes)
kalium:   module 6: park (215 bytes)
kalium:   module 7: greedy (3782 bytes)
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
kalium: instance 0 (init) spawned instance 1 (fair, fair)
kalium: instance 0 (init) spawned instance 2 (fair, fair)
kalium: instance 0 (init) spawned instance 3 (rt, rt period 2000 us budget 500 us)
rt: met 400/400
kalium: instance 3 (rt) rt periods 400 met 346 missed 53 budget-exhausted 1
kalium: instance 3 (rt) exited 0
init: rt exited 0
kalium: instance 1 (fair) exited 0
init: fair exited 0
kalium: instance 2 (fair) exited 0
init: fair exited 0
kalium: instance 0 (init) spawned instance 1 (faulty, fair)
kalium: instance 1 (faulty) trapped: out of bounds memory access
kalium: instance 1 (faulty) exited -1
init: faulty exited -1
kalium: instance 0 (init) spawned instance 1 (greedy, fair)
kalium: instance 1 (greedy) spawned instance 2 (park, fair)
kalium: instance 1 (greedy) spawned instance 3 (park, fair)
...
kalium: instance 1 (greedy) spawned instance 31 (park, fair)
greedy: spawn refused (ENOMEM) after 30 instances
kalium: instance 2 (park) exited 0
kalium: instance 3 (park) exited 0
...
kalium: instance 31 (park) exited 0
kalium: instance 1 (greedy) spawned instance 2 (park, fair)
kalium: instance 2 (park) exited 0
greedy: slot reclaimed, park exited 0
kalium: instance 1 (greedy) exited 0
init: greedy exited 0
init: done
kalium: instance 0 (init) exited 0
kalium: interpreter ran 14091910 instructions in 1111 ms (12674879 instr/s)
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

The last phase hands `init`'s `Spawn` capability to `greedy`, a deliberately
hostile service; see [Testing hostile services](#testing-hostile-services).

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
   module index `spawn` takes; the existing indices (`init=0 ... faulty=5`,
   `park=6`, `greedy=7`) are hardcoded in `init` and `greedy`, so append
   rather than insert.
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
with a queue depth of 16. At most 32 instances are live at once
(`kalium_abi::limits::MAX_INSTANCES`, kernel-wide and counting `init`):
`spawn` fails with `ENOMEM` beyond that, and the slot of an exited instance
is reused by the next one created.

## Testing hostile services

A service that holds a `Spawn` capability can try to exhaust the kernel: the
`Instance` capability `spawn` returns can be dropped at once, so the 256-slot
capability table does not bound a `spawn` loop. The kernel bounds it itself
with `MAX_INSTANCES` and reuses the slots of exited instances, so the loop
costs a bounded amount of kernel heap however long it runs. The boot-test
scenario checks this on every `cargo xtask test` with two services:

- `greedy` (module 7) gets a console and `init`'s `Spawn` capability. It
  spawns `park` instances in a loop, dropping each `Instance` capability,
  until `spawn` returns `ENOMEM`; it prints the count, which must be exactly
  `MAX_INSTANCES - 2` (everything but `init` and itself), then closes the
  channel its children block on, spawns once more to show that a slot came
  back, waits for that child and exits 0.
- `park` (module 6) blocks in `recv` on the receive end it was given and
  exits 0 when the channel closes, so the children stay live exactly as long
  as `greedy` wants and nothing in the test depends on timing.

A kernel without the limit fails the test with `greedy: no instance limit`
(and `init: greedy exited 1`); one that leaks slots fails at
`greedy: slot reclaimed, park exited 0`.

To run an unbounded variant by hand, build an `init` that loops
`spawn(spawner, 5, &[], Class::Fair, 0, 0)` followed by `cap_drop` of the
returned capability (module 5, `faulty`, exits at once), put it with the
other seven modules in a directory and point the kernel's `build.rs` at it:

```
cp target/wasm/*.wasm /tmp/bootdir/ && cp my-init.wasm /tmp/bootdir/init.wasm
KALIUM_WASM_DIR=/tmp/bootdir cargo build -p kalium-kernel --target aarch64-unknown-none --release
```

then boot it with the QEMU command above. Before the limit the kernel's
instance table grew by one record per `spawn` until a reallocation of it
failed (`kalium: panic at ...: memory allocation of 130023424 bytes failed`,
QEMU exit code 1, after about 131 000 spawns). With the limit the loop runs
for as long as the hostile `init` likes: `spawn` either succeeds into a
reused slot or returns `ENOMEM`, and the kernel heap stays flat. Run
`cargo xtask build` afterwards to restore the normal kernel binary.
