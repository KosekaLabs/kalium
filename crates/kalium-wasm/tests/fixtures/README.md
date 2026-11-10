# Test fixtures

`rust_fixture.wasm` is a real `wasm32-unknown-unknown` module produced by the Rust toolchain pinned in `rust-toolchain.toml` from the sources in `rust-src/`. It gives the decoder tests a module shaped like what the SDK examples will look like (imports, exported memory, function table, data segments, bulk memory, sign extension, saturating float-to-int, i64 and f64 arithmetic).

To regenerate it, from the repository root:

```sh
cargo build --release --target wasm32-unknown-unknown \
    --manifest-path crates/kalium-wasm/tests/fixtures/rust-src/Cargo.toml \
    --target-dir target/fixture
cp target/fixture/wasm32-unknown-unknown/release/kalium_wasm_fixture.wasm \
    crates/kalium-wasm/tests/fixtures/rust_fixture.wasm
```

`rust-src` has its own empty `[workspace]` table so it is not a member of the kalium workspace and does not interfere with `cargo test` at the root. The tests only assume the exports and the single `env.host_log` import named in `rust-src/src/lib.rs`, so a rebuild with a newer toolchain should keep them passing; `tests/decode.rs` has a test that prints a summary of what it decoded if you need to update expectations.
