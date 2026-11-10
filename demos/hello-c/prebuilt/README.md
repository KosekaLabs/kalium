# Prebuilt `hello_c.wasm`

`hello_c.wasm` is `../main.c` compiled for `wasm32-wasi`, checked in so that a machine without clang and wasi-libc (CI, for one) can still build the boot image. `cargo xtask demos` prefers a fresh build when the toolchain is present, compares it with this file and warns when they differ; without the toolchain it uses this file and says so.

Built on Ubuntu 24.04 with the distribution packages:

- `clang` 18.1.3 (`Ubuntu clang version 18.1.3 (1ubuntu1)`), `lld` 18.1.3 (`wasm-ld-18`)
- `wasi-libc` 0.0~git20230113.4362b18-3 (sysroot `/usr`: headers in `/usr/include/wasm32-wasi`, libraries in `/usr/lib/wasm32-wasi`; its objects were compiled by Ubuntu with clang 16.0.6, as the binary's `producers` section records)

with, from the repository root:

```text
clang --target=wasm32-wasi --sysroot=/usr -O2 -Wall -Wextra \
  -o demos/hello-c/prebuilt/hello_c.wasm demos/hello-c/main.c -nodefaultlibs -lc
```

`-nodefaultlibs -lc` is there because Ubuntu's clang 18 ships no wasm32 compiler-rt (`libclang_rt.builtins-wasm32.a`), which the default driver command line asks the linker for; wasi-libc's `libc.a` and this program need nothing from it. With a wasi-sdk (which bundles compiler-rt) the plain form works, and `cargo xtask demos` tries that first and falls back to the form above when the linker reports the missing compiler-rt:

```text
clang --target=wasm32-wasi --sysroot=<sysroot> -O2 -Wall -Wextra -o hello_c.wasm main.c
```

No `-D_WASI_EMULATED_*` is needed: the program uses `time()`, which is `clock_time_get`, not the emulated process clocks, signals or mman. Clang 18's default wasm32 features and the Ubuntu wasi-libc build produce only what the kernel's decoder accepts (the binary's `target_features` section lists `bulk-memory`, `mutable-globals` and `sign-ext`); no `-mno-simd128` or the like is required. The result is 202342 bytes, most of it wasi-libc's `printf` with floating-point support; it is byte-for-byte reproducible as long as the output file is named `hello_c.wasm` (the linker records the output's base name as the module name in the `name` section). It imports `args_get`, `args_sizes_get`, `clock_time_get`, `fd_close`, `fd_fdstat_get`, `fd_read`, `fd_seek`, `fd_write` and `proc_exit` from `wasi_snapshot_preview1`.

To regenerate after changing `main.c`: run the command above and commit the result together with the source (and update the size here).
