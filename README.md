# Kalium

Kalium is the kernel of [QualiaOS](https://github.com/koseka/qualia-os). It is
written entirely in Rust, runs on bare metal on mobile SoCs, and executes
exactly one kind of program: WebAssembly.

There is no native user space. Every service, every interpreter, every app
above the kernel is a WebAssembly module, isolated by the Wasm sandbox and the
MMU together, and authorized by capabilities rather than by user IDs. The
kernel does not know what JavaScript, HTML, an app or a browser is; those are
built out of WebAssembly modules that the OS ships next to it.

## What the kernel does

- Boots the SoC, sets up memory and the MMU, handles interrupts.
- Schedules WebAssembly instances, with a real-time class for the system
  agent and the audio pipeline.
- Runs WebAssembly: loads, validates, compiles or interprets, and executes
  modules with the sandbox guarantees the format promises.
- Mediates all access to hardware and to other instances through
  capabilities.
- Drives devices: display, touch, audio, storage, radios, and the NPU.

## What the kernel refuses to do

- Load or execute native code from user space.
- Carry a file system, network stack, window system or shell as kernel code
  where a WebAssembly service can do the job.
- Special-case any language, interpreter or application.

## Documentation

- [docs/architecture.md](docs/architecture.md): design, subsystems, the host
  ABI, security model, scheduling, memory, the NPU path, and a proposed
  crate layout.
- [docs/prototype-plan.md](docs/prototype-plan.md): the M1 prototype
  contract: definition of done, host ABI tables, crate contracts.
- [docs/running.md](docs/running.md): building, running and testing the
  prototype on QEMU; adding a service.
- [sdk/README.md](sdk/README.md): the service SDK and the example services.

## Status

Milestone M1, the first prototype, is complete: a `no_std` Rust kernel that
boots on QEMU `virt` (aarch64) and runs WebAssembly services through its own
interpreter, with capabilities, channels, memory objects and a two-class
(real-time EDF plus weighted fair) scheduler. Everything above the kernel is
WebAssembly built with the SDK in `sdk/`.

What the boot image does today (`docs/prototype-plan.md`, "M1 definition of
done"): the `init` service prints through its console capability, spawns
`ping` and `pong`, which exchange 100 messages over a channel with a memory
object transferred in the last one; spawns two busy-looping fair instances
and a real-time one (period 2 ms, budget 500 µs) that meets its periods under
that load; spawns `faulty`, which traps on an out-of-bounds load and is
reported by the kernel while everything else continues; hands its `Spawn`
capability to `greedy`, a hostile service that spawns and forgets instances
until the kernel's instance limit refuses it. `init` observes each exit
through `instance_wait`, and the kernel exits QEMU with the result.
Capabilities can be revoked (`cap_revoke` invalidates everything derived
from one, wherever it went) and instances can be shelved: `init` takes a
snapshot of a service that has finished an expensive initialisation
(`instance_snapshot`) and starts further instances from it
(`spawn_snapshot`, a memory copy instead of the initialisation) about ten
times faster than the fresh start (`docs/running.md`, "The runtime shelf").

The kernel also speaks WASI preview1: the `wasi_snapshot_preview1` import
module resolves for every instance, with the standard streams mapped onto
the instance's console and channel capabilities, `proc_exit` onto the
instance's exit, the clocks onto the HAL's counter, and no files, sockets
or preopens (`docs/prototype-plan.md`, "WASI preview1 compatibility"). So
ordinary programs built for `wasm32-wasip1` run unchanged: the boot image
carries two of them from `demos/`, a Rust `std` program (`println!`, `Vec`,
`HashMap`, floats, `thread::sleep`, `process::exit(3)`, about 100 KB) and a
C program compiled with clang and wasi-libc (`printf`, `malloc`, `qsort`,
`strtod`, `snprintf`, `time()`, exit code 5, about 190 KB), and `cargo xtask
test` asserts their output and exit codes.

The interpreter runs at roughly 11 to 16 million Wasm instructions per second
under QEMU TCG on a laptop-class host, and 300 to 450 million natively
(`cargo run -p kalium-wasm --release --example bench`). Ahead-of-time compilation, MMU-backed
instance memory, SMP and devices are future milestones.

To build and run (needs the toolchain from `rust-toolchain.toml` and
`qemu-system-aarch64`):

```
cargo xtask wasm    # build the SDK example services and the WASI demos to target/wasm/
cargo xtask build   # wasm, then the kernel for aarch64-unknown-none
cargo xtask run     # build, then boot in QEMU on the terminal
cargo xtask test    # host tests, build, boot in QEMU and check the scenario
cargo xtask boot-test  # HAL-only boot: UART, MMU, timer ticks, exit
cargo xtask selftest   # HAL self-test: W^X faults, stack guard, FP state, device tree
cargo xtask clippy     # lints for host crates, kernel and SDK
cargo xtask fmt-check  # formatting for both workspaces
```

[docs/running.md](docs/running.md) has the QEMU command line, the expected
output, how to write and add a service, and the WASI demos.

## License

Apache License 2.0. See [LICENSE](LICENSE).
