# Vendored WebAssembly spec tests

The `.wast` files in this directory are the `test/core` scripts of the WebAssembly specification repository at tag `opam-2.0.2` (the WebAssembly 2.0 release), fetched unmodified from

    https://raw.githubusercontent.com/WebAssembly/spec/opam-2.0.2/test/core/<name>.wast

They are used by:

- `tests/spec_validate.rs`: every module directive must decode and validate; every `assert_malformed` and `assert_invalid` must be rejected with the matching error category and message (validator step E).
- `tests/spec.rs`: execution directives (interpreter step F). Modules that import a table, memory or global (from `spectest` or from a registered module) are skipped, because M1 resolves function imports only; the runner lists the files in which that happens (`FILES_WITH_SKIPS`) and fails on a skip anywhere else. Every other directive of every file runs.

The 2.0 release tag is used rather than `main` because later revisions of the suite exercise 3.0 features (multi-memory, memory64, GC, typed function references, relaxed SIMD) that Kalium deliberately does not support. The `simd/` and `multi-memory` scripts are not vendored for the same reason.

To refresh, re-download the same file list from the same tag; do not edit the files by hand.
