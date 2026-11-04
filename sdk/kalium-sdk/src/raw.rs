//! The host ABI exactly as the kernel exports it: one `extern "C"` block
//! per import namespace, with the signatures from the ABI tables in
//! `docs/prototype-plan.md`.
//!
//! Pointers and lengths are `i32` offsets into this instance's linear
//! memory (the kernel bounds-checks every one and returns `EFAULT`);
//! capabilities are `i32` slot indices; every fallible call returns an
//! `i32` errno. The safe wrappers in the rest of the crate are the intended
//! API; this module is public for services that need something the
//! wrappers do not express.
//!
//! The Rust identifier of every function *is* the Wasm import name, and the
//! module strings are literals (attributes cannot name constants), so each
//! block is followed by a compile-time check that the literal and every
//! identifier are what `kalium-abi` says they are. A typo here is a build
//! error, not a link error at boot.
//!
//! On non-Wasm targets (host unit tests of the SDK) the functions are stub
//! definitions with the same signatures that fail with `EFAULT`, so the
//! pure parts of the crate can be tested without a kernel.
#![allow(unsafe_code)]

use kalium_abi::imports;

/// `const`-evaluable string equality.
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// `const`-evaluable "is `name` one of `list`".
const fn listed(list: &[&str], name: &str) -> bool {
    let mut i = 0;
    while i < list.len() {
        if str_eq(list[i], name) {
            return true;
        }
        i += 1;
    }
    false
}

/// Declares one import namespace and checks it against `kalium-abi`.
macro_rules! namespace {
    (
        $module:literal => $abi:ident;
        $( $(#[$meta:meta])* fn $name:ident ( $( $arg:ident : $ty:ty ),* ) $( -> $ret:ty )? ; )*
    ) => {
        #[cfg(target_arch = "wasm32")]
        #[link(wasm_import_module = $module)]
        extern "C" {
            $( $(#[$meta])* pub fn $name ( $( $arg : $ty ),* ) $( -> $ret )? ; )*
        }

        // Host stubs: every call fails with EFAULT (or returns a harmless
        // default). `unsafe` so that call sites are identical on both sides.
        $(
            #[cfg(not(target_arch = "wasm32"))]
            #[allow(unused_variables, clippy::missing_safety_doc, clippy::too_many_arguments)]
            $(#[$meta])*
            pub unsafe fn $name ( $( $arg : $ty ),* ) $( -> $ret )? {
                namespace!(@stub $( $ret )?)
            }
        )*

        const _: () = {
            assert!(str_eq($module, imports::$abi::MODULE), "import module name differs from kalium-abi");
            $( assert!(listed(imports::$abi::FUNCTIONS, stringify!($name)), "import name is not in kalium-abi"); )*
            // Every function the ABI defines for this namespace is bound.
            assert!([$( stringify!($name) ),*].len() == imports::$abi::FUNCTIONS.len(), "namespace is incomplete");
        };
    };
    (@stub) => { () };
    (@stub $ret:ty) => { <$ret as StubReturn>::VALUE };
}

/// What a host stub returns on non-Wasm targets.
#[cfg(not(target_arch = "wasm32"))]
trait StubReturn {
    const VALUE: Self;
}
#[cfg(not(target_arch = "wasm32"))]
impl StubReturn for i32 {
    /// Every fallible call fails with `EFAULT` on the host.
    const VALUE: i32 = kalium_abi::errno::EFAULT;
}
#[cfg(not(target_arch = "wasm32"))]
impl StubReturn for i64 {
    const VALUE: i64 = 0;
}

namespace! {
    "kalium:core" => core;
    /// Needs `Console` kind with `WRITE`. Writes bytes to the UART.
    fn console_write(cap: i32, ptr: i32, len: i32) -> i32;
    /// Removes the slot.
    fn cap_drop(cap: i32) -> i32;
    /// Needs `DERIVE`. New cap has `rights & old.rights` and never gains
    /// `DERIVE` unless the old had it. Writes the new index as `i32` at
    /// `out_cap`.
    fn cap_derive(cap: i32, rights: i64, out_cap: i32) -> i32;
    /// Needs a live, unrevoked slot. Invalidates every capability derived
    /// from it, wherever it went; the slot itself stays valid.
    fn cap_revoke(cap: i32) -> i32;
    /// Writes `{kind: u32, rights: u64}` (12 bytes, little-endian) at `out`.
    fn cap_info(cap: i32, out: i32) -> i32;
    /// Nanoseconds since boot, monotonic.
    fn time_now() -> i64;
    /// Gives up the rest of the time slice.
    fn yield_();
    /// Blocks for at least `ns`.
    fn sleep(ns: i64);
    /// Terminates the instance. Does not return.
    fn exit(code: i32);
    /// The calling instance's id (for logs).
    fn instance_id() -> i32;
}

namespace! {
    "kalium:ipc" => ipc;
    /// Writes two cap indices: `SEND|DERIVE|TRANSFER` at `out_send`,
    /// `RECV|WAIT|DERIVE|TRANSFER` at `out_recv`.
    fn channel_create(out_send: i32, out_recv: i32) -> i32;
    /// Needs `SEND`. `caps` points to `ncaps` `i32` indices (each needs
    /// `TRANSFER`; moved out of this table on success). `EAGAIN` if full,
    /// `ECLOSED` if no receiver cap exists any more.
    fn send(cap: i32, msg: i32, len: i32, caps: i32, ncaps: i32) -> i32;
    /// Needs `RECV`. Blocks until a message is available; copies at most
    /// `buf_len` bytes; writes received cap indices at `caps` and
    /// `{len: u32, ncaps: u32}` at `out_hdr`.
    fn recv(cap: i32, buf: i32, buf_len: i32, caps: i32, ncaps_max: i32, out_hdr: i32) -> i32;
    /// Like `recv` but `EAGAIN` instead of blocking.
    fn try_recv(cap: i32, buf: i32, buf_len: i32, caps: i32, ncaps_max: i32, out_hdr: i32) -> i32;
    /// Blocks until one of `n` receive caps (all need `WAIT`) has a
    /// message; writes its 0-based position at `out_index`.
    fn wait_any(caps: i32, n: i32, out_index: i32) -> i32;
}

namespace! {
    "kalium:memory" => memory;
    /// Zeroed memory object, cap with `READ|WRITE|DERIVE|TRANSFER`.
    fn memobj_create(size: i64, out_cap: i32) -> i32;
    /// Writes the size as `i64` at `out_size`.
    fn memobj_size(cap: i32, out_size: i32) -> i32;
    /// Needs `READ`.
    fn memobj_read(cap: i32, offset: i64, ptr: i32, len: i32) -> i32;
    /// Needs `WRITE`.
    fn memobj_write(cap: i32, offset: i64, ptr: i32, len: i32) -> i32;
}

namespace! {
    "kalium:spawn" => spawn;
    /// Needs `Spawn` kind with `SPAWN`. Moves `ncaps` caps (each needs
    /// `TRANSFER`) to the child as its slots `0..ncaps`. `class` is `0`
    /// fair, `1` real-time. Writes an `Instance` cap index at `out_cap`.
    fn spawn(cap: i32, module: i32, caps: i32, ncaps: i32, class: i32, period_ns: i64, budget_ns: i64, out_cap: i32) -> i32;
    /// Needs `WAIT`. Blocks until the instance exits; writes the exit code
    /// (`i32`; traps `-1`, kill `-2`) at `out_code`.
    fn instance_wait(cap: i32, out_code: i32) -> i32;
    /// Needs `KILL`.
    fn instance_kill(cap: i32) -> i32;
    /// Number of modules in the boot image.
    fn module_count() -> i32;
    /// Needs `Instance` kind with `SNAPSHOT`; the instance must be parked
    /// (blocked or sleeping; `EBUSY` otherwise, `ECLOSED` once exited).
    /// Writes a `Snapshot` cap index (`READ|DERIVE|TRANSFER`) at `out_cap`.
    fn instance_snapshot(cap: i32, out_cap: i32) -> i32;
    /// Like `spawn`, from the state in `snapshot_cap` (needs `Snapshot`
    /// kind with `READ`), starting at `entry` (`0` `_start`, `1` `resume`;
    /// `ENOENT` if that export is missing or mistyped).
    fn spawn_snapshot(cap: i32, snapshot_cap: i32, caps: i32, ncaps: i32, class: i32, period_ns: i64, budget_ns: i64, entry: i32, out_cap: i32) -> i32;
}

/// The linear-memory offset of `p`, as the ABI wants it.
///
/// On wasm32 a pointer *is* its 32-bit offset. On the host (stubs only) the
/// value is meaningless but never dereferenced.
#[inline(always)]
pub fn addr<T>(p: *const T) -> i32 {
    p as usize as i32
}

/// Same as [`addr`] for out-parameters.
#[inline(always)]
pub fn addr_mut<T>(p: *mut T) -> i32 {
    p as usize as i32
}
