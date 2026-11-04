//! WASI preview1 compatibility: the `wasi_snapshot_preview1` import module,
//! so that ordinary programs built for `wasm32-wasip1` (Rust `std`, C
//! through wasi-libc) run on Kalium unchanged. The contract is the "WASI
//! preview1 compatibility" section of `docs/prototype-plan.md`; this module
//! documents the design and holds the whole implementation.
//!
//! # Linking
//!
//! `wasi_snapshot_preview1` resolves for every instance, whatever
//! capabilities it holds: a WASI program has no way to ask for less, so the
//! *imports* always link and the *calls* are checked against the
//! instance's capabilities (a `fd_write` without a console is `EBADF`, not
//! a link error). [`TABLE`] declares every function of the snapshot with its
//! core-Wasm signature; an import whose name is not in the table, or whose
//! type differs, is a link error like any other unresolved import. The
//! functions that have no meaning here return `ENOSYS`, `ENOTSUP` or `EBADF`
//! (see "Policy" below) so that a program linking them still instantiates.
//!
//! Host function ids: the WASI functions occupy `BASE + index in TABLE`
//! ([`BASE`] is far above the `kalium:*` ids, which are positions in
//! `kalium_abi::imports::ALL_IMPORTS`); [`is_wasi`] tells the dispatcher in
//! `host.rs` which range an id belongs to.
//!
//! # File descriptors are capabilities
//!
//! Kalium has no file descriptors; the WASI layer maps the three standard
//! streams onto the capability slots a spawner conventionally fills:
//!
//! | fd | Backed by | Without the capability |
//! | --- | --- | --- |
//! | 0 (stdin) | the `Channel` capability with `RECV` in slot 1, if any: each message is a chunk of input | `fd_read` is `EBADF` |
//! | 1, 2 (stdout, stderr) | the `Console` capability with `WRITE` in slot 0, if any | `fd_write` is `EBADF` and the bytes are dropped |
//!
//! Nothing else is a valid fd: there are no preopened directories
//! (`fd_prestat_get` is `EBADF` for every fd, which is how wasi-libc and
//! Rust's `std` learn that there is nothing to open), no files, no sockets.
//! `fd_fdstat_get` describes fds 0..2 as character devices (so C stdio is
//! line buffered as on a terminal) with `FD_READ` on 0 and `FD_WRITE` on 1
//! and 2 when the backing capability is present. `fd_seek` and `fd_tell` on
//! them are `ESPIPE`; `fd_close` is accepted and does nothing.
//!
//! `fd_read` on stdin takes the next message from the channel (blocking
//! through the kernel's wait queue like `recv`, so the interpreter rewinds
//! and re-issues the call when a message arrives), keeps whatever does not
//! fit the caller's buffers for the next `fd_read`, and reports 0 bytes
//! (end of file) when the channel has no sender left or a message is empty.
//! The iovecs are the caller's `readv` vector, with POSIX semantics: an
//! empty iovec among others is skipped, and a call with no room at all (no
//! iovecs, or only empty ones) returns 0 at once without taking a message
//! from the channel or blocking, as `read(fd, buf, 0)` must. Capabilities
//! transferred in a message to stdin are dropped: a byte stream has no
//! place for them.
//!
//! # Process, environment, time, randomness, scheduling
//!
//! - `proc_exit(code)` is the instance's `exit(code)`: the spawner sees
//!   `code` through `instance_wait`. A `_start` that returns is exit 0, as
//!   for SDK services.
//! - `args_*` and `environ_*` report no arguments and no variables: the
//!   kernel has no notion of a command line, and `spawn` passes
//!   capabilities, not strings. (Both are bounds-checked and write the two
//!   zero counts.)
//! - `clock_time_get`: `REALTIME` (0) and `MONOTONIC` (1) both return the
//!   HAL's nanosecond counter since boot (there is no real-time clock on the
//!   `virt` board yet, so `time()` in C counts from boot, not from 1970);
//!   the CPU-time clocks (2, 3) are `ENOTSUP`, other ids `EINVAL`.
//!   `clock_res_get` reports the counter's period.
//! - `random_get` fills the buffer from a per-instance xorshift64* generator
//!   seeded from the timer counter and the instance id on first use. **It is
//!   not cryptographic**: it is what Rust's `HashMap` needs for its hash
//!   seed, and nothing more. A hardware or CSPRNG source is a later
//!   milestone and the plan says so.
//! - `sched_yield` is `yield_`; `poll_oneoff` with clock subscriptions only
//!   (what `std::thread::sleep` and `nanosleep` issue) sleeps until the
//!   earliest deadline through the scheduler's sleep mechanism and reports
//!   every subscription whose deadline has passed; `fd_read`/`fd_write`
//!   subscriptions are `ENOTSUP`, and so the whole call is, as a WASI
//!   runtime without non-blocking fds is allowed to do.
//!
//! # Policy for the rest of the snapshot
//!
//! | Functions | Result |
//! | --- | --- |
//! | `fd_*` on an fd other than 0, 1, 2 | `EBADF` |
//! | other `fd_*` on 0, 1, 2 (`fd_advise`, `fd_allocate`, `fd_datasync`, `fd_fdstat_set_*`, `fd_filestat_*`, `fd_pread`, `fd_pwrite`, `fd_readdir`, `fd_renumber`, `fd_sync`) | `ENOTSUP` |
//! | `path_*` (no file system) and `sock_*` (no sockets) | `ENOTSUP` |
//! | `proc_raise` | `ENOSYS` |
//!
//! # Memory safety
//!
//! Every pointer and length is bounds-checked against the calling instance's
//! linear memory before anything is read, written or output, with the
//! helpers shared with the `kalium:*` functions in `host.rs`; a failure is
//! WASI `EFAULT`. iovec arrays are checked as a whole and then each buffer
//! they name; a negative count is `EINVAL`. Errno values are the WASI
//! preview1 ones (`wasi/api.h`), not Kalium's.

use alloc::vec::Vec;

use kalium_abi::wasi as abi;
use kalium_cap::{ObjectKind, Rights};
use kalium_hal::{console, timer};
use kalium_ipc::IpcError;
use kalium_wasm::{FuncType, HostFuncId, HostOutcome, Memory, Trap, ValType, Value};

use crate::host::{self, Args, Effect, HostCall, Step};

/// The import module name, `wasi_snapshot_preview1`.
pub const MODULE: &str = abi::MODULE;

/// First host function id of the WASI range: `BASE + i` is `TABLE[i]`.
pub const BASE: u32 = 0x1000;

/// WASI preview1 errno values (`__WASI_ERRNO_*` in `wasi/api.h`), the ones
/// this layer returns. They are unrelated to `kalium_abi::errno`.
pub mod errno {
    pub const SUCCESS: i32 = 0;
    pub const BADF: i32 = 8;
    pub const FAULT: i32 = 21;
    pub const INVAL: i32 = 28;
    pub const NOSYS: i32 = 52;
    pub const NOTSUP: i32 = 58;
    pub const SPIPE: i32 = 70;
}

/// `__WASI_CLOCKID_*`.
mod clockid {
    pub const REALTIME: i32 = 0;
    pub const MONOTONIC: i32 = 1;
    pub const PROCESS_CPUTIME_ID: i32 = 2;
    pub const THREAD_CPUTIME_ID: i32 = 3;
}

/// `__WASI_FILETYPE_CHARACTER_DEVICE`.
const FILETYPE_CHARACTER_DEVICE: u8 = 2;
/// `__WASI_RIGHTS_FD_READ`, `__WASI_RIGHTS_FD_WRITE`.
const RIGHTS_FD_READ: u64 = 1 << 1;
const RIGHTS_FD_WRITE: u64 = 1 << 6;
/// `__WASI_EVENTTYPE_CLOCK`.
const EVENTTYPE_CLOCK: u8 = 0;
/// `__WASI_SUBCLOCKFLAGS_SUBSCRIPTION_CLOCK_ABSTIME`.
const SUBCLOCKFLAGS_ABSTIME: u16 = 1;

/// Wire sizes (`wasi/api.h`): `iovec`/`ciovec`, `fdstat`, `subscription`,
/// `event`.
const IOVEC_SIZE: usize = 8;
const FDSTAT_SIZE: usize = 24;
const SUBSCRIPTION_SIZE: usize = 48;
const EVENT_SIZE: usize = 32;

/// One function of the snapshot: its import name and core-Wasm type.
pub struct Sig {
    pub name: &'static str,
    pub params: &'static [ValType],
    pub results: &'static [ValType],
}

use ValType::{I32, I64};

const fn sig(name: &'static str, params: &'static [ValType]) -> Sig {
    Sig {
        name,
        params,
        results: &[I32],
    }
}

/// Every function of `wasi_snapshot_preview1` with its signature, in the
/// order of `wasi/api.h`. Pointers, lengths, fds, flags and small enums are
/// `i32`; 64-bit sizes, offsets, timestamps and rights are `i64`. Every
/// function returns an `i32` errno except `proc_exit`, which returns
/// nothing.
pub const TABLE: [Sig; 46] = [
    sig("args_get", &[I32, I32]),
    sig("args_sizes_get", &[I32, I32]),
    sig("environ_get", &[I32, I32]),
    sig("environ_sizes_get", &[I32, I32]),
    sig("clock_res_get", &[I32, I32]),
    sig("clock_time_get", &[I32, I64, I32]),
    sig("fd_advise", &[I32, I64, I64, I32]),
    sig("fd_allocate", &[I32, I64, I64]),
    sig("fd_close", &[I32]),
    sig("fd_datasync", &[I32]),
    sig("fd_fdstat_get", &[I32, I32]),
    sig("fd_fdstat_set_flags", &[I32, I32]),
    sig("fd_fdstat_set_rights", &[I32, I64, I64]),
    sig("fd_filestat_get", &[I32, I32]),
    sig("fd_filestat_set_size", &[I32, I64]),
    sig("fd_filestat_set_times", &[I32, I64, I64, I32]),
    sig("fd_pread", &[I32, I32, I32, I64, I32]),
    sig("fd_prestat_get", &[I32, I32]),
    sig("fd_prestat_dir_name", &[I32, I32, I32]),
    sig("fd_pwrite", &[I32, I32, I32, I64, I32]),
    sig("fd_read", &[I32, I32, I32, I32]),
    sig("fd_readdir", &[I32, I32, I32, I64, I32]),
    sig("fd_renumber", &[I32, I32]),
    sig("fd_seek", &[I32, I64, I32, I32]),
    sig("fd_sync", &[I32]),
    sig("fd_tell", &[I32, I32]),
    sig("fd_write", &[I32, I32, I32, I32]),
    sig("path_create_directory", &[I32, I32, I32]),
    sig("path_filestat_get", &[I32, I32, I32, I32, I32]),
    sig(
        "path_filestat_set_times",
        &[I32, I32, I32, I32, I64, I64, I32],
    ),
    sig("path_link", &[I32, I32, I32, I32, I32, I32, I32]),
    sig("path_open", &[I32, I32, I32, I32, I32, I64, I64, I32, I32]),
    sig("path_readlink", &[I32, I32, I32, I32, I32, I32]),
    sig("path_remove_directory", &[I32, I32, I32]),
    sig("path_rename", &[I32, I32, I32, I32, I32, I32]),
    sig("path_symlink", &[I32, I32, I32, I32, I32]),
    sig("path_unlink_file", &[I32, I32, I32]),
    sig("poll_oneoff", &[I32, I32, I32, I32]),
    Sig {
        name: "proc_exit",
        params: &[I32],
        results: &[],
    },
    sig("proc_raise", &[I32]),
    sig("sched_yield", &[]),
    sig("random_get", &[I32, I32]),
    sig("sock_accept", &[I32, I32, I32]),
    sig("sock_recv", &[I32, I32, I32, I32, I32, I32]),
    sig("sock_send", &[I32, I32, I32, I32, I32]),
    sig("sock_shutdown", &[I32, I32]),
];

/// Whether `id` is a WASI function (as opposed to a `kalium:*` one).
pub fn is_wasi(id: HostFuncId) -> bool {
    id.0 >= BASE && ((id.0 - BASE) as usize) < TABLE.len()
}

/// Import resolver for `wasi_snapshot_preview1`: the name must be in
/// [`TABLE`] with exactly its signature.
pub fn resolve(name: &str, ty: &FuncType) -> Result<HostFuncId, ()> {
    let i = TABLE.iter().position(|s| s.name == name).ok_or(())?;
    let s = &TABLE[i];
    if ty.params != s.params || ty.results != s.results {
        return Err(());
    }
    Ok(HostFuncId(BASE + i as u32))
}

/// Checks the table at boot: no duplicate names, `proc_exit` the only
/// function without a result.
pub fn check_table() -> Result<(), &'static str> {
    for (i, s) in TABLE.iter().enumerate() {
        if TABLE[..i].iter().any(|o| o.name == s.name) {
            return Err("duplicate WASI function name");
        }
        if (s.results.is_empty()) != (s.name == "proc_exit") {
            return Err("WASI function with a wrong result type");
        }
        if resolve(
            s.name,
            &FuncType::new(s.params.to_vec(), s.results.to_vec()),
        ) != Ok(HostFuncId(BASE + i as u32))
        {
            return Err("WASI function does not resolve to its own id");
        }
    }
    Ok(())
}

/// Per-instance WASI state.
#[derive(Default)]
pub struct State {
    /// xorshift64* state; `0` until the first `random_get` seeds it.
    rng: u64,
    /// The stdin message being consumed and how far it has been read.
    stdin: Vec<u8>,
    stdin_pos: usize,
    /// `poll_oneoff` in progress: the time the first issue of the call
    /// computed its deadlines from, so that the re-issued call computes the
    /// same ones.
    poll_started: Option<u64>,
}

type R = Result<Step, i32>;

const OK: R = Ok(Step::Ret(errno::SUCCESS));

/// Maps a `kalium_abi::errno` from the shared bounds-checking helpers to
/// the WASI value: `EINVAL` stays a bad argument, everything else is a bad
/// pointer.
fn map_fault(e: i32) -> i32 {
    if e == kalium_abi::errno::EINVAL {
        errno::INVAL
    } else {
        errno::FAULT
    }
}

fn range(mem: &Memory, ptr: i32, len: i32) -> Result<(u32, usize), i32> {
    host::range(mem, ptr, len).map_err(map_fault)
}

fn range_n(mem: &Memory, ptr: i32, count: i32, size: usize) -> Result<(u32, usize), i32> {
    host::range_n(mem, ptr, count, size).map_err(map_fault)
}

fn write(mem: &mut Memory, ptr: u32, bytes: &[u8]) -> Result<(), i32> {
    host::write_bytes(mem, ptr, bytes).map_err(map_fault)
}

fn write_u32(mem: &mut Memory, ptr: u32, v: u32) -> Result<(), i32> {
    write(mem, ptr, &v.to_le_bytes())
}

fn write_u64(mem: &mut Memory, ptr: u32, v: u64) -> Result<(), i32> {
    write(mem, ptr, &v.to_le_bytes())
}

fn read_u32(mem: &Memory, ptr: u32) -> Result<u32, i32> {
    let b = mem.slice(ptr, 4).map_err(|_| errno::FAULT)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_u64(mem: &Memory, ptr: u32) -> Result<u64, i32> {
    let b = mem.slice(ptr, 8).map_err(|_| errno::FAULT)?;
    Ok(u64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

/// The buffers named by an iovec array, each bounds-checked.
fn iovecs(mem: &Memory, iovs: i32, iovs_len: i32) -> Result<Vec<(u32, usize)>, i32> {
    let (p, _) = range_n(mem, iovs, iovs_len, IOVEC_SIZE)?;
    let mut v = Vec::with_capacity(iovs_len as usize);
    for i in 0..iovs_len as u32 {
        let at = p
            .checked_add(i.checked_mul(IOVEC_SIZE as u32).ok_or(errno::FAULT)?)
            .ok_or(errno::FAULT)?;
        let buf = read_u32(mem, at)?;
        let len = read_u32(mem, at + 4)?;
        let len = i32::try_from(len).map_err(|_| errno::FAULT)?;
        v.push(range(mem, buf as i32, len)?);
    }
    Ok(v)
}

/// `args_sizes_get` / `environ_sizes_get`: zero entries, zero bytes.
fn sizes_get(mem: &mut Memory, count: i32, size: i32) -> R {
    let (cp, _) = range(mem, count, 4)?;
    let (bp, _) = range(mem, size, 4)?;
    write_u32(mem, cp, 0)?;
    write_u32(mem, bp, 0)?;
    OK
}

/// `args_get` / `environ_get`: zero entries, so the arrays are never
/// touched (a zero-length range is accepted whatever its pointer).
fn list_get(mem: &mut Memory, ptrs: i32, buf: i32) -> R {
    range(mem, ptrs, 0)?;
    range(mem, buf, 0)?;
    OK
}

/// Whether `fd` is one of the standard streams.
fn is_std(fd: i32) -> bool {
    (0..=2).contains(&fd)
}

impl HostCall<'_> {
    fn wasi(&mut self) -> &mut State {
        &mut self.k.instances[self.cur].wasi
    }

    /// The console object behind fds 1 and 2, if the instance has one.
    fn stdout_console(&mut self) -> Option<kalium_cap::ObjectId> {
        let idx = host::cap_index(abi::STDOUT_CONSOLE_SLOT).ok()?;
        self.caps()
            .check(idx, ObjectKind::Console, Rights::WRITE)
            .ok()
            .map(|c| c.object)
    }

    /// The channel behind fd 0, if the instance has one.
    fn stdin_channel(&mut self) -> Option<kalium_cap::ObjectId> {
        let idx = host::cap_index(abi::STDIN_CHANNEL_SLOT).ok()?;
        self.caps()
            .check(idx, ObjectKind::Channel, Rights::RECV)
            .ok()
            .map(|c| c.object)
    }

    fn fd_write(
        &mut self,
        mem: &mut Memory,
        fd: i32,
        iovs: i32,
        iovs_len: i32,
        nwritten: i32,
    ) -> R {
        let (np, _) = range(mem, nwritten, 4)?;
        let bufs = iovecs(mem, iovs, iovs_len)?;
        if !is_std(fd) {
            return Err(errno::BADF);
        }
        if fd == 0 || self.stdout_console().is_none() {
            return Err(errno::BADF);
        }
        let mut total: u32 = 0;
        for (p, l) in bufs {
            if l > 0 {
                let bytes = mem.slice(p, l).map_err(|_| errno::FAULT)?;
                console::with_console(|_| console::write_bytes(bytes));
            }
            total = total.saturating_add(l as u32);
        }
        write_u32(mem, np, total)?;
        OK
    }

    fn fd_read(&mut self, mem: &mut Memory, fd: i32, iovs: i32, iovs_len: i32, nread: i32) -> R {
        let (np, _) = range(mem, nread, 4)?;
        let bufs = iovecs(mem, iovs, iovs_len)?;
        if !is_std(fd) {
            return Err(errno::BADF);
        }
        let chan = match (fd, self.stdin_channel()) {
            (0, Some(chan)) => chan,
            _ => return Err(errno::BADF),
        };
        // No room anywhere (no iovecs, or only empty ones): 0 bytes at
        // once, as for `read(fd, buf, 0)`; the channel is not touched, so
        // nothing is taken from it and nothing blocks.
        if bufs.iter().all(|&(_, l)| l == 0) {
            write_u32(mem, np, 0)?;
            return OK;
        }
        if self.wasi().stdin_pos >= self.wasi().stdin.len() {
            // Nothing buffered: take the next message.
            match self.k.channel_mut(chan).ok_or(errno::BADF)?.try_recv() {
                Ok(msg) => {
                    let now = timer::now_ns();
                    for cap in msg.caps {
                        self.k.release(cap, now);
                    }
                    let st = self.wasi();
                    st.stdin = msg.bytes;
                    st.stdin_pos = 0;
                }
                Err(IpcError::Empty) => return Ok(self.block_on(&[chan])),
                // Closed: end of file.
                Err(_) => {
                    write_u32(mem, np, 0)?;
                    return OK;
                }
            }
        }
        let st = &mut self.k.instances[self.cur].wasi;
        let mut total: usize = 0;
        for (p, l) in bufs {
            let avail = st.stdin.len() - st.stdin_pos;
            if avail == 0 {
                break;
            }
            let n = l.min(avail);
            if n == 0 {
                // An empty iovec among others is skipped, not end of file.
                continue;
            }
            write(mem, p, &st.stdin[st.stdin_pos..st.stdin_pos + n])?;
            st.stdin_pos += n;
            total += n;
        }
        if st.stdin_pos >= st.stdin.len() {
            st.stdin = Vec::new();
            st.stdin_pos = 0;
        }
        write_u32(mem, np, total as u32)?;
        OK
    }

    fn fd_fdstat_get(&mut self, mem: &mut Memory, fd: i32, out: i32) -> R {
        let (op, _) = range(mem, out, FDSTAT_SIZE as i32)?;
        if !is_std(fd) {
            return Err(errno::BADF);
        }
        let rights = match fd {
            0 if self.stdin_channel().is_some() => RIGHTS_FD_READ,
            1 | 2 if self.stdout_console().is_some() => RIGHTS_FD_WRITE,
            _ => 0,
        };
        let mut stat = [0u8; FDSTAT_SIZE];
        stat[0] = FILETYPE_CHARACTER_DEVICE;
        // fs_flags (u16 at 2) = 0; fs_rights_base at 8; fs_rights_inheriting at 16 = 0.
        stat[8..16].copy_from_slice(&rights.to_le_bytes());
        write(mem, op, &stat)?;
        OK
    }

    fn clock_time_get(&mut self, mem: &mut Memory, id: i32, out: i32) -> R {
        let (op, _) = range(mem, out, 8)?;
        match id {
            clockid::REALTIME | clockid::MONOTONIC => {
                write_u64(mem, op, timer::now_ns())?;
                OK
            }
            clockid::PROCESS_CPUTIME_ID | clockid::THREAD_CPUTIME_ID => Err(errno::NOTSUP),
            _ => Err(errno::INVAL),
        }
    }

    fn clock_res_get(&mut self, mem: &mut Memory, id: i32, out: i32) -> R {
        let (op, _) = range(mem, out, 8)?;
        match id {
            clockid::REALTIME | clockid::MONOTONIC => {
                let hz = timer::frequency_hz().max(1);
                write_u64(mem, op, (1_000_000_000 / hz).max(1))?;
                OK
            }
            clockid::PROCESS_CPUTIME_ID | clockid::THREAD_CPUTIME_ID => Err(errno::NOTSUP),
            _ => Err(errno::INVAL),
        }
    }

    fn random_get(&mut self, mem: &mut Memory, buf: i32, len: i32) -> R {
        let (p, l) = range(mem, buf, len)?;
        let cur = self.cur as u64;
        let st = self.wasi();
        if st.rng == 0 {
            // Not cryptographic; see the module docs.
            st.rng = timer::counter() ^ (cur << 56) ^ 0x9E37_79B9_7F4A_7C15;
            if st.rng == 0 {
                st.rng = 0x9E37_79B9_7F4A_7C15;
            }
        }
        if l > 0 {
            let dst = mem.slice_mut(p, l).map_err(|_| errno::FAULT)?;
            for chunk in dst.chunks_mut(8) {
                // xorshift64*
                let mut x = st.rng;
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                st.rng = x;
                let out = x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
                chunk.copy_from_slice(&out[..chunk.len()]);
            }
        }
        OK
    }

    fn poll_oneoff(&mut self, mem: &mut Memory, subs: i32, events: i32, n: i32, nevents: i32) -> R {
        if n <= 0 {
            return Err(errno::INVAL);
        }
        let (sp, _) = range_n(mem, subs, n, SUBSCRIPTION_SIZE)?;
        let (ep, _) = range_n(mem, events, n, EVENT_SIZE)?;
        let (np, _) = range(mem, nevents, 4)?;
        let now = timer::now_ns();
        let started = self.wasi().poll_started.unwrap_or(now);
        // (userdata, deadline) per subscription.
        let mut list: Vec<(u64, u64)> = Vec::with_capacity(n as usize);
        for i in 0..n as u32 {
            let at = sp + i * SUBSCRIPTION_SIZE as u32;
            let userdata = read_u64(mem, at)?;
            let tag = mem.slice(at + 8, 1).map_err(|_| errno::FAULT)?[0];
            if tag != EVENTTYPE_CLOCK {
                // fd_read/fd_write subscriptions: no non-blocking fds here.
                self.wasi().poll_started = None;
                return Err(errno::NOTSUP);
            }
            let id = read_u32(mem, at + 16)? as i32;
            if id != clockid::REALTIME && id != clockid::MONOTONIC {
                self.wasi().poll_started = None;
                return Err(errno::INVAL);
            }
            let timeout = read_u64(mem, at + 24)?;
            let flags = mem.slice(at + 40, 2).map_err(|_| errno::FAULT)?;
            let flags = u16::from_le_bytes([flags[0], flags[1]]);
            let deadline = if flags & SUBCLOCKFLAGS_ABSTIME != 0 {
                timeout
            } else {
                started.saturating_add(timeout)
            };
            list.push((userdata, deadline));
        }
        let earliest = list.iter().map(|&(_, d)| d).min().unwrap_or(now);
        if earliest > now {
            self.wasi().poll_started = Some(started);
            let tid = self.tid();
            self.k.sched.sleep(tid, earliest);
            self.effect = Effect::Slept;
            return Ok(Step::Block);
        }
        self.wasi().poll_started = None;
        let mut count: u32 = 0;
        for (userdata, deadline) in list {
            if deadline <= now {
                let mut ev = [0u8; EVENT_SIZE];
                ev[..8].copy_from_slice(&userdata.to_le_bytes());
                // error (u16 at 8) = 0, type (u8 at 10) = clock.
                ev[10] = EVENTTYPE_CLOCK;
                write(mem, ep + count * EVENT_SIZE as u32, &ev)?;
                count += 1;
            }
        }
        write_u32(mem, np, count)?;
        OK
    }
}

/// Dispatches WASI function `id` (`BASE + index`) for the calling instance.
pub fn call(
    h: &mut HostCall<'_>,
    mem: &mut Memory,
    id: HostFuncId,
    args: &[Value],
    results: &mut [Value],
) -> Result<HostOutcome, Trap> {
    let index = id.0.checked_sub(BASE).ok_or(Trap::Host)? as usize;
    let name = TABLE.get(index).ok_or(Trap::Host)?.name;
    let a = Args(args);
    let step: R = match name {
        "fd_write" => h.fd_write(mem, a.i32(0)?, a.i32(1)?, a.i32(2)?, a.i32(3)?),
        "fd_read" => h.fd_read(mem, a.i32(0)?, a.i32(1)?, a.i32(2)?, a.i32(3)?),
        "fd_close" => {
            if is_std(a.i32(0)?) {
                OK
            } else {
                Err(errno::BADF)
            }
        }
        "fd_fdstat_get" => h.fd_fdstat_get(mem, a.i32(0)?, a.i32(1)?),
        "fd_prestat_get" | "fd_prestat_dir_name" => Err(errno::BADF),
        "fd_seek" | "fd_tell" => {
            if is_std(a.i32(0)?) {
                Err(errno::SPIPE)
            } else {
                Err(errno::BADF)
            }
        }
        "proc_exit" => return Ok(HostOutcome::Exit(a.i32(0)?)),
        "args_sizes_get" | "environ_sizes_get" => sizes_get(mem, a.i32(0)?, a.i32(1)?),
        "args_get" | "environ_get" => list_get(mem, a.i32(0)?, a.i32(1)?),
        "clock_time_get" => h.clock_time_get(mem, a.i32(0)?, a.i32(2)?),
        "clock_res_get" => h.clock_res_get(mem, a.i32(0)?, a.i32(1)?),
        "random_get" => h.random_get(mem, a.i32(0)?, a.i32(1)?),
        "sched_yield" => match h.yield_() {
            HostOutcome::Continue => OK,
            other => return Ok(other),
        },
        "poll_oneoff" => h.poll_oneoff(mem, a.i32(0)?, a.i32(1)?, a.i32(2)?, a.i32(3)?),
        "proc_raise" => Err(errno::NOSYS),
        n if n.starts_with("fd_") => {
            if is_std(a.i32(0)?) {
                Err(errno::NOTSUP)
            } else {
                Err(errno::BADF)
            }
        }
        // path_* and sock_*: no file system, no sockets.
        _ => Err(errno::NOTSUP),
    };
    match step {
        Ok(Step::Ret(code)) | Err(code) => {
            host::set_result(results, Value::I32(code))?;
            Ok(HostOutcome::Continue)
        }
        Ok(Step::Block) => Ok(HostOutcome::Block),
        Ok(Step::Exit(code)) => Ok(HostOutcome::Exit(code)),
    }
}
