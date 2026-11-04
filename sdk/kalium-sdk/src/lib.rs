//! Kalium SDK: safe wrappers over the kernel's host ABI for services
//! compiled to `wasm32-unknown-unknown`.
//!
//! A service is a `cdylib` crate that exports `_start` and takes its initial
//! capabilities from slots `0..n` of its capability table, in the order its
//! spawner passed them:
//!
//! ```ignore
//! #![no_std]
//! use kalium_sdk::{Cap, Console};
//!
//! #[no_mangle]
//! pub extern "C" fn _start() {
//!     let console = Console::from_cap(Cap::slot(0));
//!     console.set_global();
//!     kalium_sdk::println!("hello from {}", "a service");
//!     kalium_sdk::exit(0);
//! }
//! ```
//!
//! The crate has no heap, no `std` and no dependencies besides
//! [`kalium_abi`] (re-exported as [`abi`]), whose constants define every
//! name, number and layout used here. The raw imports live in [`raw`]; the
//! only `unsafe` in the crate is the calls into them, each with a safety
//! comment explaining why the pointers and lengths handed to the kernel are
//! valid. Nothing here panics on data that comes from the kernel or from a
//! peer: every fallible operation returns [`Errno`].
//!
//! Printing: [`Console`] writes bytes; the [`print!`] / [`println!`] macros
//! format through `core::fmt` into a stack buffer and write to the global
//! console set with [`Console::set_global`]. For the smallest binaries, the
//! [`text::Line`] builder formats strings and integers without `core::fmt`.
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(test)]
extern crate std;

pub use kalium_abi as abi;
pub use kalium_abi::{kind, rights};

pub mod console;
pub mod ipc;
pub mod memobj;
pub mod raw;
pub mod spawn;
pub mod text;

pub use console::Console;
pub use ipc::{Channel, Receiver, Sender};
pub use memobj::MemObj;
pub use spawn::{instance_kill, instance_wait, module_count, spawn, Class, Instance};
pub use text::Line;

use core::sync::atomic::{AtomicI32, Ordering};

/// An error returned by a host call: the ABI errno values, by name.
///
/// `0` (`OK`) is never an `Errno`; see [`Errno::check`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Errno {
    /// `EBADCAP`: index is not a live slot.
    BadCap,
    /// `EPERM`: slot exists but lacks the right, or is the wrong kind.
    Perm,
    /// `EINVAL`: bad argument.
    Inval,
    /// `ENOMEM`: out of memory or over a quota.
    NoMem,
    /// `EAGAIN`: would block / queue full.
    Again,
    /// `ECLOSED`: peer gone.
    Closed,
    /// `ETOOBIG`: message or object too large.
    TooBig,
    /// `ENoEnt`: no such module / unresolved import.
    NoEnt,
    /// `EFAULT`: pointer/length outside linear memory.
    Fault,
    /// `EBUSY`: resource in use.
    Busy,
    /// A value the ABI does not define (a newer kernel, or a bug).
    Other(i32),
}

impl Errno {
    /// Turns a raw errno into a `Result`: `0` is `Ok(())`.
    pub const fn check(code: i32) -> Result<(), Errno> {
        match Errno::from_code(code) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The `Errno` for a non-zero code; `None` for `0` (`OK`).
    pub const fn from_code(code: i32) -> Option<Errno> {
        use kalium_abi::errno as e;
        Some(match code {
            e::OK => return None,
            e::EBADCAP => Errno::BadCap,
            e::EPERM => Errno::Perm,
            e::EINVAL => Errno::Inval,
            e::ENOMEM => Errno::NoMem,
            e::EAGAIN => Errno::Again,
            e::ECLOSED => Errno::Closed,
            e::ETOOBIG => Errno::TooBig,
            e::ENOENT => Errno::NoEnt,
            e::EFAULT => Errno::Fault,
            e::EBUSY => Errno::Busy,
            other => Errno::Other(other),
        })
    }

    /// The ABI value.
    pub const fn code(self) -> i32 {
        use kalium_abi::errno as e;
        match self {
            Errno::BadCap => e::EBADCAP,
            Errno::Perm => e::EPERM,
            Errno::Inval => e::EINVAL,
            Errno::NoMem => e::ENOMEM,
            Errno::Again => e::EAGAIN,
            Errno::Closed => e::ECLOSED,
            Errno::TooBig => e::ETOOBIG,
            Errno::NoEnt => e::ENOENT,
            Errno::Fault => e::EFAULT,
            Errno::Busy => e::EBUSY,
            Errno::Other(code) => code,
        }
    }

    /// The ABI name (`"EBADCAP"`, ...), or `"E?"` for a value the ABI does
    /// not define.
    pub const fn name(self) -> &'static str {
        match kalium_abi::errno::name(self.code()) {
            Some(name) => name,
            None => "E?",
        }
    }
}

/// A capability: an index into this instance's capability table.
///
/// Plain data: copying a `Cap` does not duplicate the slot, and dropping
/// one does not remove it (use [`cap_drop`]). After a `Cap` is transferred
/// with [`Sender::send`] or [`spawn`] the slot is gone and the value is
/// stale.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap(pub i32);

impl Cap {
    /// The "no capability" value, `-1`, which the kernel never accepts.
    pub const NONE: Cap = Cap(kalium_abi::CAP_NONE);

    /// The capability in slot `n`. Initial capabilities are slots `0..n` in
    /// the order the spawner passed them.
    pub const fn slot(n: u32) -> Cap {
        Cap(n as i32)
    }

    /// The raw index.
    pub const fn raw(self) -> i32 {
        self.0
    }

    /// Whether this is [`Cap::NONE`] (or any other value the kernel
    /// rejects outright).
    pub const fn is_none(self) -> bool {
        self.0 < 0
    }

    /// See [`cap_derive`].
    pub fn derive(self, rights: u64) -> Result<Cap, Errno> {
        cap_derive(self, rights)
    }

    /// See [`cap_info`].
    pub fn info(self) -> Result<CapInfo, Errno> {
        cap_info(self)
    }
}

/// What [`cap_info`] reports about a capability.
pub type CapInfo = kalium_abi::layout::CapInfo;

/// Removes the slot (`cap_drop`). The object lives on while other
/// capabilities refer to it.
pub fn cap_drop(cap: Cap) -> Result<(), Errno> {
    // SAFETY: no pointers are passed.
    Errno::check(unsafe { raw::cap_drop(cap.0) })
}

/// Mints a new capability on the same object with `rights & old.rights`
/// (`cap_derive`); needs `DERIVE`. The new cap never gains `DERIVE` unless
/// the old one had it. The rights bits are in [`rights`].
pub fn cap_derive(cap: Cap, rights: u64) -> Result<Cap, Errno> {
    let mut out: i32 = Cap::NONE.0;
    // SAFETY: `out` is a live, writable `i32` for the duration of the call;
    // the kernel writes exactly 4 bytes at its offset.
    let r = unsafe { raw::cap_derive(cap.0, rights as i64, raw::addr_mut(&mut out)) };
    Errno::check(r)?;
    if out < 0 {
        return Err(Errno::Other(r));
    }
    Ok(Cap(out))
}

/// The kind and rights of a capability (`cap_info`).
pub fn cap_info(cap: Cap) -> Result<CapInfo, Errno> {
    let mut out = [0u8; kalium_abi::layout::CAP_INFO_SIZE];
    // SAFETY: `out` is a live 12-byte buffer, the exact wire size the
    // kernel writes.
    let r = unsafe { raw::cap_info(cap.0, raw::addr_mut(out.as_mut_ptr())) };
    Errno::check(r)?;
    Ok(CapInfo::from_le_bytes(out))
}

/// Nanoseconds since boot, monotonic (`time_now`).
pub fn time_now() -> i64 {
    // SAFETY: no pointers are passed.
    unsafe { raw::time_now() }
}

/// Gives up the rest of the time slice (`yield_`).
pub fn yield_now() {
    // SAFETY: no pointers are passed.
    unsafe { raw::yield_() }
}

/// Blocks for at least `ns` nanoseconds (`sleep`). Non-positive values
/// return at once (after giving the kernel a chance to reschedule).
pub fn sleep(ns: i64) {
    // SAFETY: no pointers are passed.
    unsafe { raw::sleep(ns) }
}

/// Terminates the instance with `code` (`exit`). Never returns.
///
/// By convention `0` is success; the kernel reports `-1` for a trap and
/// `-2` for a kill, and the SDK's panic handler exits with `101`.
pub fn exit(code: i32) -> ! {
    loop {
        // SAFETY: no pointers are passed.
        unsafe { raw::exit(code) }
    }
}

/// The calling instance's id, for logs (`instance_id`).
pub fn instance_id() -> i32 {
    // SAFETY: no pointers are passed.
    unsafe { raw::instance_id() }
}

/// The console the [`print!`] macros and the panic handler write to.
static GLOBAL_CONSOLE: AtomicI32 = AtomicI32::new(kalium_abi::CAP_NONE);

pub(crate) fn global_console_cap() -> Option<Cap> {
    let cap = Cap(GLOBAL_CONSOLE.load(Ordering::Relaxed));
    if cap.is_none() {
        None
    } else {
        Some(cap)
    }
}

pub(crate) fn set_global_console_cap(cap: Cap) {
    GLOBAL_CONSOLE.store(cap.0, Ordering::Relaxed);
}

/// The panic handler: writes the message and location to the global
/// console, if one is set, then exits with code `101`.
///
/// Services that never panic (no `unwrap`, no indexing that can fail) do
/// not link any of this; the linker drops it with `core::fmt`.
#[cfg(all(target_arch = "wasm32", not(test)))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    use core::fmt::Write;
    if let Some(console) = Console::global() {
        let mut w = console::Writer::new(console);
        let _ = write!(w, "panic: {}", info.message());
        if let Some(loc) = info.location() {
            let _ = write!(w, " at {}:{}:{}", loc.file(), loc.line(), loc.column());
        }
        let _ = w.write_str("\n");
        let _ = w.flush();
    }
    exit(kalium_abi::exit::PANIC)
}

/// Clamps a slice length to what fits in an ABI `i32` length argument.
pub(crate) fn abi_len(len: usize) -> Result<i32, Errno> {
    i32::try_from(len).map_err(|_| Errno::TooBig)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_round_trips_every_abi_value() {
        assert_eq!(Errno::check(0), Ok(()));
        for &(code, name) in kalium_abi::errno::ALL {
            if code == 0 {
                assert_eq!(Errno::from_code(code), None);
                continue;
            }
            let e = Errno::from_code(code).expect("non-zero errno");
            assert_eq!(e.code(), code);
            assert_eq!(e.name(), name);
            assert_eq!(Errno::check(code), Err(e));
            assert!(!matches!(e, Errno::Other(_)), "{name} should be named");
        }
        assert_eq!(Errno::from_code(99), Some(Errno::Other(99)));
        assert_eq!(Errno::Other(99).code(), 99);
        assert_eq!(Errno::Other(99).name(), "E?");
        assert_eq!(Errno::from_code(-3), Some(Errno::Other(-3)));
    }

    #[test]
    fn cap_slots_and_none() {
        assert_eq!(Cap::slot(0), Cap(0));
        assert_eq!(Cap::slot(7).raw(), 7);
        assert!(Cap::NONE.is_none());
        assert_eq!(Cap::NONE.raw(), -1);
        assert!(!Cap::slot(0).is_none());
        assert_eq!(core::mem::size_of::<Cap>(), 4);
    }

    #[test]
    fn host_stubs_fail_cleanly() {
        // On the host every raw call reports EFAULT; the wrappers must map
        // that without panicking.
        assert_eq!(cap_drop(Cap::slot(0)), Err(Errno::Fault));
        assert_eq!(cap_derive(Cap::slot(0), rights::WRITE), Err(Errno::Fault));
        assert_eq!(cap_info(Cap::slot(0)), Err(Errno::Fault));
        assert_eq!(time_now(), 0);
        // Infallible `i32` imports return the stub's EFAULT marker too.
        let _ = instance_id();
        yield_now();
        sleep(1);
    }

    #[test]
    fn abi_len_rejects_oversize() {
        assert_eq!(abi_len(0), Ok(0));
        assert_eq!(abi_len(4096), Ok(4096));
        assert_eq!(abi_len(i32::MAX as usize), Ok(i32::MAX));
        assert_eq!(abi_len(i32::MAX as usize + 1), Err(Errno::TooBig));
    }
}
