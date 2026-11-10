# Contribution Guide

This file is primarily intended for developers who wish to fork the project and potentially contribute to it. This project follows [Phased Versioning](https://labs.koseka.net/phased-versioning) for its versioning, branching, and release rules, and [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) for its commit messages. So, make sure to read both first before contributing to the project in any way.

## Project Structure

Here are the main directories and files in the project:

```plaintext
.
├── kernel/            # The kernel binary (aarch64 only, not host-tested).
├── crates/
│   ├── kalium-abi/    # ABI constants shared by the kernel and the SDK.
│   ├── kalium-wasm/   # WebAssembly decoder, validator, interpreter and snapshots.
│   ├── kalium-hal/    # aarch64 hardware layer: boot, UART, MMU, GIC, timer, exceptions.
│   ├── kalium-mm/     # Frame allocator, kernel heap, memory objects.
│   ├── kalium-cap/    # Capability tables, rights, derivation lineage, revocation.
│   ├── kalium-ipc/    # Channels with capability transfer and wait queues.
│   └── kalium-sched/  # Real-time EDF and weighted fair scheduler.
├── sdk/               # Separate workspace: kalium-sdk and the example services (wasm32).
├── demos/             # Separate workspace: unmodified Rust std and C programs (WASI).
├── xtask/             # The build, run and test driver behind `cargo xtask`.
├── docs/              # Architecture, prototype contract, running guide, status.
├── Cargo.toml
└── package.json
```

The root workspace holds the kernel, its crates and `xtask`; its default members are the host-testable crates, so `cargo build` and `cargo test` at the root cover those. The kernel is built for `aarch64-unknown-none` through `cargo xtask`, which also builds the SDK and demo workspaces for WebAssembly and embeds their modules in the kernel's boot image. [docs/prototype-plan.md](docs/prototype-plan.md) is the binding contract for the crates and the host ABI: update it in the same change as the code.

Additionally, the `package.json` file configures the [Node](https://nodejs.org) environment required to run the [Trunk CLI](https://docs.trunk.io/code-quality/overview) metalinter.

## Setting Up the Development Environment

First, ensure that you have **Rust** installed on your machine. You can install **Rust** by following the instructions on the official [Rust website](https://www.rust-lang.org/tools/install). The toolchain and targets the project needs are pinned in `rust-toolchain.toml`, and `rustup` installs them on the first `cargo` command in the repository.

Second, the kernel boots in **QEMU**, so install `qemu-system-aarch64` (tested with QEMU 8.2; on Debian and Ubuntu it comes with the `qemu-system-arm` package). Rebuilding the C demo also needs clang and wasi-libc; without them, `xtask` uses the checked-in `demos/hello-c/prebuilt/hello_c.wasm`.

Third, this project uses the [**Trunk Code Quality CLI**](https://docs.trunk.io/code-quality/overview) as an npm package for formatting and linting the code, and **npm** as a package manager. So, make sure you have **node.js** and **npm** installed on your machine. You can install both of them from the official [node.js](https://nodejs.org) website.

Next, clone the `kalium` repository to your local machine and install the development dependencies:

```sh
git clone https://github.com/KosekaLabs/kalium.git          # Clones the repository.
cd kalium                                                   # Moves into the project directory.
npm install                                                 # Installs the development dependencies.
```

Since the **Trunk CLI** is used for formatting the code, it's best if you disable the _format on save_ option in your editor to avoid potential conflicts with the project's formatting configurations.

If you are using [**Zed**](https://zed.dev), you can locally disable the _format on save_ option of your editor for this project by adding the following line to the `.zed/settings.json` file at the root of the project directory:

```json
{
	"format_on_save": "off"
}
```

If you are using [**VSCode**](https://code.visualstudio.com), you can locally disable the _format on save_ option of your editor for this project by adding the following line to the `.vscode/settings.json` file at the root of the project directory:

```json
{
	"editor.formatOnSave": false
}
```

As for the linting, the project comes with its own linters and configurations, so if you have your own linters installed with custom configurations, you should make sure they don't conflict with the project's linters. You can check the list of linters (and formatters) along with their configurations in the `.trunk/trunk.yaml` file and the `.trunk/configs/` directory.

If you have followed all the steps correctly, you should now have a working development environment for the project. If you encounter any issues, feel free to open an issue on the project's [GitHub repository](https://github.com/KosekaLabs/kalium/issues).

## Linting and Formatting the Code

The linters and formatters work through git hooks, so they will run automatically when you commit changes. However, it's best to also run them manually before committing changes to avoid failing the commit hook.

To make sure the **Trunk CLI** is managing the git hooks, you can run the following command:

```sh
npm run trunk git-hooks sync
```

You can manually run the linters and formatters using the following commands:

```sh
npm run check                                               # Runs linters and formatters on all the changed files.
npm run check -- --all                                      # Runs linters and formatters on all the files in the repository.
```

You can manually format the code using the following commands:

```sh
npm run fmt                                                 # Formats all the changed files.
npm run fmt -- --all                                        # Formats all the files in the repository.
```

Rust formatting uses `.trunk/configs/.rustfmt.toml`, both through Trunk and through `cargo xtask fmt-check`, which checks the three workspaces. `cargo xtask clippy` runs clippy with `-D warnings` on the host crates, the kernel, the SDK and the demos.

## Testing and Building the Project

Keep these five checks green after every change:

```sh
cargo xtask test                                            # Host tests, wasm modules, kernel, self-test and the QEMU scenario.
cargo xtask boot-test                                       # HAL-only boot (fast).
cargo xtask selftest                                        # HAL self-test: W^X faults, stack guard, FP state, device tree.
cargo xtask clippy                                          # Clippy with -D warnings everywhere.
cargo xtask fmt-check                                       # Formatting of the three workspaces.
```

For quicker iterations on the host crates:

```sh
cargo test                                                  # Runs the host crates' tests.
cargo test -p kalium-wasm                                   # Runs the tests of one crate.
cargo run -p kalium-wasm --release --example bench          # Interpreter benchmark.
cargo run -p kalium-wasm --release --example startup        # Instantiate and snapshot timings.
```

You can build and boot the kernel using the following commands:

```sh
cargo xtask build                                           # Builds the wasm modules, then the kernel.
cargo xtask run                                             # Builds, then boots the kernel in QEMU.
```

New kernel behaviour gets a line in the boot scenario, checked by `xtask`'s ordered expectations, plus a must-fail fixture in `xtask`'s unit tests. `unsafe` lives only in `kalium-hal`, `kalium-mm`, the kernel binary and the SDK's raw imports, and host paths sized by service input use fallible allocation and return `ENOMEM`. [docs/running.md](docs/running.md) explains how to write and add a service and how to test hostile ones.

## Building the Documentation

You can build the documentation using the following commands:

```sh
cargo doc --workspace --no-deps                             # Builds the documentation at `target/doc/`.
cargo doc --workspace --no-deps --open                      # Builds the documentation and opens it in the browser.
```

## License

Copyright 2026 Amon Rayfa.

This project is licensed under the [Apache License (Version 2.0)](LICENSE).
