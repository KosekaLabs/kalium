# WASI demos

Ordinary programs for `wasm32-wasip1` that run on Kalium unchanged through
the kernel's WASI preview1 layer (`kernel/src/wasi.rs`;
`docs/prototype-plan.md`, "WASI preview1 compatibility"). Nothing here uses
the Kalium SDK or knows about capabilities: the spawner's console capability
in slot 0 is the program's stdout and stderr, a channel in slot 1 is its
stdin (a message per chunk of input; `hello_c` gets one), `proc_exit` is its
exit.

| Directory | Module | What | Built by |
| --- | --- | --- | --- |
| `hello-std/` | 8, `hello_std` | Rust `std`: `println!`, `Vec`, `format!`, `HashMap`, floats, args/env, `thread::sleep`, `process::exit(3)` | `cargo build --release --target wasm32-wasip1 --manifest-path demos/Cargo.toml` (this directory is its own Cargo workspace) |
| `hello-c/` | 9, `hello_c` | C via wasi-libc: `printf`, `malloc`/`free`, `qsort`, `strtod`/`strtol`, `snprintf`, recursion, `time()`, stdin through `fgets`, `readv` and `read`, `return 5` | clang + wasi-libc (`hello-c/prebuilt/README.md` has the command); `hello-c/prebuilt/hello_c.wasm` is used when the toolchain is absent |

`cargo xtask demos` builds both into `target/wasm/` (and `cargo xtask wasm`
runs it after the SDK examples); `cargo xtask test` boots the kernel and
asserts their output and exit codes. See `docs/running.md`, "The WASI
demos".
