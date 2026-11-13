# Kalium: instructions for agents

These are the standing instructions for any agent (or person) working in this repository. They always apply. What the prototype does today, how it was verified, its loose ends, the open decisions and the ordered next steps are in [docs/status.md](docs/status.md): read it before you start.

## What Kalium is

Kalium is the kernel of [QualiaOS](https://github.com/KosekaLabs/qualia-os), Koseka's open-source mobile operating system. This repository holds a working prototype: a Rust, `no_std`, bare-metal aarch64 microkernel whose only executable format is WebAssembly. It boots on QEMU `virt` and runs WebAssembly services through its own spec-tested interpreter, with capability-gated host calls, IPC with capability transfer, real-time plus fair scheduling, snapshots, quotas and a WASI preview1 layer.

There is no AI in the kernel, and none may be added. The model belongs to QualiaOS, one level up. Kalium is meant to make such a model possible (real-time cadence, pinned memory, NPU queues) while staying usable for an OS with no AI at all.

The prototype is a design probe, not a settled kernel. Whether Kalium ends up as this kernel of Koseka's own or as a stripped-down Linux is an open decision; see [docs/status.md](docs/status.md#open-decisions).

## Build, run and test

Requirements:

- The Rust toolchain pinned in `rust-toolchain.toml` (rustup installs it), with the `aarch64-unknown-none`, `wasm32-unknown-unknown` and `wasm32-wasip1` targets, `rust-src` and `llvm-tools`.
- `qemu-system-aarch64` (CI uses Ubuntu's `qemu-system-arm` package; QEMU 8.2 and 11.1 are known to work).
- Node.js and npm for the trunk metalinter (`npm install` once).
- Optional: clang with wasi-libc to rebuild the C demo; otherwise `demos/hello-c/prebuilt/hello_c.wasm` is used.

The gates, which must all stay green after every change:

```text
cargo xtask test       # host tests, wasm services, demos, kernel, selftest, then the QEMU scenario
cargo xtask boot-test  # HAL-only boot on QEMU
cargo xtask selftest   # HAL self-test on QEMU (W^X faults, stack guard, FP state, device tree), twice
cargo xtask clippy     # -D warnings on host crates, kernel, SDK and demos
cargo xtask fmt-check  # rustfmt with .trunk/configs/.rustfmt.toml
npm run check          # trunk metalinter (markdown, YAML, Rust, actions, secrets)
```

Other commands: `cargo xtask run` boots interactively (`Ctrl-A X` quits), `cargo test` runs only the host crates, and `npm run fmt` formats everything. `docs/running.md` has the full command reference, the QEMU command line, the expected console output and how to add or attack a service.

## How the code is organised

- `kernel/`: the kernel binary (aarch64 only). `crates/`: `kalium-abi`, `kalium-wasm` (decoder, validator, interpreter, snapshots), `kalium-hal` (aarch64), `kalium-mm`, `kalium-cap`, `kalium-ipc`, `kalium-sched`. `sdk/` and `demos/` are separate workspaces built for wasm32. `xtask/` drives everything.
- `docs/prototype-plan.md` is the binding contract: host ABI tables, crate APIs, limits, WASI and revocation rules. `crates/kalium-wasm/tests/plan.rs` pins part of it. Change the plan in the same commit as the code it describes.
- The boot module order lives in `kernel/build.rs` (`MODULES`) and must match `xtask` (`WASM_MODULES`, `DEMO_MODULES`) and the indices hardcoded in `sdk/examples/init`.

## Engineering rules

- New kernel behaviour gets a scenario line checked by xtask's ordered expectations, plus a must-fail case in xtask's unit tests.
- `unsafe` lives only in `kalium-hal`, `kalium-mm`, the kernel binary and the SDK's raw imports. `kalium-wasm` has none.
- Host paths whose size depends on service input use fallible allocation and return `ENOMEM`; a service must never be able to panic the kernel.
- Every bug fix comes with a regression test.
- Rust edition 2024 throughout, except the test fixture crate in `crates/kalium-wasm/tests/fixtures/rust-src`, which stays on 2021 on purpose.
- Error handling in std Rust binaries (today only `xtask`) follows Koseka's standard, [Mabe](https://github.com/AmonRayfa/mabe) (`#[mabe::main]`, `bail`, `Context`, `Result`). `xtask` has not been converted yet (it is on the list in docs/status.md). The `no_std` crates use their own error types.

## Repository conventions

- **Branches and versions:** [Phased Versioning](https://labs.koseka.net/phased-versioning), never SemVer. `dev` is the default and nightly branch, the only place breaking changes may land; there is no `main` or `master`. Version branches (`v1`, `v2`, …) are cut from `dev` and receive `dev` merges only. Until `v1` exists, every crate stays at version `0.0.0`, the zeroth generation.
- **Releases** go through the [`oyo`](https://github.com/AmonRayfa/oyo) CLI (`oyo gen`, `oyo phase <name>`, `oyo rev`), never raw `git branch` or `git tag` for version branches and release tags. A release adds its CHANGELOG entry on `dev`, merges `dev` into the version branch, then tags with `oyo`.
- **Commits** follow [Conventional Commits](https://www.conventionalcommits.org) and explain why.
- **Linting:** the trunk metalinter runs as an npm dependency. Its pre-commit hook formats and its pre-push hook checks; run `npm run trunk git-hooks sync` after cloning. Configs live in `.trunk/configs/`; there is no spell checker on purpose.
- **License:** Apache 2.0, stock text (`LICENSE`), with `NOTICE` and the community files (README, CONTRIBUTING, CHANGELOG, SECURITY) kept current.

## Never

- Never add AI, a model or model-specific code to the kernel.
- Never create a `main` branch, commit straight to the newest version branch, or rewrite `dev`'s published history.
- Never tag or cut versions by hand; use `oyo`.
- Never let a gate go red on `dev`, or skip a gate to land a change.
- Never commit secrets, credentials, or code you do not have the right to publish under Apache 2.0. This repository is public.
- Never put anything about how the QualiaOS model is trained (method, code, results) in this repository. That stays closed.
