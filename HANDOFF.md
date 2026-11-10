# Handoff: where the Kalium prototype stands

This file is for whoever picks the work up next, human or agent. It covers what exists, how to run it, what was verified, what is unfinished, and what to do next. It was written at the end of the first cloud session on 2026-11-10.

## 1. Repositories and branches

| Repository | Branch | Commits | Contents |
| --- | --- | --- | --- |
| `KosekaLabs/kalium` | `claude/qualiaos-architecture-wnanv0` | 16 | The kernel prototype (this repo). |
| `KosekaLabs/qualia-os` | `claude/qualiaos-architecture-wnanv0` (from `dev`) | 5 | QualiaOS design documents only, no code. |

**Update (2026-11-10, local session):** the work has been taken over on Amon's Mac. The bundles were unpacked: Kalium lives at `~/Projects/kalium` and QualiaOS at `~/Projects/qualia-os`, each with the work on its `dev` branch (the default branch under Phased Versioning; there is no `main`). Nothing has been pushed to GitHub yet, and no pull request has been opened. All in-repo links point at `KosekaLabs`.

## 2. The project in one paragraph

QualiaOS is an agentic, audio-first mobile OS in which every app is a web app and the browser engine is the OS. Kalium is its kernel: Rust, `no_std`, bare-metal aarch64, and it executes only WebAssembly. JavaScript, HTML and CSS engines are meant to be Wasm modules shipped next to the kernel (the "runtime shelf"), never kernel code. The design is in `qualia-os/docs/` and in `docs/architecture.md` here.

## 3. What Kalium does today

`cargo xtask test` boots the kernel on QEMU `virt` (cortex-a72, GICv2) and runs 13 Wasm services from the boot image, checking their output in order. The kernel exits QEMU with code 0 only if the whole scenario passes.

| Module | Index | What it proves |
| --- | --- | --- |
| `init` | 0 | Console output through a capability; spawns and waits on everything else. |
| `ping`, `pong` | 1, 2 | 100 channel messages; the last carries a memory-object capability. |
| `rt` | 3 | A 2 ms period / 500 µs budget real-time service meets about 400 of 400 periods. |
| `fair` ×2 | 4 | Busy fair-class load running alongside `rt`. |
| `faulty` | 5 | An out-of-bounds load traps; exit code -1 reaches `init`. |
| `park`, `greedy` | 6, 7 | A hostile spawner stops at the 32-instance quota; slots are reclaimed. |
| `revoked` | 8 | A revoked derived capability gives `EBADCAP`; a revoked channel end gives `ECLOSED` to the peer. |
| `shelf` | 9 | Snapshot after a heavy initialisation, three restores about 10x faster than a cold start. |
| `hog` | 10 | Hits every resource quota without crashing the kernel; a self-referencing channel cycle is collected; the instance budget settles at 0. |
| `hello_std` | 11 | Unmodified Rust `std` program (`wasm32-wasip1`) through the WASI layer, exit code 3. |
| `hello_c` | 12 | Unmodified C program (clang + wasi-libc), stdin fed from a channel, exit code 5. |

The module order is defined in `kernel/build.rs` (`MODULES`) and must match `xtask` (`WASM_MODULES`, `DEMO_MODULES`) and the indices hardcoded in `sdk/examples/init`.

## 4. Code map

```text
kernel/            kernel binary (aarch64 only, not host-tested)
  src/main.rs      kmain, limits, boot banner
  src/kernel.rs    object registry, instances, accounting, collector, revoke
  src/host.rs      host ABI: kalium:core / ipc / memory / spawn (bounds + cap checks)
  src/wasi.rs      wasi_snapshot_preview1 compatibility layer
  src/run.rs       run loop, fuel calibration, preemption, idle
  build.rs         embeds target/wasm/*.wasm as the boot image
  link.ld          load address 0x4020_0000, W^X sections, stack guards
crates/
  kalium-abi       ABI constants: errno, rights, kinds, import names, limits
  kalium-wasm      decoder, validator, interpreter, snapshots (no unsafe)
  kalium-hal       aarch64: boot, UART, MMU, GIC, timer, exceptions, FDT, selftest
  kalium-mm        frame allocator, kernel heap, memory objects
  kalium-cap       capability tables, rights, derivation lineage, revocation
  kalium-ipc       channels with capability transfer, wait queues, message cost
  kalium-sched     EDF real-time + weighted fair scheduler (pure, host-tested)
sdk/               separate workspace: kalium-sdk + 11 example services (wasm32)
demos/             separate workspace: hello-std (Rust std) and hello-c (C, prebuilt checked in)
xtask/             cargo xtask build|run|boot-test|selftest|wasm|demos|test|clippy|fmt-check
docs/              architecture.md, prototype-plan.md (binding contract), running.md, status.md
```

## 5. How to build and test

Requirements: the Rust toolchain in `rust-toolchain.toml` (stable with the `aarch64-unknown-none`, `wasm32-unknown-unknown` and `wasm32-wasip1` targets, `rust-src`, `llvm-tools`) and `qemu-system-aarch64` (tested with QEMU 8.2). Optional: clang (tested 18) and wasi-libc to rebuild the C demo; without them xtask uses `demos/hello-c/prebuilt/hello_c.wasm`. The first session used Rust 1.97 on Ubuntu 24.04.

```text
cargo xtask test       # everything: host tests, wasm + demos, kernel, selftest, QEMU scenario
cargo xtask run        # build and boot interactively
cargo xtask boot-test  # HAL-only boot (fast)
cargo xtask selftest   # HAL self-test: W^X faults, stack guard, FP state, device tree
cargo xtask clippy     # -D warnings on host crates, kernel, SDK, demos
cargo xtask fmt-check  # both workspaces
cargo test             # host crates only
cargo run -p kalium-wasm --release --example bench    # interpreter benchmark
cargo run -p kalium-wasm --release --example startup  # instantiate / snapshot timings
```

At handoff all five gates passed: `test`, `boot-test`, `selftest`, `clippy`, `fmt-check`. There are 266 host tests. CI is defined in `.github/workflows/ci-cd.yaml` but has never run, because nothing was pushed.

On Ubuntu, clang has no `libclang_rt.builtins-wasm32.a`, so the C demo is linked with `-nodefaultlibs -lc`. xtask retries with that automatically; `demos/hello-c/prebuilt/README.md` has the exact command.

To run a hostile or custom service, point `KALIUM_WASM_DIR` at a directory holding the 13 `.wasm` files with one replaced, then build the kernel. See "Testing hostile services" in `docs/running.md`.

## 6. Documents to read first

1. `docs/prototype-plan.md`: **binding contract.** It holds the host ABI tables, crate APIs, limits, the WASI and revocation rules, and the definition of done. `crates/kalium-wasm/tests/plan.rs` pins its kalium-wasm block, so an API change must update both together.
2. `docs/running.md`: commands, QEMU command line, expected output, how to write and add a service, and the hostile-service tests.
3. `docs/status.md`: what runs, components, performance, bugs found and fixed, known limitations.
4. `docs/architecture.md`: the long-term kernel design and roadmap.
5. `qualia-os/docs/*.md`: the OS, model and agent design, including `open-questions.md` with the model's dimension, memory and cadence budgets.

## 7. History of the work

| Milestone | Result |
| --- | --- |
| Design | QualiaOS docs (system, web apps and permissions, model, agent interface, open questions) and the Kalium architecture doc. |
| M1 | Workspace, all crates, HAL boot on QEMU, kernel integration, 6-service scenario. |
| Hardening 1 | Fixed: `call_indirect` resume bug, unbounded tables, decode-time locals amplification, unbounded instance records (quota 32). |
| M2 | Interpreter 2.3x faster; imported tables, memories and globals (90/90 spec files, no skips); W^X, stack guards, FP/SIMD save, FDT. |
| M3 | WASI preview1 layer, Rust std and C demos, compiled instruction stream with superinstructions (1.3x to 2.7x more). |
| M4 | Capability revocation with lineage; per-module program cache; snapshots and `spawn_snapshot`. |
| M5 | Per-instance quotas, kernel-wide budget with 8 MiB headroom, fallible allocation on host paths, queued-capability cycle collector. |

Every milestone ended with an adversarial review whose findings a second agent tried to refute before any fix. Ten bugs were confirmed and fixed, each with a regression test. They are listed in `docs/status.md`.

## 8. Key numbers

| Measure | Value |
| --- | --- |
| Interpreter on host (x86 Xeon 2.1 GHz) | fib ~370 to 550, loop ~1050 to 1250, call_indirect ~660 to 740 M instr/s |
| Interpreter under QEMU TCG | 25 to 40 M instr/s, depending on host load |
| Full scenario | ~55 M Wasm instructions, ~2 s of QEMU time |
| Instantiating a 100 KB module (program cached) | ~30 µs on host |
| Limits | 32 instances, 256 cap slots, 252 pages (256M QEMU), 16 MiB per memobj, 32 MiB memobjs and 32 MiB snapshots per instance, 64 channels per instance, queue depth 16, 4 KiB messages, 4 caps per message, derive depth 16 |

## 9. Known limitations and loose ends

Design limits, documented in `docs/status.md`:

- Single core. `kalium-wasm` uses `Rc`/`RefCell` and is `!Send`; SMP needs a redesign of ownership.
- Interpreter only, no AOT compiler yet.
- No devices beyond UART and timer.
- Linear memory is `Vec`-backed with software bounds checks.
- WASI has no filesystem or sockets; `random_get` is not cryptographic.
- Memory objects are copied, not mapped.

Smaller items reported by agents and not yet acted on:

- The kalium-wasm opcode space is full: 255 of 255 `u8` opcodes. A new superinstruction means dropping one or moving to `u16`.
- Release builds trust the validator for operand types on the raw value stack. Confusion is checked only in debug builds and is never memory-unsafe.
- The collector's reachability pass can cost on the order of all queued capabilities per `cap_drop` in a hostile setup. It is bounded, and exits batch it.
- Wake lists (`WaitQueue::wake_all`) still allocate infallibly. They are bounded by 32 instances and covered by the headroom.
- `Lineage::revoke_among` is no longer used by the kernel and is kept for its tests.
- `cargo doc -p kalium-wasm` reports a few broken intra-doc links to private items in `exec.rs`.
- Sample timings in `docs/running.md` drift run to run. Treat them as examples.
- The `instance_snapshot` path wraps the snapshot in an infallible `Arc::new`, which is small and fixed-size.
- The vendored spec tests are the WebAssembly 2.0 release (`opam-2.0.2`), not `main`, on purpose: `main` tests 3.0 features M1 rejects.

## 10. Pending decisions for the owner

1. **QuickJS demo.** JavaScript running on Kalium as a Wasm module is the natural headline demo. The first session's permission policy blocked downloading the QuickJS source, so it was replaced by the C demo. A local session can do it: build QuickJS with clang + wasi-libc as `demos/quickjs`, add it to the boot image and the scenario.
2. **Merging.** Settled: the work is on `dev`, the default branch under Phased Versioning. There is no `main`.
3. **Open design questions** in `qualia-os/docs/open-questions.md`, such as the model's KV-cache strategy and cadence. They do not block kernel work.

## 11. Suggested next steps

In roughly this order:

1. Push both `dev` branches and let CI run once. Fix whatever differs on GitHub's runners, most likely QEMU version or timing in the `rt` check.
2. QuickJS on Kalium (decision 1), which validates the runtime-shelf idea end to end.
3. Single-pass AOT compiler for aarch64 behind the existing interpreter API. W^X and page-granular attributes are already in place.
4. SMP: secondary cores, per-core run loops, `Arc`/locking in place of `Rc`/`RefCell`.
5. Display and input on QEMU (virtio-gpu, virtio-input) and a first compositor service.
6. Real hardware: choose a SoC by how open its NPU is.

## 12. Working conventions that held up

- Treat `docs/prototype-plan.md` as the contract and update it in the same change as the code.
- Keep the five gates green after every change: `cargo xtask test`, `boot-test`, `selftest`, `clippy`, `fmt-check`.
- New kernel behaviour gets a scenario line checked by xtask's ordered expectations, plus a must-fail fixture in xtask's unit tests.
- `unsafe` lives only in `kalium-hal`, `kalium-mm`, the kernel binary, and the SDK's raw imports.
- Host paths sized by service input must use fallible allocation and return `ENOMEM`.

## 13. Repository conventions applied (2026-11-10)

The local session brought the repository in line with Koseka's conventions:

- Edition 2024 in all three workspaces (`#[unsafe(no_mangle)]`, `unsafe extern`, let chains); version `0.0.0`, the zeroth generation under Phased Versioning.
- Trunk metalinter through npm (`package.json`, `.trunk/trunk.yaml`, configs in `.trunk/configs/`); `npm run check` / `npm run fmt`. Rust formatting follows `.trunk/configs/.rustfmt.toml`, which `cargo xtask fmt-check` uses too, and the codebase was reformatted with it.
- CI is `.github/workflows/ci-cd.yaml`: the standard host build and test on Ubuntu, macOS and Windows, plus a `kernel` job that runs `fmt-check`, `clippy` and the QEMU scenario.
- README, CONTRIBUTING, CHANGELOG, SECURITY and NOTICE from the Koseka templates, with the heading SVGs and the Koseka Labs banner in `img/`.
- `xtask` does not use Mabe yet: its commands all return exit codes (`i32`), and converting them is a rewrite of its own.
