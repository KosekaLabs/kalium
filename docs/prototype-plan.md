# Prototype plan (milestone M1)

This is the working brief for the first Kalium prototype. It is the contract
between the crates; code that disagrees with this document is wrong until the
document is changed. The long-term design is in [architecture.md](architecture.md).

## M1 definition of done

`cargo xtask test` passes, which means:

1. All host-side unit tests pass (`cargo test` at the workspace root).
2. The kernel boots on QEMU `virt` (aarch64, cortex-a72, GICv2) and exits
   cleanly with code 0 through semihosting after the boot-test scenario:
   - the init service (a Wasm module from the SDK examples) prints
     `hello from init` through the console capability;
   - init spawns `ping` and `pong` instances connected by a channel and they
     exchange 100 messages, the last one carrying a transferred capability;
   - init spawns one real-time instance (`rt`, period 2 ms, budget 500 µs)
     and two fair instances that busy-loop; the RT instance reports that it
     met at least 95% of its periods while the fair ones were running;
   - a `faulty` instance traps on an out-of-bounds load and the kernel reports
     the trap and continues; init observes the exit through `instance_wait`.
3. `cargo xtask build` produces `target/aarch64-unknown-none/release/kalium`
   and `cargo xtask run` boots it interactively in QEMU.

## Conventions

- Edition 2021, stable toolchain pinned by `rust-toolchain.toml`. No nightly.
- Every crate is `#![no_std]`. Crates that need heap use `extern crate alloc`.
  Only `kalium-hal`, `kalium-mm` and the kernel binary may contain `unsafe`;
  every other crate has `#![forbid(unsafe_code)]`.
- Every crate except `kalium-hal` and the kernel binary builds and tests on
  the host (`x86_64-unknown-linux-gnu`). `kalium-hal` is an empty crate on
  non-aarch64 targets (`#[cfg(target_arch = "aarch64")]` around everything).
- Host tests live next to the code (`#[cfg(test)]`), with `std` enabled only
  under `cfg(test)` where needed.
- No panics on untrusted input. Decoding a module, resolving a capability,
  receiving a message: all return `Result`. `unwrap`/`expect`/indexing that
  can panic on attacker-controlled data is a bug.
- Dependencies: keep to a minimum and only pure-Rust, `no_std`-capable
  crates. Allowed without discussion: `libm`, `spin`, `bitflags`,
  `hashbrown`, `tock-registers` or `aarch64-cpu` for the HAL. Host-only dev
  dependencies (`wat`, `wast`, `proptest`) are fine.
- Formatting: `cargo fmt`. Lints: `cargo clippy` clean on the default members.
- Commit messages: imperative, explain why.

## Workspace layout

```
Cargo.toml              workspace; default-members are the host-testable crates
.cargo/config.toml      `cargo xtask` alias; aarch64 rustflags (linker script)
rust-toolchain.toml
kernel/                 the kernel binary (aarch64 only), link.ld, boot image
crates/kalium-abi       ABI constants shared with the SDK. No dependencies.
crates/kalium-wasm      Wasm decoder, validator, interpreter. alloc only.
crates/kalium-hal       aarch64: boot, UART, MMU, exceptions, timer, GIC, exit
crates/kalium-mm        frame allocator, global allocator, memory objects
crates/kalium-cap       capability objects and tables
crates/kalium-ipc       channels with capability transfer
crates/kalium-sched     threads, RT (EDF) and fair classes, run loop
sdk/                    separate workspace targeting wasm32-unknown-unknown
sdk/kalium-sdk          safe wrappers over the host ABI
sdk/examples/*          init, ping, pong, rt, fair, faulty
xtask/                  `cargo xtask build|run|test|wasm|clippy`
```

## Host ABI (the contract between kernel and SDK)

All host functions are core-Wasm imports. Pointers and lengths are `i32`
offsets into the instance's linear memory; the kernel bounds-checks every one
and returns `EFAULT` on failure. Every fallible call returns an `i32` errno
(`0` is OK). Out-parameters are written into linear memory at the given
pointer. Capabilities are `i32` indices into the instance's capability table;
`-1` is never valid.

### Namespaces and when they resolve

| Module name | Resolves when |
| --- | --- |
| `kalium:core` | always |
| `kalium:ipc` | always |
| `kalium:memory` | always |
| `kalium:spawn` | the instance holds at least one capability of kind `Spawn` at instantiation |

An import that does not resolve is a link error and the instance is not
created (`ENOENT` to the spawner).

### `kalium:core`

| Function | Signature | Notes |
| --- | --- | --- |
| `console_write` | `(cap: i32, ptr: i32, len: i32) -> i32` | Needs `Console` kind with `WRITE`. Writes bytes to the UART. |
| `cap_drop` | `(cap: i32) -> i32` | Removes the slot. |
| `cap_derive` | `(cap: i32, rights: i64, out_cap: i32) -> i32` | Needs `DERIVE`. New cap has `rights & old.rights` and never gains `DERIVE` unless the old had it. Writes new index as `i32` at `out_cap`. |
| `cap_info` | `(cap: i32, out: i32) -> i32` | Writes `{kind: u32, rights: u64}` (12 bytes, little-endian, no padding) at `out`. |
| `time_now` | `() -> i64` | Nanoseconds since boot, monotonic. |
| `yield_` | `() -> ()` | Gives up the rest of the time slice. |
| `sleep` | `(ns: i64) -> ()` | Blocks for at least `ns`. |
| `exit` | `(code: i32) -> ()` | Terminates the instance. Does not return. |
| `instance_id` | `() -> i32` | The calling instance's id (for logs). |

### `kalium:ipc`

| Function | Signature | Notes |
| --- | --- | --- |
| `channel_create` | `(out_send: i32, out_recv: i32) -> i32` | Creates a channel. Writes two cap indices: one with `SEND|DERIVE|TRANSFER`, one with `RECV|WAIT|DERIVE|TRANSFER`. Both refer to the same object. |
| `send` | `(cap: i32, msg: i32, len: i32, caps: i32, ncaps: i32) -> i32` | Needs `SEND`. `caps` points to `ncaps` `i32` indices; each must have `TRANSFER`; they are removed from the sender's table on success. `EAGAIN` if the queue is full (queue depth 16, message limit 4096 bytes, up to 4 caps). `ECLOSED` if no receiver cap exists any more. |
| `recv` | `(cap: i32, buf: i32, buf_len: i32, caps: i32, ncaps_max: i32, out_hdr: i32) -> i32` | Needs `RECV`. Blocks until a message is available. Copies at most `buf_len` bytes; the transferred caps are added to the receiver's table and their indices written at `caps`. `out_hdr` receives `{len: u32, ncaps: u32}` with the full message length (so truncation is detectable). `ECLOSED` if the queue is empty and no sender cap exists any more. |
| `try_recv` | same as `recv` | Non-blocking: `EAGAIN` instead of blocking. |
| `wait_any` | `(caps: i32, n: i32, out_index: i32) -> i32` | Blocks until one of `n` receive caps has a message; writes its position (0-based) at `out_index`. All need `WAIT`. |

### `kalium:memory`

| Function | Signature | Notes |
| --- | --- | --- |
| `memobj_create` | `(size: i64, out_cap: i32) -> i32` | Zeroed memory object, cap with `READ|WRITE|DERIVE|TRANSFER`. Size limit 16 MiB in M1. |
| `memobj_size` | `(cap: i32, out_size: i32) -> i32` | Writes `i64`. |
| `memobj_read` | `(cap: i32, offset: i64, ptr: i32, len: i32) -> i32` | Needs `READ`. |
| `memobj_write` | `(cap: i32, offset: i64, ptr: i32, len: i32) -> i32` | Needs `WRITE`. |

Mapping memory objects into linear memory is deferred to the multi-memory /
MMU-backed design; M1 copies.

### `kalium:spawn`

| Function | Signature | Notes |
| --- | --- | --- |
| `spawn` | `(cap: i32, module: i32, caps: i32, ncaps: i32, class: i32, period_ns: i64, budget_ns: i64, out_cap: i32) -> i32` | Needs `Spawn` kind with `SPAWN`. `module` is an index into the boot image's module table. The `ncaps` caps (each needs `TRANSFER`) are moved to the child, becoming its slots `0..ncaps` in order. `class` is `0` fair, `1` real-time. Returns an `Instance` cap with `WAIT|KILL|DERIVE|TRANSFER`. |
| `instance_wait` | `(cap: i32, out_code: i32) -> i32` | Needs `WAIT`. Blocks until the instance exits; writes exit code (`i32`; traps report `-1`, kill reports `-2`). |
| `instance_kill` | `(cap: i32) -> i32` | Needs `KILL`. |
| `module_count` | `() -> i32` | Number of modules in the boot image. |

### Errno

| Value | Name | Meaning |
| --- | --- | --- |
| 0 | `OK` | |
| 1 | `EBADCAP` | Index is not a live slot. |
| 2 | `EPERM` | Slot exists but lacks the right, or is the wrong kind. |
| 3 | `EINVAL` | Bad argument. |
| 4 | `ENOMEM` | Out of memory or over a quota. |
| 5 | `EAGAIN` | Would block / queue full. |
| 6 | `ECLOSED` | Peer gone. |
| 7 | `ETOOBIG` | Message or object too large. |
| 8 | `ENOENT` | No such module / unresolved import. |
| 9 | `EFAULT` | Pointer/length outside linear memory. |
| 10 | `EBUSY` | Resource in use. |

### Rights (u64 bit flags)

`READ=1, WRITE=2, SEND=4, RECV=8, DERIVE=16, TRANSFER=32, KILL=64, WAIT=128, SPAWN=256`.

### Object kinds (u32)

`Console=1, Channel=2, MemObj=3, Instance=4, Spawn=5`.

`kalium-abi` defines all of the above as constants plus the import module and
function names, and nothing else. It has no dependencies and no `unsafe`.

## Crate contracts

### kalium-wasm

Decoder, validator and interpreter for core WebAssembly as emitted by Rust's
`wasm32-unknown-unknown` target on the pinned toolchain. Required features:
MVP, mutable globals, sign-extension, non-trapping float-to-int, bulk memory
(`memory.copy/fill/init`, `data.drop`, `table.copy/init/fill/grow/size`,
`elem.drop`), multi-value (block types with params and results), reference
types (`ref.null/is_null/func`, `table.get/set`, multiple tables). SIMD and
threads are out of scope for M1 (reject with a clear error). Floats use `libm`.

Execution is resumable: the interpreter keeps its value stack, frames and
program counter in an `Execution` struct and runs for a fuel budget, so the
scheduler can preempt it at any back-edge or call. Host calls are restartable:
if the host returns `Block`, the interpreter leaves the arguments on the stack
and rewinds to the call instruction, so resuming re-issues the call.

```rust
pub enum Value { I32(i32), I64(i64), F32(f32), F64(f64), FuncRef(Option<u32>), ExternRef(Option<u32>) }
pub struct Module { /* decoded + validated */ }
impl Module {
    /// Decode and validate. Never panics on malformed input.
    pub fn new(bytes: &[u8]) -> Result<Module, Error>;
    pub fn imports(&self) -> &[Import];          // module, name, desc (func type index, table, memory, global)
    pub fn exports(&self) -> &[Export];
    pub fn func_type(&self, idx: u32) -> &FuncType;
}
pub struct HostFuncId(pub u32);
pub enum HostOutcome { Continue, Block, Exit(i32) }
pub trait Host {
    /// Called with the args; on Continue, `results` must be filled.
    fn call(&mut self, ctx: &mut HostCtx<'_>, func: HostFuncId, args: &[Value], results: &mut [Value]) -> Result<HostOutcome, Trap>;
}
/// What a host function may touch: the calling instance's linear memory.
pub struct HostCtx<'a> { pub memory: &'a mut Memory /* ... */ }
impl Memory { pub fn read(&self, off: u32, buf: &mut [u8]) -> Result<(), Trap>; pub fn write(&mut self, off: u32, data: &[u8]) -> Result<(), Trap>; pub fn size_bytes(&self) -> usize; }
pub struct Instance { /* module ref, memory, tables, globals, resolved imports */ }
impl Instance {
    /// Resolve each function import through `resolve`; Err(()) is a link error.
    pub fn new(module: Arc<Module>, resolve: &mut dyn FnMut(&str, &str, &FuncType) -> Result<HostFuncId, ()>, limits: &Limits) -> Result<Instance, Error>;
    /// Runs the start function (if any) to completion is NOT done here; see `Execution::start`.
    pub fn export_func(&self, name: &str) -> Option<u32>;
}
pub struct Execution { /* stack, frames, pc, state */ }
impl Execution {
    pub fn new(inst: &Instance, func: u32, args: &[Value]) -> Result<Execution, Error>;
    pub fn run(&mut self, inst: &mut Instance, host: &mut dyn Host, fuel: u64) -> ExecResult;
}
pub enum ExecResult { Done(Vec<Value>), OutOfFuel, Blocked, Exited(i32), Trap(Trap) }
pub struct Limits { pub max_memory_pages: u32, pub max_stack_values: usize, pub max_call_depth: usize }
```

Fuel: 1 unit per instruction is fine; `run` returns `OutOfFuel` promptly after
the budget is spent. Memory grows via `Vec<u8>` in M1 with the page limit
enforced. Every memory access is bounds-checked (`Trap::OutOfBounds`).
Tests: unit tests per module, plus a `wast` runner (`tests/spec.rs`) that runs
the subset of the official spec test suite covering the required features;
the `.wast` files are vendored under `crates/kalium-wasm/tests/spec/`.

### kalium-hal (aarch64 only)

- `boot.S` / `global_asm!`: `_start` parks all but core 0, sets the stack,
  zeroes `.bss`, drops from EL2 to EL1 if entered at EL2, calls `kmain`.
- `link.ld` in `kernel/` (load address `0x4008_0000` for QEMU `virt` with
  `-kernel`). Sections: text, rodata, data, bss, a 1 MiB boot stack, heap
  symbols `__heap_start`/`__heap_end` covering RAM up to the end of a
  configurable size (default 128 MiB from `0x4000_0000`).
- MMU: identity map with 2 MiB blocks (or 1 GiB where convenient): RAM as
  normal write-back cacheable, the first 1 GiB (MMIO) as device memory.
  Enabled before any atomic or allocator use.
- PL011 UART at `0x0900_0000`: `fn putc(u8)`, `fn write_str`, and a `print!`/
  `println!` macro pair behind a spinlock.
- Exception vectors for EL1 (sync, IRQ, FIQ, SError from the current EL and
  from lower EL) with full register save; sync exceptions print ESR/ELR/FAR
  and panic. IRQ dispatches through the GIC.
- GICv2 (`dist 0x0800_0000`, `cpu 0x0801_0000`): enable, route, ack, EOI.
- Generic timer: `fn now_ns() -> u64` from `CNTPCT_EL0`/`CNTFRQ_EL0`,
  `fn set_timer(ns_from_now)`, IRQ 30 (PPI) wired to a handler the scheduler
  registers: `fn set_tick_handler(fn())`.
- Exit: semihosting `SYS_EXIT` with an exit code (`fn exit(code: u32) -> !`),
  so QEMU run with `-semihosting-config enable=on,target=native` propagates
  the code; fall back to PSCI `SYSTEM_OFF` via HVC.
- Panic handler that prints the location and message, then `exit(1)`.
- `fn wait_for_interrupt()` (`wfi`), `fn enable_irqs()`, `fn disable_irqs()`.

Verified by: the kernel binary booting on QEMU, printing, taking 10 timer
ticks and exiting with code 0 (`cargo xtask run -- --boot-only`).

### kalium-mm

- `FrameAllocator`: bitmap over 4 KiB frames in `[__heap_start, __heap_end)`;
  `alloc_frames(n) -> Option<PhysAddr>`, `free_frames`. Host-testable over a
  `Vec<u8>` arena.
- `KernelHeap`: a first-fit free-list allocator with coalescing implementing
  `core::alloc::GlobalAlloc` behind a spinlock, initialized with a region.
  Host-tested with randomized alloc/free sequences.
- `MemObject`: `{ bytes: Vec<u8> }` with `read(offset, buf)`, `write(offset, data)`
  bounds-checked; size limit enforced at creation. (Backing by frames with
  MMU mapping is post-M1.)

### kalium-cap

```rust
pub struct Rights(u64);                      // bitflags from kalium-abi
pub enum ObjectKind { Console, Channel, MemObj, Instance, Spawn }
pub struct ObjectId(u64);                    // kernel-wide unique, never reused
pub struct Capability { pub object: ObjectId, pub kind: ObjectKind, pub rights: Rights }
pub struct CapTable { /* slots: Vec<Option<Capability>>, free list */ }
impl CapTable {
    pub fn new(limit: usize) -> Self;
    pub fn insert(&mut self, cap: Capability) -> Result<CapIndex, CapError>;   // ENOMEM at limit
    pub fn get(&self, idx: CapIndex) -> Result<&Capability, CapError>;         // EBADCAP
    pub fn check(&self, idx: CapIndex, kind: ObjectKind, rights: Rights) -> Result<&Capability, CapError>; // EBADCAP / EPERM
    pub fn take(&mut self, idx: CapIndex) -> Result<Capability, CapError>;      // for transfer
    pub fn derive(&mut self, idx: CapIndex, rights: Rights) -> Result<CapIndex, CapError>;
    pub fn drop_slot(&mut self, idx: CapIndex) -> Result<(), CapError>;
}
```
`CapIndex` is a newtype over `u32`; conversion from the ABI's `i32` rejects
negatives. The object registry (what an `ObjectId` refers to) lives in the
kernel, not here.

### kalium-ipc

```rust
pub struct Message { pub bytes: Vec<u8>, pub caps: Vec<Capability> }
pub struct Channel { /* bounded VecDeque<Message>, sender/receiver cap counts */ }
impl Channel {
    pub fn new(depth: usize, max_len: usize, max_caps: usize) -> Self;
    pub fn send(&mut self, msg: Message) -> Result<(), IpcError>;    // Full, TooBig, Closed
    pub fn try_recv(&mut self) -> Result<Message, IpcError>;         // Empty, Closed
    pub fn sender_added/sender_dropped/receiver_added/receiver_dropped(&mut self);
    pub fn is_readable(&self) -> bool;
}
```
Blocking is the scheduler's job: the kernel's host functions return `Block`
and park the thread on the channel's wait list (`WaitQueue` type in this
crate: `park(thread_id)`, `wake_one()`, `wake_all()`).

### kalium-sched

```rust
pub struct ThreadId(pub u32);
pub enum Class { Fair { weight: u32 }, RealTime { period_ns: u64, budget_ns: u64 } }
pub enum State { Ready, Running, Blocked, Sleeping { until_ns: u64 }, Exited }
pub struct Scheduler { /* run queues, clock */ }
impl Scheduler {
    pub fn add(&mut self, id: ThreadId, class: Class, now_ns: u64);
    pub fn pick_next(&mut self, now_ns: u64) -> Option<(ThreadId, u64 /* fuel or slice ns */)>;
    pub fn account(&mut self, id: ThreadId, ran_ns: u64, now_ns: u64, outcome: RunOutcome);
    pub fn block(&mut self, id: ThreadId); pub fn wake(&mut self, id: ThreadId, now_ns: u64);
    pub fn sleep(&mut self, id: ThreadId, until_ns: u64); pub fn tick(&mut self, now_ns: u64);
    pub fn next_deadline_ns(&self) -> Option<u64>;   // for programming the timer
    pub fn rt_stats(&self, id: ThreadId) -> Option<RtStats>; // periods, met, missed
}
pub enum RunOutcome { Preempted, Yielded, Blocked, Slept, Exited }
```
Policy: RT threads are released every period with `budget_ns`; runnable RT
threads are picked earliest-deadline-first ahead of all fair threads; an RT
thread that exhausts its budget is demoted to the fair queue until its next
release. Fair threads use weighted round robin with a 1 ms base slice. The
scheduler is pure (no I/O, no globals) and is host-tested with a simulated
clock proving the RT guarantee under fair load.

### kernel

- `kmain`: HAL init (UART, MMU, exceptions, GIC, timer), heap init, print
  banner, load the boot image, create the init instance with
  `[Console(WRITE|DERIVE|TRANSFER), Spawn(SPAWN|TRANSFER)]`, run the scheduler
  loop until no threads remain, then `exit(0)` (or `exit(1)` if any instance
  trapped unexpectedly, except the `faulty` scenario which is expected).
- Boot image: `include_bytes!` of the SDK example modules in a fixed order
  (`init=0, ping=1, pong=2, rt=3, fair=4, faulty=5`) via a `build.rs` that
  reads `target/wasm/*.wasm` produced by `cargo xtask wasm`; a clear error if
  missing.
- Host ABI implementation: one `impl Host` that dispatches on `HostFuncId`,
  with the object registry (`Vec<Object>` keyed by `ObjectId`) holding
  channels, memory objects, instances.
- The run loop: `pick_next` → run `Execution` with fuel proportional to the
  slice → map `ExecResult` to `RunOutcome` → handle wakeups (channel sends
  wake receivers, exits wake waiters, timer ticks wake sleepers and release
  RT periods).
- Fuel-to-time: calibrate at boot by running a known loop for 1 ms, or use
  the timer tick to interrupt: the IRQ handler sets a preempt flag that the
  interpreter checks (`Execution::run` takes fuel; the kernel passes a
  moderate fuel and loops, checking the flag between runs).

### SDK and examples

`kalium-sdk` (wasm32, `no_std`): `extern "C"` imports exactly matching the
ABI tables above (`#[link(name = "kalium:core")]` etc.), safe wrappers
(`Console::write`, `Channel`, `MemObj`, `spawn`, `sleep`, `exit`), a
`println!` to a console cap, a panic handler calling `exit(101)`, and a
`#[no_mangle] pub extern "C" fn _start()` convention: each example exports
`_start` and takes its initial capabilities from slots `0..n`.

Examples (each a `cdylib`/`bin` built with `--target wasm32-unknown-unknown`):
`init`, `ping`, `pong`, `rt`, `fair`, `faulty` implementing the boot-test
scenario in the definition of done. Built by `cargo xtask wasm` into
`target/wasm/<name>.wasm`.

### xtask

`cargo xtask wasm` (build SDK examples), `build` (wasm, then kernel for
aarch64 in release), `run` (build, then QEMU), `test` (host tests, then
build, then QEMU in boot-test mode with a timeout, asserting the expected
lines and exit code), `clippy`, `fmt-check`. QEMU command:

```
qemu-system-aarch64 -M virt,gic-version=2 -cpu cortex-a72 -m 256M -nographic \
  -semihosting-config enable=on,target=native -no-reboot \
  -kernel target/aarch64-unknown-none/release/kalium
```

## Work breakdown

| Step | Depends on | Output |
| --- | --- | --- |
| A. abi + cap | – | constants, tables, tests |
| B. wasm core (decoder, IR, API skeleton) | – | `Module::new` decoding Rust-emitted modules |
| C. hal + kernel boot | – | boots on QEMU, prints, exits |
| D. mm | – | allocators + memobj, host tests |
| E. wasm validator | B | rejects bad modules, spec subset |
| F. wasm interpreter | B | runs modules, host calls, fuel, spec subset |
| G. ipc + sched | A, D | host tests incl. RT guarantee |
| H. sdk + examples | A | `.wasm` artifacts |
| I. integration | all | boot-test scenario passes |
| J. CI, docs, review | I | green workflow, updated README |
