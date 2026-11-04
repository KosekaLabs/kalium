# Running the prototype

The M1 prototype boots on QEMU `virt` (aarch64, cortex-a72, GICv2), runs the
boot-test scenario from [prototype-plan.md](prototype-plan.md) with the
example services from `sdk/examples` (the six of the plan plus the hostile
`greedy`/`park` pair, see [Testing hostile services](#testing-hostile-services))
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
process ids), so `fair`, `faulty`, `hello_std`, `hello_c` and `greedy` all
show up as instance 1 after `ping` is gone. The instruction count on each
exit line is the interpreter's fuel accounting for that instance (a chunk
that ends in a trap is not counted, so `faulty` shows 0).

```
kalium: boot
kalium: timer frequency 62500000 Hz
kalium: entered at EL1, dtb at 0x40000000 (1048576 bytes), RAM 0x40000000..0x50000000 (256 MiB) from the device tree
kalium: image 0x40200000..0x403a6000 (W^X: text RX, rodata RO, data/stacks RW, guards unmapped), heap 0x403a6000..0x50000000 (252 MiB)
kalium: boot image: 10 modules
kalium:   module 0: init (4421 bytes)
kalium:   module 1: ping (1945 bytes)
kalium:   module 2: pong (2668 bytes)
kalium:   module 3: rt (1059 bytes)
kalium:   module 4: fair (310 bytes)
kalium:   module 5: faulty (157 bytes)
kalium:   module 6: park (215 bytes)
kalium:   module 7: greedy (3782 bytes)
kalium:   module 8: hello_std (101148 bytes)
kalium:   module 9: hello_c (202342 bytes)
kalium: instance limits: 256 pages, 65536 stack values, call depth 1024, 256 cap slots
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
kalium: instance 0 (init) exited 0 after 4707 instructions
kalium: interpreter ran 19023785 instructions in 1238 ms (15356057 instr/s)
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
its stdin; see [The WASI demos](#the-wasi-demos).
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
2. Add `<name>` to the module lists in `xtask/src/main.rs` and
   `kernel/build.rs` (`MODULES`), which must agree: the SDK modules
   (`WASM_MODULES` in `xtask`: `init=0 ... faulty=5`, `park=6`, `greedy=7`)
   come first, then the WASI demos (`DEMO_MODULES`: `hello_std=8`,
   `hello_c=9`). The position is the module index `spawn` takes, and the
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
is reused by the next one created.

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

- `demos/hello-std` (module 8, `hello_std`): a Rust `std` program with
  `println!`, a `Vec`, `format!`, a `HashMap` (its hasher is seeded through
  `random_get`), floating point, `std::env::args`/`vars` (both empty),
  `thread::sleep` (through `poll_oneoff`) and `process::exit(3)`. Built by
  `cargo xtask demos` with
  `cargo build --release --target wasm32-wasip1 --manifest-path demos/Cargo.toml`
  (profile: `opt-level = "s"`, LTO, `panic = "abort"`, stripped); about
  100 KB, about 1.3 M interpreted instructions.
- `demos/hello-c` (module 9, `hello_c`): a C program with `printf` in
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
