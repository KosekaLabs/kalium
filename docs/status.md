# Prototype status

This page records what the Kalium prototype does today, how it was verified,
and what is next. The numbers are from QEMU `virt` (cortex-a72, TCG) on a
4-core x86 host unless stated otherwise, so they say more about relative
cost than about real hardware.

## What runs

`cargo xtask test` boots the kernel and runs a scenario of thirteen
WebAssembly services from the boot image. In order:

| Service | What it proves |
| --- | --- |
| `init` | Console output through a capability; spawns and waits on everything below. |
| `ping`, `pong` | 100 messages over a channel, the last one carrying a memory-object capability that pong reads back. |
| `rt`, `fair` ×2 | A 2 ms / 500 µs real-time service meets 398 to 400 of 400 periods while two fair services busy-loop. |
| `faulty` | An out-of-bounds load traps; the kernel reports it and init observes exit code -1. |
| `park`, `greedy` | A hostile spawner is stopped at the 32-instance quota and its slots are reclaimed. |
| `revoked` | A derived console capability is revoked while in use; the holder gets `EBADCAP` and a revoked channel end reports `ECLOSED` to its peer. |
| `shelf` | An instance is snapshotted after an expensive initialisation and three copies are restored from the snapshot, about 10x faster than a cold start. |
| `hog` | A hostile service hits every resource quota (memory objects, pages, channels, queued messages) and the kernel stays alive; a capability sent over its own channel is collected as an unreachable cycle; the instance budget settles at zero. |
| `hello_std` | An unmodified Rust `std` program for `wasm32-wasip1` (println, Vec, HashMap, floats, sleep, exit code 3). |
| `hello_c` | An unmodified C program built with clang and wasi-libc (printf, malloc, qsort, stdin from a channel, exit code 5). |

The whole scenario executes about 55 million Wasm instructions in about 2 s
of QEMU time, between 25 and 40 million instructions per second interpreted
depending on host load. The host-side test suites hold 266 tests.

## Components

- **Kernel binary** (`kernel/`): boot image, object registry, host ABI
  (`kalium:core`, `kalium:ipc`, `kalium:memory`, `kalium:spawn`, and
  `wasi_snapshot_preview1`), run loop with measured fuel-to-time calibration
  and timer preemption, trap reporting, instance and resource quotas with a
  kernel-wide budget, and a collector for objects reachable only through
  queued capabilities.
- **kalium-wasm**: decoder, validator and interpreter for WebAssembly 2.0
  (MVP, sign extension, non-trapping conversions, bulk memory, reference
  types, multi-value). Passes all 90 files of the vendored core spec suite
  with no skipped directives. Resumable execution with exact fuel, restartable
  host calls, compiled dense instruction stream with superinstructions,
  per-module program cache, shared tables/memories/globals across instances,
  snapshots with a versioned byte format. No `unsafe`.
- **kalium-hal** (aarch64): boot, PL011 UART, MMU with W^X page attributes
  and unmapped guard pages, GICv2, generic timer, exception vectors with full
  FP/SIMD state, device-tree RAM sizing, semihosting exit with decoded fault
  reports, and a self-test mode.
- **kalium-mm**: frame allocator, first-fit kernel heap, memory objects.
- **kalium-cap**: capability tables with rights, derivation lineage and
  revocation.
- **kalium-ipc**: bounded channels with capability transfer and wait queues.
- **kalium-sched**: EDF real-time and weighted fair classes, pure and
  host-tested with a simulated clock.
- **SDK** (`sdk/`): `no_std` wrappers over the host ABI for `wasm32`
  services, with compile-time checks that its import table matches
  `kalium-abi`.
- **Demos** (`demos/`): the Rust `std` and C programs above.

## Performance

Interpreter throughput on the host (`cargo run -p kalium-wasm --release
--example bench`, x86 Xeon 2.1 GHz, million Wasm instructions per second):

| Workload | First version | Now |
| --- | --- | --- |
| fib (calls) | 154 | 369 to 549 |
| loop (locals, branches) | 190 | 1050 to 1257 |
| memcpy (loads, stores) | 178 | 845 to 897 |
| mandelbrot (f64) | 237 | 459 to 608 |
| call_indirect | 159 | 660 to 740 |
| host calls | 163 | 357 to 487 |

Instantiating a 100 KB module takes about 30 µs once its program is cached
(1.3 ms before the cache). Restoring a snapshot is one memcpy of the memory
plus the table and globals.

## How it was verified

Every round of work ended with an adversarial review by an independent
agent, each finding independently challenged by a second agent before being
fixed. Confirmed and fixed, each with a regression test:

- A blocked host call reached through `call_indirect` resumed with a shifted
  stack (sandbox correctness).
- Tables were unbounded by instance limits; decode-time locals could amplify a
  2 KB module into hundreds of megabytes.
- Instance records were never reclaimed and had no quota.
- The binding plan disagreed with the Wasm crate API; imported function
  addresses inside the importer's own range were accepted; the RAM mapping
  extended past the end of RAM.
- `fd_read` reported EOF on a leading empty iovec and blocked on a zero-byte
  read.
- A failed revocation reported success.
- A service could exhaust the kernel heap and make a later unchecked kernel
  allocation panic (now per-instance quotas, a kernel-wide budget with
  headroom, fallible allocation on every host path).
- In-flight capabilities in queued messages were not charged; snapshot copies
  allocated infallibly.
- A capability queued in its own channel formed a reference cycle that was
  never freed (now a mark-and-sweep collector over queued capabilities).
- The exception vector save macro overflowed its 0x80-byte slot and was
  silently shifted by alignment (found while adding FP/SIMD state).

Refuted after verification (not reachable or overstated in this prototype):
missing FP save impact, W^X and stack guard as bugs (both since implemented
anyway), a WASI stack-size spawn failure, an unmetered `cap_revoke`, and
others listed in the workflow logs.

Gates that must stay green: `cargo xtask test`, `boot-test`, `selftest`,
`clippy` (with `-D warnings` across host crates, kernel and SDK) and
`fmt-check`. The CI definition in `.github/workflows/ci.yml` runs them.

## Known limitations

- Single core. The Wasm crate uses `Rc`/`RefCell` handles and is `!Send`; SMP
  needs per-core ownership or locking.
- Interpreter only. No ahead-of-time compilation yet; the W^X mapping and
  page-granular attributes are in place for it.
- No devices beyond UART and timer. No display, input, storage, network or
  NPU.
- Linear memory is `Vec`-backed and bounds-checked in software; MMU-backed
  memories with guard pages wait for the AOT path that would benefit.
- WASI is a compatibility shim: no filesystem, no sockets, clock and random
  are best-effort, `poll_oneoff` handles clock subscriptions only.
- Rust can not target a second linear memory, so memory objects are copied
  rather than mapped into services; a service may import its whole memory
  instead.

## Next

See the roadmap in [architecture.md](architecture.md): AOT compilation,
SMP, display and input on QEMU, then a real SoC.
