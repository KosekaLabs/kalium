# Running the prototype

The M1 prototype boots on QEMU `virt` (aarch64, cortex-a72, GICv2), runs the
boot-test scenario from [prototype-plan.md](prototype-plan.md) with the
example services from `sdk/examples` (the six of the plan, the hostile
`greedy`/`park` pair and the hoarder `hog`, see
[Testing hostile services](#testing-hostile-services),
`revoked`, see [Revoking capabilities](#revoking-capabilities), and
`shelf`, see [The runtime shelf](#the-runtime-shelf-snapshots))
and the two WASI demo programs from `demos/` (an ordinary Rust `std` program
and an ordinary C program, see [The WASI demos](#the-wasi-demos)), and exits
QEMU through semihosting with the result as the exit code.

## Prerequisites

- The pinned stable toolchain from `rust-toolchain.toml` with the
  `aarch64-unknown-none`, `wasm32-unknown-unknown` and `wasm32-wasip1`
  targets (`rustup show` installs them).
- `qemu-system-aarch64` (QEMU 8.x; Debian/Ubuntu package `qemu-system-arm`).
- Optional: `clang` with `lld` and a wasi-libc sysroot (Debian/Ubuntu
  packages `clang`, `lld`, `wasi-libc`, or a wasi-sdk) to rebuild the C demo;
  without them `cargo xtask demos` uses the checked-in
  `demos/hello-c/prebuilt/hello_c.wasm` and says so.

## Commands

| Command | What it does |
| --- | --- |
| `cargo xtask wasm` | Builds the SDK examples for `wasm32-unknown-unknown` and copies them to `target/wasm/<name>.wasm` in boot image order, then runs `demos`. |
| `cargo xtask demos` | Builds the WASI demos into `target/wasm/`: `hello_std` with `cargo build --release --target wasm32-wasip1` (its own workspace in `demos/`), `hello_c` with clang and wasi-libc when found (`CLANG` and `WASI_SYSROOT` override the search; the fresh build is compared with the prebuilt binary and a difference is reported), else from `demos/hello-c/prebuilt/` with a message saying why. |
| `cargo xtask build` | `wasm`, then the kernel for `aarch64-unknown-none` (release) to `target/aarch64-unknown-none/release/kalium`. `--selftest` adds the `hal-selftest` feature (see [The HAL self-test](#the-hal-self-test)), `--boot-only` the `boot-only` feature. |
| `cargo xtask run` | `build`, then boots the kernel in QEMU on the terminal. QEMU exits when `init` exits; `Ctrl-A X` quits early. Takes the same flags as `build`. |
| `cargo xtask test` | Host tests (`cargo test`, the HAL's device tree reader tests and the SDK's), `selftest`, `build`, boots QEMU with the console captured, checks the scenario lines and the exit code. |
| `cargo xtask boot-test` | HAL-only check: the kernel built with the `boot-only` feature takes ten timer ticks and exits 0. No Wasm is embedded. It overwrites `target/aarch64-unknown-none/release/kalium`; run `cargo xtask build` again for the full kernel. |
| `cargo xtask selftest` | HAL hardening check: the kernel built with `boot-only` and `hal-selftest` runs the self-test against the memory map and the fault path, must print `kalium: selftest PASS`, then passes the boot test; once with the usual `-m 256M` and once with `-m 129M`, a RAM size that is not a multiple of 2 MiB. Overwrites the kernel binary like `boot-test`. |
| `cargo xtask clippy` | `clippy -D warnings` on the host crates, the kernel, the SDK and the demos. |
| `cargo xtask fmt-check` | `cargo fmt --check` on the three workspaces (root, `sdk/`, `demos/`). |

`KALIUM_QEMU_TIMEOUT=<seconds>` changes the 120 s limit on QEMU runs.

The QEMU command `xtask` uses (run it by hand after `cargo xtask build`):

```
qemu-system-aarch64 -M virt,gic-version=2 -cpu cortex-a72 -m 256M -nographic \
  -semihosting-config enable=on,target=native -no-reboot \
  -kernel target/aarch64-unknown-none/release/kalium
```

## Expected output

Lines starting with `kalium:` come from the kernel, the others from the
services through their console capability (the WASI demos through their
stdout, which is the same capability). The `rt: met` count varies a little
from run to run; `cargo xtask test` requires at least 380 of 400. Instance
ids are slot numbers and are reused once an instance has exited (like
process ids), so `fair`, `faulty`, `hello_std`, `hello_c`, `revoked`,
`shelf`, `hog`, `park` and `greedy` all show up as instance 1 after `ping` is gone, and
the three instances restored from `shelf`'s snapshot as instance 2 one
after the other. The times on the `init: shelf init N us, restore M us`
line vary with the host; `cargo xtask test` requires `M < N`. The instruction count on each
exit line is the interpreter's fuel accounting for that instance (a chunk
that ends in a trap is not counted, so `faulty` shows 0).

```
kalium: boot
kalium: timer frequency 62500000 Hz
kalium: entered at EL1, dtb at 0x40000000 (1048576 bytes), RAM 0x40000000..0x50000000 (256 MiB) from the device tree
kalium: image 0x40200000..0x403b6000 (W^X: text RX, rodata RO, data/stacks RW, guards unmapped), heap 0x403b6000..0x50000000 (252 MiB)
kalium: boot image: 13 modules
kalium:   module 0: init (9377 bytes)
kalium:   module 1: ping (1945 bytes)
kalium:   module 2: pong (2668 bytes)
kalium:   module 3: rt (1059 bytes)
kalium:   module 4: fair (310 bytes)
kalium:   module 5: faulty (157 bytes)
kalium:   module 6: park (215 bytes)
kalium:   module 7: greedy (3782 bytes)
kalium:   module 8: revoked (1110 bytes)
kalium:   module 9: shelf (1257 bytes)
kalium:   module 10: hog (3470 bytes)
kalium:   module 11: hello_std (101148 bytes)
kalium:   module 12: hello_c (202342 bytes)
kalium: instance limits: 256 pages, 65536 stack values, call depth 1024, 256 cap slots
kalium: instance quotas: 32 MiB memory objects, 32 MiB snapshots, 64 channels; budget 244 MiB for all instances (8 MiB kernel headroom)
kalium: created instance 0 (init)
hello from init
kalium: instance 0 (init) spawned instance 1 (ping, fair)
kalium: instance 0 (init) spawned instance 2 (pong, fair)
kalium: instance 1 (ping) exited 0 after 20850 instructions
pong: got memobj hello
init: ping exited 0
pong: 100 messages
kalium: instance 2 (pong) exited 0 after 41978 instructions
init: pong exited 0
kalium: instance 0 (init) spawned instance 1 (fair, fair)
kalium: instance 0 (init) spawned instance 2 (fair, fair)
kalium: instance 0 (init) spawned instance 3 (rt, rt period 2000 us budget 500 us)
rt: met 400/400
kalium: instance 3 (rt) rt periods 400 met 399 missed 1 budget-exhausted 0
kalium: instance 3 (rt) exited 0 after 702576 instructions
init: rt exited 0
kalium: instance 1 (fair) exited 0 after 10881108 instructions
kalium: instance 2 (fair) exited 0 after 6024740 instructions
init: fair exited 0
init: fair exited 0
kalium: instance 0 (init) spawned instance 1 (faulty, fair)
kalium: instance 1 (faulty) trapped: out of bounds memory access
kalium: instance 1 (faulty) exited -1 after 0 instructions
init: faulty exited -1
kalium: instance 0 (init) spawned instance 1 (hello_std, fair)
hello_std: hello from Rust std on Kalium
hello_std: vec sum 55, len 10, squares [1, 4, 9, 16, 25]
hello_std: fmt   Kalium|Kalium  |0042|0xff|+7
hello_std: map [("one", 1), ("three", 3), ("two", 2)] total 6
hello_std: sqrt(2) = 1.414214, leibniz pi ~ 3.1416, 1e3 ln = 6.908
hello_std: args [], env vars 0, HOME None
hello_std: slept 10 ms (clock advanced)
hello_std: exiting with 3
kalium: instance 1 (hello_std) exited 3 after 1291847 instructions
init: hello_std exited 3
kalium: instance 0 (init) spawned instance 1 (hello_c, fair)
hello_c: hello from C on Kalium
hello_c: printf 42|   42|42   |ff|10|K|str| 3.14|1.234500e+03|0.0001|%
hello_c: argc=0 argv0=(none)
hello_c: malloc/free ok (3 blocks)
hello_c: qsort [1, 2, 3, 5, 8, 13, 21]
hello_c: strtod 3.14159 rest " tail", strtol 42 -17 255
hello_c: snprintf "x=7 y=3.50 z=end" (16), "truncat" (9)
hello_c: fact(10)=3628800 fib(20)=6765
hello_c: time() ok (1 s)
hello_c: stdin fgets "hello"
hello_c: stdin readv (empty, 15) got 5 "world"
hello_c: stdin read got 0 (end of file)
hello_c: stdin read of 0 bytes got 0 at once
hello_c: exiting with 5
kalium: instance 1 (hello_c) exited 5 after 413033 instructions
init: hello_c exited 5
kalium: instance 0 (init) spawned instance 1 (revoked, fair)
revoked: hello through a derived console
kalium: instance 0 (init) revoked slot 16: 1 derived capability invalidated
kalium: instance 0 (init) revoked slot 15: 1 derived capability invalidated
init: send to revoked receiver: ECLOSED
init: handle derives again after revoke
kalium: instance 1 (revoked) exited 42 after 657 instructions
init: revoked exited 42
kalium: instance 0 (init) spawned instance 1 (shelf, fair)
shelf: initialised
kalium: instance 0 (init) snapshotted instance 1 (shelf): 21 pages, 1376294 bytes
kalium: instance 0 (init) spawned instance 2 (shelf, fair) from snapshot
shelf: resumed ok
kalium: instance 2 (shelf) exited 0 after 1048831 instructions
init: resumed shelf exited 0
kalium: instance 0 (init) spawned instance 2 (shelf, fair) from snapshot
shelf: resumed ok
kalium: instance 2 (shelf) exited 0 after 1048831 instructions
init: resumed shelf exited 0
kalium: instance 0 (init) spawned instance 2 (shelf, fair) from snapshot
shelf: resumed ok
kalium: instance 2 (shelf) exited 0 after 1048831 instructions
init: resumed shelf exited 0
kalium: instance 1 (shelf) exited 0 after 11665700 instructions
init: shelf exited 0
init: shelf init 284494 us, restore 27429 us
kalium: instance 0 (init) spawned instance 1 (hog, fair)
hog: memobjs 2, pages 256, channels 64, messages 1024
hog: send 4096 bytes on a fresh channel: ok
hog: 15 cap-only messages sent, 8 received back
hog: quota reached, kernel alive
kalium: instance 1 (hog) exited 0 after 32084 instructions
kalium: instance 1 (hog) exit drained 1016 queued messages, 4179200 budget bytes
init: hog exited 0
kalium: instance 0 (init) spawned instance 1 (park, fair)
kalium: instance 1 (park) exited 0 after 32 instructions
init: after hog, spawn ok
kalium: instance 0 (init) spawned instance 1 (greedy, fair)
kalium: instance 1 (greedy) spawned instance 2 (park, fair)
kalium: instance 1 (greedy) spawned instance 3 (park, fair)
...
kalium: instance 1 (greedy) spawned instance 31 (park, fair)
greedy: spawn refused (ENOMEM) after 30 instances
kalium: instance 2 (park) exited 0 after 53 instructions
kalium: instance 3 (park) exited 0 after 53 instructions
...
kalium: instance 31 (park) exited 0 after 53 instructions
kalium: instance 1 (greedy) spawned instance 2 (park, fair)
kalium: instance 2 (park) exited 0 after 53 instructions
greedy: slot reclaimed, park exited 0
kalium: instance 1 (greedy) exited 0 after 3768 instructions
init: greedy exited 0
init: done
kalium: instance 0 (init) exited 0 after 10054 instructions
kalium: interpreter ran 44173705 instructions in 1898 ms (23269797 instr/s)
kalium: instance budget: 0 bytes in use
kalium: init exited 0
```

QEMU's exit code is 0 when `init` exited 0 and every other instance had
exited; otherwise 1. A kernel panic prints `kalium: panic at ...` and exits 1;
a deadlock (every instance blocked, nothing for the timer to wake) prints
`kalium: deadlock: ...` and exits 1. A fault in kernel code itself (not a
Wasm trap: a data or instruction abort in the interpreter or the HAL) prints
`kalium: unhandled Synchronous exception (...)`, one line
`kalium: kernel fault: <data|instruction> abort (<permission|translation|...>
fault, level N[, read|write]) at FAR_EL1 = ..., ELR_EL1 = ...`, the stack
pointers and the register file, and exits with code **2**; `xtask` names the
meaning of a non-zero code in its failure message.

The RAM size comes from the device tree QEMU installs at the start of RAM:
`-m 512M` gives `RAM 0x40000000..0x60000000 (512 MiB)` and a 508 MiB heap.
Without a device tree the kernel says so and assumes the linker script's
128 MiB.

The `kalium: instance 5 (rt) rt periods ...` line is the scheduler's own
accounting: `met` counts periods in which the thread finished (slept) before
the period boundary, `budget-exhausted` periods in which it used its whole
500 µs budget. The first periods after `spawn` are slow because QEMU's TCG
translates the kernel's real-time paths on first use; the `rt` service's own
`met` count is the one the test checks.

## Kernel memory map

The kernel is identity mapped with 4 KiB pages over its own image and 2 MiB
blocks elsewhere, write-xor-execute throughout (`SCTLR_EL1.WXN` is set as
well):

| Range | Mapping |
| --- | --- |
| `0x0000_0000..0x4000_0000` | MMIO: device memory, read-write, never executable |
| `0x4000_0000..0x4020_0000` | the device tree QEMU installs at the start of RAM: read-only, never executable |
| `.text` (from `0x4020_0000`) | read-only, executable |
| `.rodata` (incl. the boot image) | read-only, never executable |
| `.data`, `.bss` | read-write, never executable |
| 16 KiB guard | unmapped: a boot stack overflow faults here |
| 1 MiB boot stack (`SP_EL0`) | read-write, never executable |
| 16 KiB guard | unmapped: an exception stack overflow faults here |
| 64 KiB exception stack (`SP_EL1`) | read-write, never executable |
| heap, up to the end of RAM | read-write, never executable, 2 MiB blocks; the last block, if RAM does not end on a 2 MiB boundary, with 4 KiB pages up to the end of RAM |
| above RAM | unmapped: a stray access is a translation fault (the mapping ends exactly at the end of RAM, rounded down to 4 KiB, so nothing without backing memory is mapped) |

The kernel runs on `SP_EL0`; every exception switches to `SP_EL1`, so a
kernel stack overflow is reported from an intact stack instead of
double-faulting. The vectors save and restore the FP/SIMD registers
(`q0`–`q31`, `FPCR`, `FPSR`) with the general-purpose ones, so an interrupt
cannot clobber the interpreter's floating-point state; the round trip costs
about 70 counter ticks (1.1 µs) under QEMU, about 10 ticks more than without
the FP save.

The image is loaded at `0x4020_0000`, 2 MiB above the start of RAM, because
QEMU `-kernel` with an ELF image installs the device tree at the start of RAM
only if its 1 MiB buffer fits below the lowest loaded segment (and it does
not pass the address in `x0`, which stays 0; the HAL checks `x0` first and
then the start of RAM).

## The HAL self-test

`cargo xtask selftest` boots a `boot-only` kernel with the `hal-selftest`
feature. Before the tick loop it checks the mapping of every region with the
`AT S1E1R/W` instructions, then provokes each kind of fault on purpose and
verifies the exception handler's decoding: a write to `.rodata` and to
`.text`, a jump into `.data`, into the stack and into the heap, a read at
the end of RAM (the first unmapped byte: the mapping ends exactly where RAM
does), and a kernel stack overflow by recursion. The handler reports what it
caught (`kalium: selftest: handler caught ...`) and returns to the test
harness instead of exiting. Finally it times the exception round trip. The
whole run happens twice, with `-m 256M` and with `-m 129M`: with 129 MiB
the last 2 MiB block of RAM is only half there, so its mapping with 4 KiB
pages and the translation fault at `0x48100000` are checked as well.
Abbreviated output:

```
kalium: selftest: at: .rodata: ok (0x40207000: read ok, write refused, normal write-back)
kalium: selftest: at: stack guard: ok (0x40215000: read refused (translation fault), write refused, unmapped)
kalium: selftest: mapping ends at RAM end: ok (RAM ends at 0x50000000, mapped to 0x50000000, heap to 0x50000000)
kalium: selftest: at: past RAM: ok (0x50000000: read refused (translation fault), write refused, unmapped)
kalium: selftest: at: uart: ok (0x9000000: read ok, write ok, device-nGnRnE)
kalium: selftest: handler caught data abort (permission fault, level 3, write) at FAR_EL1 = 0x40207bf0, ELR_EL1 = 0x40205158
kalium: selftest: write .rodata: ok (data abort (permission fault, level 3, write) at FAR_EL1 = 0x40207bf0, ELR_EL1 = 0x40205158)
kalium: selftest: handler caught instruction abort (permission fault, level 3) at FAR_EL1 = 0x40209000, ELR_EL1 = 0x40209000
kalium: selftest: exec .data: ok (instruction abort (permission fault, level 3) at FAR_EL1 = 0x40209000, ELR_EL1 = 0x40209000)
kalium: selftest: handler caught data abort (translation fault, level 3, write) at FAR_EL1 = 0x40218f48, ELR_EL1 = 0x4020620c
kalium: selftest: stack overflow: ok (data abort (translation fault, level 3, write) at FAR_EL1 = 0x40218f48, ELR_EL1 = 0x4020620c, SP_EL0 = 0x40218f40 in the guard, reported from the exception stack)
kalium: selftest: stack restored: ok (SP = 0x40318dd0)
kalium: selftest: exception round trip: ok (71.94 ticks (1151 ns) per svc, averaged over 20000 (62500000 Hz counter))
kalium: selftest PASS: 22 passed, 0 failed
kalium: 10 ticks
```

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

After `faulty`, `init` runs the two WASI demos (`hello_std`, `hello_c`), each
with a console capability as its stdout, `hello_c` also with a channel as
its stdin; see [The WASI demos](#the-wasi-demos). Then it spawns `revoked`
and takes its capabilities back; see
[Revoking capabilities](#revoking-capabilities). Then it shelves `shelf`:
a snapshot of the initialised service, and three instances started from
it; see [The runtime shelf](#the-runtime-shelf-snapshots). Then it spawns
`hog`, which hoards until every quota refuses it, and checks that the
kernel is unharmed afterwards; see
[Resource quotas](#resource-quotas-hog). The last phase hands `init`'s
`Spawn` capability to `greedy`, a deliberately hostile service; see
[Testing hostile services](#testing-hostile-services).

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
2. Add `<name>` to the module lists in `xtask/src/main.rs` and
   `kernel/build.rs` (`MODULES`), which must agree: the SDK modules
   (`WASM_MODULES` in `xtask`: `init=0 ... faulty=5`, `park=6`, `greedy=7`,
   `revoked=8`, `shelf=9`, `hog=10`) come first, then the WASI demos (`DEMO_MODULES`:
   `hello_std=11`, `hello_c=12`). The position is the module index `spawn` takes, and the
   existing indices are hardcoded in `init` and `greedy`, so append a new
   SDK module to the end of `WASM_MODULES` and insert it before `hello_std`
   in `build.rs`, then bump `MOD_HELLO_STD`/`MOD_HELLO_C` in `init`; a
   non-SDK program (another WASI binary) goes after `hello_c` in both lists
   instead, with its own build step in `demos()`.
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
is reused by the next one created. What a service creates is charged to
it until it is freed or the service exits: at most 32 MiB of memory
objects, 32 MiB of snapshots and 64 channels per instance, and all
instances together at most the heap minus an 8 MiB kernel headroom
(`ENOMEM` beyond any of them); see [Resource quotas](#resource-quotas-hog).

## The WASI demos

The kernel also resolves the `wasi_snapshot_preview1` import module for
every instance ([prototype-plan.md](prototype-plan.md), "WASI preview1
compatibility"; the implementation and its design notes are in
`kernel/src/wasi.rs`), so programs built for `wasm32-wasip1` run without
knowing about Kalium: their stdout and stderr are the `Console` capability
in slot 0, their stdin the `Channel` capability in slot 1 (if any), there
are no files, no preopened directories and no sockets, `proc_exit` is the
instance's exit, `clock_time_get` is the HAL's counter since boot (both
clocks), `random_get` is a non-cryptographic xorshift, and `poll_oneoff`
sleeps on clock subscriptions. `demos/` holds two such programs, written as
anyone would write them:

- `demos/hello-std` (module 11, `hello_std`): a Rust `std` program with
  `println!`, a `Vec`, `format!`, a `HashMap` (its hasher is seeded through
  `random_get`), floating point, `std::env::args`/`vars` (both empty),
  `thread::sleep` (through `poll_oneoff`) and `process::exit(3)`. Built by
  `cargo xtask demos` with
  `cargo build --release --target wasm32-wasip1 --manifest-path demos/Cargo.toml`
  (profile: `opt-level = "s"`, LTO, `panic = "abort"`, stripped); about
  100 KB, about 1.3 M interpreted instructions.
- `demos/hello-c` (module 12, `hello_c`): a C program with `printf` in
  several conversions, `malloc`/`free`, `qsort`, `strtod`/`strtol`,
  `snprintf`, recursion, `time()`, stdin through `fgets`, `readv` and
  `read`, and `return 5`. Compiled with clang and
  wasi-libc; `demos/hello-c/prebuilt/README.md` has the exact command and
  the toolchain versions, and the prebuilt binary next to it is what
  `cargo xtask demos` uses when clang or the sysroot are missing (it says
  so; with the toolchain present it rebuilds and reports whether the result
  matches the prebuilt one). About 200 KB (mostly `printf`'s floating-point
  support), about 0.4 M interpreted instructions.

`init` spawns each with a console capability derived from its own
(`WRITE|TRANSFER`) in slot 0 and waits; `hello_c` also gets the receive end
of a channel in slot 1, its stdin, which `init` fills before the spawn with
three messages, `hello\n`, `world` and an empty one (end of file), keeping
the sending end until `hello_c` has exited. `hello_c` reads them with
`fgets`, with a `readv` whose empty first buffer must be skipped (not
reported as end of file), with a `read` that hits the empty message, and
ends with a zero-length `read` on the drained stdin, which must return 0 at
once rather than wait for input: since `init` holds the sender and is
waiting for `hello_c`, a kernel that blocked there would report a deadlock
and fail the test. `cargo xtask test` asserts their output lines,
`init: hello_std exited 3` and `init: hello_c exited 5`. A
WASI program that needs something the layer does not provide gets a WASI
errno (`EBADF` for any fd but 0..2, `ENOTSUP` for files and sockets,
`ENOSYS` for `proc_raise`), never a link failure, so wasi-libc's and Rust's
startup code, which probe for preopened directories, run unchanged.

To run another WASI program: drop its `.wasm` into the boot image (see
[Writing and adding a service](#writing-and-adding-a-service); it needs no
SDK) and spawn it with a console capability in slot 0 and, if it reads
stdin, the receive end of a channel in slot 1; whatever the spawner sends
on the channel is what the program reads, a message at a time.

## Revoking capabilities

A capability can be taken back from whoever holds it. Every capability
carries its *lineage*, the node it occupies in the derivation tree of its
object: capabilities the kernel mints fresh (`channel_create`,
`memobj_create`, `spawn`, `init`'s boot capabilities) are roots,
`cap_derive` adds a child under the source, a transfer keeps the node.
`cap_revoke(cap)` (SDK: `cap_revoke`, `Cap::revoke`) invalidates every
capability below `cap`'s node, and only those, wherever they are: in the
caller's table, in another instance's, or inside a message still queued on
a channel. `cap` itself stays valid and may derive again; its siblings and
what it was derived from are untouched; and `cap_drop` is not revocation
(dropping a capability leaves its derivations valid).

What the holder of a revoked capability sees: the next host call that
takes it fails with `EBADCAP`, like a slot that was never filled, whatever
the call and whatever rights the capability had; a `recv`, `wait_any` or
`instance_wait` already blocked on it is woken and fails the same way. The
dead capability still occupies its slot (the kernel does not rearrange
another instance's table) until `cap_drop`, which succeeds on it, or the
instance's exit; nothing is leaked either way, since the object reference
and the lineage node go with the slot. The kernel reports each revocation
(`kalium: instance 0 (init) revoked slot 16: 1 derived capability
invalidated`); the count includes capabilities in flight. The check costs
a walk up the chain of at most 16 nodes (`MAX_DERIVE_DEPTH`; `cap_derive`
fails with `ENOMEM` beyond it), never a scan of the tables; `cap_revoke`
itself scans every table and queue once. Each revocation bumps the node's
32-bit generation; `cap_revoke` fails with `ENOMEM`, invalidating nothing,
once that would wrap.

Channel ends are the one place revocation acts immediately rather than on
the next call: a channel capability counts as a sender or receiver only
while it is valid, so revoking the receive end you gave away closes the
channel for its senders at once (`send` returns `ECLOSED` once no valid
receiver is left; a revoked receiver cannot keep a channel open forever),
and revoking a send end closes it for receivers once the queue drains.
Dropping the dead capability later does not change the counts again.

The idiom is a handle you keep: derive one with `DERIVE` from the
capability you own, transfer a derivation of the handle, and revoke the
handle when the time comes. The boot-test scenario does exactly that with
`revoked` (module 8), spawned after the WASI demos:

- `init` keeps `handle = console.derive(WRITE|DERIVE|TRANSFER)` and gives
  `revoked` `handle.derive(WRITE|TRANSFER)` (slot 0), the receive end of a
  `go` channel (slot 1), the send end of a `ready` channel (slot 2) and a
  derivation `data_rx.derive(RECV|WAIT|TRANSFER)` of a data channel's
  receive end (slot 3).
- `revoked` prints `revoked: hello through a derived console`, sends
  `ready` and blocks on `go`.
- `init` receives `ready`, revokes `handle` (the console in `revoked`'s
  slot 0 is dead; `init`'s console and `handle` are not: it derives from
  `handle` once more and prints `init: handle derives again after revoke`)
  and revokes `data_rx` (the receiver in slot 3 is dead), then drops its
  own `data_rx`: the channel has no valid receiver, and `init`'s `send` on
  it reports `init: send to revoked receiver: ECLOSED`. Then it sends `go`.
- `revoked` wakes up, finds `console_write` failing with `EBADCAP` and
  `recv` on slot 3 failing with `EBADCAP`, and exits 42
  (`init: revoked exited 42`). Any other outcome is a different exit code
  (43: the console still worked, 44: the receiver still worked, 45: the
  setup failed), and a line printed after the revocation (`revoked: this
  line must not appear`) fails the scenario on its own.

## The runtime shelf: snapshots

An instance whose initialisation is expensive (the model is an
interpreter that parses its standard library before it can run a line)
can be started once, captured, and started again from the capture: a
memory copy instead of the initialisation. `instance_snapshot(cap)` (SDK:
`Instance::snapshot`, needs the `SNAPSHOT` right every `spawn` grants)
takes the state of an instance that is parked in a host call (blocked in
`recv`, `wait_any` or `instance_wait`, or sleeping): the contents of its
linear memory, its globals and tables and which segments it has dropped,
nothing of the execution in flight, as `kalium-wasm`'s `Instance::snapshot`
captures it. It returns a capability on a new `Snapshot` object
(`READ|DERIVE|TRANSFER`), which remembers the module, and is freed with
its last capability. A runnable instance, or the caller itself, is refused
with `EBUSY` (the spawner yields and retries; the window is the few
instructions between a service's last message and its `recv`); an exited
one with `ECLOSED`, since the kernel frees an instance's Wasm state at
exit. `spawn_snapshot(cap, snapshot, caps, ncaps, class, period, budget,
entry, out)` (SDK: `spawn_snapshot`, `Snapshot::spawn`) is `spawn` from
the snapshot: the new instance's memory, globals and tables start as
captured (`Instance::from_snapshot`), no data or element segment and no
start function runs, and the execution starts at `entry`: `0` runs the
export `_start` as `spawn` does, `1` runs the export `resume`, the
convention for continuing from a restored state (`ENOENT` if the export is
missing or not `[] -> []`). The restored memory holds whatever the
original stored, including the slot numbers of its capabilities, so the
spawner passes the new instance's capabilities in the same order.

The boot-test scenario does this with `shelf` (module 9), spawned after
`revoked`:

- `shelf`'s `_start` fills a 256 KiB table with a deliberately slow
  deterministic computation (about 11.7 M instructions), records a
  checksum, prints `shelf: initialised`, sends `ready` to `init` and parks
  in `recv` on a `go` channel.
- `init` takes the time from the `spawn` to `ready` as the cost of a
  fresh start, snapshots `shelf` (`kalium: instance 0 (init) snapshotted
  instance 1 (shelf): 21 pages, 1376294 bytes`: the table, the SDK's
  1 MiB stack and the globals, as the plan's byte format would encode
  them) and starts three instances from the snapshot at `resume`, timing
  each from `spawn_snapshot` to its exit.
- `resume` runs on the restored memory: it recomputes the checksum of the
  table (about 1.05 M instructions, a tenth of building it), prints
  `shelf: resumed ok` and exits 0; a mismatch exits 1, a restore without
  the state exits 3.
- `init` closes `go` so that the original exits 0, prints `init: shelf
  init N us, restore M us` (M the fastest of the three) and drops the
  snapshot. `cargo xtask test` requires `M < N`; under QEMU on a
  laptop-class host the fresh start takes about 285 ms and a restore
  about 27 ms, the ratio of the instruction counts, since the copy of
  1.3 MB costs well under a millisecond.

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

### Resource quotas: `hog`

A service that holds no `Spawn` capability can still try to exhaust the
kernel through what any instance may create: memory objects of 16 MiB,
snapshots of other instances, channels with queues of 16 messages of
4096 bytes, and its own linear memory. Before the quotas, a loop of
`memobj_create(16 MiB)` (or of `instance_snapshot` on a parked child, or
`memory.grow` in many instances) filled the 252 MiB heap in a few dozen
calls, each refused with `ENOMEM` only once the heap was actually empty,
and the kernel's next ordinary allocation, a registry entry or the copy
of a 4096-byte message in `send`, failed:

```
memonly: start

kalium: panic at /rustc/.../library/alloc/src/alloc.rs:573:9: memory allocation of 1952 bytes failed
```

QEMU exit code 1, every instance gone: a denial of service by any
service. The kernel now accounts for every byte it allocates on an
instance's behalf ([prototype-plan.md](prototype-plan.md), "Resource
quotas"): per instance, the memory objects it created (32 MiB,
`kalium_abi::limits::MAX_MEMOBJ_BYTES_PER_INSTANCE`), the snapshots it
took (32 MiB, `MAX_SNAPSHOT_BYTES_PER_INSTANCE`) and the channels it
created (64, `MAX_CHANNELS_PER_INSTANCE`), charged until the object is
freed or the creator exits; and kernel-wide, one budget of the heap
minus an 8 MiB headroom (`kalium: instance quotas: ... budget 244 MiB
for all instances (8 MiB kernel headroom)` in the boot banner) that
linear memories and tables (on creation and on every `grow`), execution
stacks, memory objects, snapshots and queued messages (each at the cost
of the kernel's copy, `kalium_ipc::Message::cost_of`: its queue slot,
its payload and the capabilities it carries with their lineage nodes,
so that a zero-byte message carrying four capabilities is charged too)
are charged
to. Over a quota or over the budget is `ENOMEM` (a `memory.grow` returns
-1) with nothing allocated; the headroom is what the kernel's own small
allocations and the compiled programs of the boot image (about 2.5 MiB
for its modules) live in, and the host calls allocate everything sized
by an instance's arguments fallibly, so a short heap is an `ENOMEM`
there too, not a panic. The same loop now stops at two objects and the
kernel goes on:

```
memonly: start
memonly: heap filled with 2 memobjs
memonly: send 4096 bytes under memory pressure: ok
memonly: done
kalium: instance 0 (init) exited 0 after 1415 instructions
kalium: init exited 0
```

The boot-test scenario checks this on every `cargo xtask test` with
`hog` (module 10), spawned after `shelf` with a console only. It creates
memory objects of the largest size until refused, grows its linear memory
until refused (64 pages at a time, halving on refusal), creates channels
until refused and fills each queue with 4096-byte messages until refused,
prints the counts, which must be exactly the quotas' values
(`hog: memobjs 2, pages 256, channels 64, messages 1024`), gives one
channel back, takes a fresh one and sends 4096 bytes on it
(`hog: send 4096 bytes on a fresh channel: ok`; `ENOMEM` would be
accepted, a panic would not), fills the rest of that queue with
zero-byte messages carrying four capabilities each (derived from a
channel end it keeps) and takes half of the queue back
(`hog: 15 cap-only messages sent, 8 received back`), prints
`hog: quota reached, kernel alive` and exits 0 holding everything: 32 MiB
of memory objects, 16 MiB of linear memory and 63 full queues of
4096-byte messages plus 8 cap-only ones, which the kernel frees at its
exit and reports: `kalium: instance 1 (hog) exit drained 1016 queued
messages, 4179200 budget bytes`, where the bytes must be exactly what
those messages cost the budget (`Message::cost_of`, computed by `xtask`
from the same function: 1008 messages of 4096 bytes and 8 of none with 4
capabilities, slot and capability lists included). A kernel that charged
only the payload would report 4128768 and fail on that line: a queue of
cap-only messages would then be free, and 32 instances with 64 full
queues each could park about 5 MiB of capability lists and lineage nodes
in the 8 MiB headroom. `init` then spawns `park` (released at once) and
creates a 16 MiB memory object of its own, and reports
`init: after hog, spawn ok`. When `init` exits, the kernel reports
`kalium: instance budget: 0 bytes in use`: everything charged at `send`,
`spawn`, `memobj_create` and `instance_snapshot` was released by `recv`,
exit and the objects' destruction. A
kernel that panics fails the scenario on the `kalium: panic` line (and
every line after it); one whose quota is missing or wrong fails on the
counts.

The kernel-wide budget is what stops several instances doing this at
once. A variant for running by hand (built like the unbounded `spawn`
variant above: an `init` that spawns hoarders which park instead of
exiting, each holding its share, with a console, a receive end to park
on and a send end to report through) shows it: the first four get their
full quotas (32 MiB of memory objects, 16 MiB of linear memory, 4 MiB of
queued messages, plus the reservation for the execution stack, about
52.5 MiB each), the fifth and sixth get what is left of the 244 MiB
(`memory.grow` returns -1 at 145 and 81 pages, `memobj_create` and
`send` return `ENOMEM`), the seventh `spawn` is refused, `init`'s own
16 MiB `memobj_create` is refused while they are parked and succeeds once
they have exited, and the kernel never panics:

```
hogpark 4: memobjs 2, pages 256, channels 64, messages 1024
hogpark 5: memobjs 2, pages 145, channels 64, messages 1024
hogpark 6: memobjs 0, pages 81, channels 64, messages 382
global: spawn hogpark refused: ENOMEM
global: 6 hogs parked
global: init memobj_create 16 MiB with hogs parked: ENOMEM
global: init send 4096 bytes with hogs parked: ok
kalium: instance 1 (park) exited 0 after 66655 instructions
...
global: init memobj_create 16 MiB after hogs exited: ok
kalium: init exited 0
```

(The hoarders give their channels back before reporting, since a `send`
needs room in the budget for its copy of the message; a service that
has exhausted the budget itself cannot even send until it frees
something, which is the point.)
