//! Kalium host ABI definitions, shared between the kernel and the SDK.
//!
//! This crate is the contract described in `docs/prototype-plan.md` under
//! "Host ABI". It contains constants, names and wire layouts, and nothing
//! else: no dependencies, no `unsafe`, no allocation. Both the kernel's
//! import resolver and the SDK's `extern "C"` blocks are written against the
//! names defined here, so a function or namespace that is not in this crate
//! does not exist as far as Kalium is concerned.
//!
//! Quoting the plan:
//!
//! > All host functions are core-Wasm imports. Pointers and lengths are `i32`
//! > offsets into the instance's linear memory; the kernel bounds-checks every
//! > one and returns `EFAULT` on failure. Every fallible call returns an `i32`
//! > errno (`0` is OK). Out-parameters are written into linear memory at the
//! > given pointer. Capabilities are `i32` indices into the instance's
//! > capability table; `-1` is never valid.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// The capability index value that is never valid.
///
/// > Capabilities are `i32` indices into the instance's capability table;
/// > `-1` is never valid.
///
/// The SDK uses it as the "no capability" sentinel; the kernel rejects it
/// (and every other negative index) with [`errno::EBADCAP`].
pub const CAP_NONE: i32 = -1;

/// Errno values returned by every fallible host function (`0` is OK).
pub mod errno {
    /// Success.
    pub const OK: i32 = 0;
    /// Index is not a live slot.
    pub const EBADCAP: i32 = 1;
    /// Slot exists but lacks the right, or is the wrong kind.
    pub const EPERM: i32 = 2;
    /// Bad argument.
    pub const EINVAL: i32 = 3;
    /// Out of memory or over a quota.
    pub const ENOMEM: i32 = 4;
    /// Would block / queue full.
    pub const EAGAIN: i32 = 5;
    /// Peer gone.
    pub const ECLOSED: i32 = 6;
    /// Message or object too large.
    pub const ETOOBIG: i32 = 7;
    /// No such module / unresolved import.
    pub const ENOENT: i32 = 8;
    /// Pointer/length outside linear memory.
    pub const EFAULT: i32 = 9;
    /// Resource in use.
    pub const EBUSY: i32 = 10;

    /// Every errno value with its name, in numeric order. Useful for
    /// diagnostics on both sides of the ABI.
    pub const ALL: &[(i32, &str)] = &[
        (OK, "OK"),
        (EBADCAP, "EBADCAP"),
        (EPERM, "EPERM"),
        (EINVAL, "EINVAL"),
        (ENOMEM, "ENOMEM"),
        (EAGAIN, "EAGAIN"),
        (ECLOSED, "ECLOSED"),
        (ETOOBIG, "ETOOBIG"),
        (ENOENT, "ENOENT"),
        (EFAULT, "EFAULT"),
        (EBUSY, "EBUSY"),
    ];

    /// The symbolic name of an errno value, or `None` if it is not one the
    /// ABI defines.
    pub const fn name(errno: i32) -> Option<&'static str> {
        let mut i = 0;
        while i < ALL.len() {
            if ALL[i].0 == errno {
                return Some(ALL[i].1);
            }
            i += 1;
        }
        None
    }
}

/// Rights (`u64` bit flags).
///
/// > `READ=1, WRITE=2, SEND=4, RECV=8, DERIVE=16, TRANSFER=32, KILL=64,
/// > WAIT=128, SPAWN=256, SNAPSHOT=512`.
///
/// A host call that needs a right the capability lacks fails with
/// [`errno::EPERM`].
pub mod rights {
    /// May read the object (`memobj_read`).
    pub const READ: u64 = 1;
    /// May write the object (`memobj_write`, `console_write`).
    pub const WRITE: u64 = 2;
    /// May send on a channel (`send`).
    pub const SEND: u64 = 4;
    /// May receive from a channel (`recv`, `try_recv`).
    pub const RECV: u64 = 8;
    /// May mint weaker capabilities on the same object (`cap_derive`).
    pub const DERIVE: u64 = 16;
    /// May be moved to another instance (`send` caps, `spawn` caps).
    pub const TRANSFER: u64 = 32;
    /// May terminate the instance (`instance_kill`).
    pub const KILL: u64 = 64;
    /// May block on the object (`wait_any`, `instance_wait`).
    pub const WAIT: u64 = 128;
    /// May create instances (`spawn`, `spawn_snapshot`).
    pub const SPAWN: u64 = 256;
    /// May capture the instance's state (`instance_snapshot`).
    pub const SNAPSHOT: u64 = 512;

    /// Every defined right with its name, in bit order.
    pub const ALL: &[(u64, &str)] = &[
        (READ, "READ"),
        (WRITE, "WRITE"),
        (SEND, "SEND"),
        (RECV, "RECV"),
        (DERIVE, "DERIVE"),
        (TRANSFER, "TRANSFER"),
        (KILL, "KILL"),
        (WAIT, "WAIT"),
        (SPAWN, "SPAWN"),
        (SNAPSHOT, "SNAPSHOT"),
    ];

    /// The union of every defined right. Bits outside this mask are not
    /// rights and a value carrying them is a bad argument.
    pub const MASK: u64 =
        READ | WRITE | SEND | RECV | DERIVE | TRANSFER | KILL | WAIT | SPAWN | SNAPSHOT;

    /// Rights of the send end returned by `channel_create`:
    /// > one with `SEND|DERIVE|TRANSFER`
    pub const CHANNEL_SEND_END: u64 = SEND | DERIVE | TRANSFER;
    /// Rights of the receive end returned by `channel_create`:
    /// > one with `RECV|WAIT|DERIVE|TRANSFER`
    pub const CHANNEL_RECV_END: u64 = RECV | WAIT | DERIVE | TRANSFER;
    /// Rights of the capability returned by `memobj_create`:
    /// > cap with `READ|WRITE|DERIVE|TRANSFER`
    pub const MEMOBJ_OWNER: u64 = READ | WRITE | DERIVE | TRANSFER;
    /// Rights of the `Instance` capability returned by `spawn` and
    /// `spawn_snapshot`:
    /// > Returns an `Instance` cap with `WAIT|KILL|SNAPSHOT|DERIVE|TRANSFER`.
    pub const INSTANCE_OWNER: u64 = WAIT | KILL | SNAPSHOT | DERIVE | TRANSFER;
    /// Rights of the `Snapshot` capability returned by `instance_snapshot`:
    /// > a cap to a new object of kind `Snapshot` with `READ|DERIVE|TRANSFER`
    pub const SNAPSHOT_OWNER: u64 = READ | DERIVE | TRANSFER;
    /// Rights of init's console capability:
    /// > `Console(WRITE|DERIVE|TRANSFER)`
    pub const INIT_CONSOLE: u64 = WRITE | DERIVE | TRANSFER;
    /// Rights of init's spawn capability:
    /// > `Spawn(SPAWN|TRANSFER)`
    pub const INIT_SPAWN: u64 = SPAWN | TRANSFER;
}

/// Object kinds (`u32`).
///
/// > `Console=1, Channel=2, MemObj=3, Instance=4, Spawn=5, Snapshot=6`.
///
/// `0` is reserved and never names a kind.
pub mod kind {
    /// The UART console (`console_write`).
    pub const CONSOLE: u32 = 1;
    /// An IPC channel; the send and receive ends are capabilities on the
    /// same object with different rights.
    pub const CHANNEL: u32 = 2;
    /// A memory object (`memobj_*`).
    pub const MEMOBJ: u32 = 3;
    /// A running or exited instance (`instance_wait`, `instance_kill`,
    /// `instance_snapshot`).
    pub const INSTANCE: u32 = 4;
    /// The authority to create instances (`spawn`). Holding one at
    /// instantiation is what makes `kalium:spawn` resolve.
    pub const SPAWN: u32 = 5;
    /// The captured state of an instance (`instance_snapshot`), which
    /// `spawn_snapshot` starts new instances from. Freed with its last
    /// capability.
    pub const SNAPSHOT: u32 = 6;

    /// Every defined kind with its name, in numeric order.
    pub const ALL: &[(u32, &str)] = &[
        (CONSOLE, "Console"),
        (CHANNEL, "Channel"),
        (MEMOBJ, "MemObj"),
        (INSTANCE, "Instance"),
        (SPAWN, "Spawn"),
        (SNAPSHOT, "Snapshot"),
    ];
}

/// Import module names and the function names in each.
///
/// > | Module name | Resolves when |
/// > | --- | --- |
/// > | `kalium:core` | always |
/// > | `kalium:ipc` | always |
/// > | `kalium:memory` | always |
/// > | `kalium:spawn` | the instance holds at least one capability of kind `Spawn` at instantiation |
/// >
/// > An import that does not resolve is a link error and the instance is not
/// > created (`ENOENT` to the spawner).
///
/// Each namespace module has a `MODULE` constant (the Wasm import module
/// string), one constant per function name, and a `FUNCTIONS` list of those
/// names. [`ALL_IMPORTS`](imports::ALL_IMPORTS) flattens them into `(module, name)` pairs for the
/// kernel's resolver and the SDK.
pub mod imports {
    /// `kalium:core`: capability table operations, time, yield, exit.
    /// Always resolves.
    pub mod core {
        /// The import module name.
        pub const MODULE: &str = "kalium:core";

        /// `(cap: i32, ptr: i32, len: i32) -> i32`
        /// > Needs `Console` kind with `WRITE`. Writes bytes to the UART.
        pub const CONSOLE_WRITE: &str = "console_write";
        /// `(cap: i32) -> i32`
        /// > Removes the slot.
        pub const CAP_DROP: &str = "cap_drop";
        /// `(cap: i32, rights: i64, out_cap: i32) -> i32`
        /// > Needs `DERIVE`. New cap has `rights & old.rights` and never
        /// > gains `DERIVE` unless the old had it. Writes new index as `i32`
        /// > at `out_cap`. `ENOMEM` beyond `MAX_DERIVE_DEPTH` derivations
        /// > from a fresh capability.
        pub const CAP_DERIVE: &str = "cap_derive";
        /// `(cap: i32) -> i32`
        /// > Needs a live, unrevoked slot; no right. Invalidates every
        /// > capability derived from it, transitively, wherever it went;
        /// > the slot itself stays valid and may derive again. A revoked
        /// > channel end stops counting as an endpoint at once.
        ///
        /// See `docs/prototype-plan.md`, "Revocation".
        pub const CAP_REVOKE: &str = "cap_revoke";
        /// `(cap: i32, out: i32) -> i32`
        /// > Writes `{kind: u32, rights: u64}` (12 bytes, little-endian, no
        /// > padding) at `out`.
        ///
        /// See [`crate::layout::CapInfo`].
        pub const CAP_INFO: &str = "cap_info";
        /// `() -> i64`
        /// > Nanoseconds since boot, monotonic.
        pub const TIME_NOW: &str = "time_now";
        /// `() -> ()`
        /// > Gives up the rest of the time slice.
        pub const YIELD: &str = "yield_";
        /// `(ns: i64) -> ()`
        /// > Blocks for at least `ns`.
        pub const SLEEP: &str = "sleep";
        /// `(code: i32) -> ()`
        /// > Terminates the instance. Does not return.
        pub const EXIT: &str = "exit";
        /// `() -> i32`
        /// > The calling instance's id (for logs).
        pub const INSTANCE_ID: &str = "instance_id";

        /// Every function in this namespace, in ABI table order.
        pub const FUNCTIONS: &[&str] = &[
            CONSOLE_WRITE,
            CAP_DROP,
            CAP_DERIVE,
            CAP_REVOKE,
            CAP_INFO,
            TIME_NOW,
            YIELD,
            SLEEP,
            EXIT,
            INSTANCE_ID,
        ];
    }

    /// `kalium:ipc`: channels with capability transfer. Always resolves.
    pub mod ipc {
        /// The import module name.
        pub const MODULE: &str = "kalium:ipc";

        /// `(out_send: i32, out_recv: i32) -> i32`
        /// > Creates a channel. Writes two cap indices: one with
        /// > `SEND|DERIVE|TRANSFER`, one with `RECV|WAIT|DERIVE|TRANSFER`.
        /// > Both refer to the same object.
        pub const CHANNEL_CREATE: &str = "channel_create";
        /// `(cap: i32, msg: i32, len: i32, caps: i32, ncaps: i32) -> i32`
        /// > Needs `SEND`. `caps` points to `ncaps` `i32` indices; each must
        /// > have `TRANSFER`; they are removed from the sender's table on
        /// > success. `EAGAIN` if the queue is full (queue depth 16, message
        /// > limit 4096 bytes, up to 4 caps). `ECLOSED` if no receiver cap
        /// > exists any more.
        pub const SEND: &str = "send";
        /// `(cap: i32, buf: i32, buf_len: i32, caps: i32, ncaps_max: i32, out_hdr: i32) -> i32`
        /// > Needs `RECV`. Blocks until a message is available. Copies at
        /// > most `buf_len` bytes; the transferred caps are added to the
        /// > receiver's table and their indices written at `caps`. `out_hdr`
        /// > receives `{len: u32, ncaps: u32}` with the full message length
        /// > (so truncation is detectable). `ECLOSED` if the queue is empty
        /// > and no sender cap exists any more.
        ///
        /// See [`crate::layout::RecvHeader`].
        pub const RECV: &str = "recv";
        /// Same signature as [`RECV`].
        /// > Non-blocking: `EAGAIN` instead of blocking.
        pub const TRY_RECV: &str = "try_recv";
        /// `(caps: i32, n: i32, out_index: i32) -> i32`
        /// > Blocks until one of `n` receive caps has a message; writes its
        /// > position (0-based) at `out_index`. All need `WAIT`.
        pub const WAIT_ANY: &str = "wait_any";

        /// Every function in this namespace, in ABI table order.
        pub const FUNCTIONS: &[&str] = &[CHANNEL_CREATE, SEND, RECV, TRY_RECV, WAIT_ANY];
    }

    /// `kalium:memory`: memory objects. Always resolves.
    ///
    /// > Mapping memory objects into linear memory is deferred to the
    /// > multi-memory / MMU-backed design; M1 copies.
    pub mod memory {
        /// The import module name.
        pub const MODULE: &str = "kalium:memory";

        /// `(size: i64, out_cap: i32) -> i32`
        /// > Zeroed memory object, cap with `READ|WRITE|DERIVE|TRANSFER`.
        /// > Size limit 16 MiB in M1.
        pub const MEMOBJ_CREATE: &str = "memobj_create";
        /// `(cap: i32, out_size: i32) -> i32`
        /// > Writes `i64`.
        pub const MEMOBJ_SIZE: &str = "memobj_size";
        /// `(cap: i32, offset: i64, ptr: i32, len: i32) -> i32`
        /// > Needs `READ`.
        pub const MEMOBJ_READ: &str = "memobj_read";
        /// `(cap: i32, offset: i64, ptr: i32, len: i32) -> i32`
        /// > Needs `WRITE`.
        pub const MEMOBJ_WRITE: &str = "memobj_write";

        /// Every function in this namespace, in ABI table order.
        pub const FUNCTIONS: &[&str] = &[MEMOBJ_CREATE, MEMOBJ_SIZE, MEMOBJ_READ, MEMOBJ_WRITE];
    }

    /// `kalium:spawn`: instance creation and control. Resolves only when
    /// the instance holds a `Spawn` capability at instantiation.
    pub mod spawn {
        /// The import module name.
        pub const MODULE: &str = "kalium:spawn";

        /// `(cap: i32, module: i32, caps: i32, ncaps: i32, class: i32, period_ns: i64, budget_ns: i64, out_cap: i32) -> i32`
        /// > Needs `Spawn` kind with `SPAWN`. `module` is an index into the
        /// > boot image's module table. The `ncaps` caps (each needs
        /// > `TRANSFER`) are moved to the child, becoming its slots
        /// > `0..ncaps` in order. `class` is `0` fair, `1` real-time. Returns
        /// > an `Instance` cap with `WAIT|KILL|SNAPSHOT|DERIVE|TRANSFER`.
        pub const SPAWN: &str = "spawn";
        /// `(cap: i32, out_code: i32) -> i32`
        /// > Needs `WAIT`. Blocks until the instance exits; writes exit code
        /// > (`i32`; traps report `-1`, kill reports `-2`).
        pub const INSTANCE_WAIT: &str = "instance_wait";
        /// `(cap: i32) -> i32`
        /// > Needs `KILL`.
        pub const INSTANCE_KILL: &str = "instance_kill";
        /// `() -> i32`
        /// > Number of modules in the boot image.
        pub const MODULE_COUNT: &str = "module_count";
        /// `(cap: i32, out_cap: i32) -> i32`
        /// > Needs `Instance` kind with `SNAPSHOT`. Captures the state of
        /// > the instance (its memory, globals and tables; nothing of the
        /// > execution in flight) into a new `Snapshot` object and writes
        /// > a cap on it with `READ|DERIVE|TRANSFER` at `out_cap`. The
        /// > instance must be blocked or sleeping: `EBUSY` while it is
        /// > runnable (or is the caller), `ECLOSED` once it has exited.
        /// > `EBUSY` as well if its memory or tables are shared.
        ///
        /// See `docs/prototype-plan.md`, "Snapshots".
        pub const INSTANCE_SNAPSHOT: &str = "instance_snapshot";
        /// `(cap: i32, snapshot_cap: i32, caps: i32, ncaps: i32, class: i32, period_ns: i64, budget_ns: i64, entry: i32, out_cap: i32) -> i32`
        /// > Like `spawn`, but the instance is created from the snapshot's
        /// > module with the snapshot's state (no segment or start function
        /// > runs) and starts at `entry` ([`crate::entry`]): `0` the export
        /// > `_start`, `1` the export `resume`; `ENOENT` if the export is
        /// > missing or not of type `[] -> []`, `EINVAL` for any other
        /// > value. `snapshot_cap` needs `Snapshot` kind with `READ`.
        pub const SPAWN_SNAPSHOT: &str = "spawn_snapshot";

        /// Every function in this namespace, in ABI table order.
        pub const FUNCTIONS: &[&str] = &[
            SPAWN,
            INSTANCE_WAIT,
            INSTANCE_KILL,
            MODULE_COUNT,
            INSTANCE_SNAPSHOT,
            SPAWN_SNAPSHOT,
        ];
    }

    /// Every import module name, in the order of the plan's table.
    pub const MODULES: &[&str] = &[core::MODULE, ipc::MODULE, memory::MODULE, spawn::MODULE];

    /// Every host function as a `(module, name)` pair, in ABI table order.
    ///
    /// This is the complete set of imports the kernel resolves; an import
    /// whose pair is not in this table is a link error. Each name appears
    /// exactly once across all namespaces, so `name` alone identifies a
    /// host function.
    pub const ALL_IMPORTS: &[(&str, &str)] = &[
        (core::MODULE, core::CONSOLE_WRITE),
        (core::MODULE, core::CAP_DROP),
        (core::MODULE, core::CAP_DERIVE),
        (core::MODULE, core::CAP_REVOKE),
        (core::MODULE, core::CAP_INFO),
        (core::MODULE, core::TIME_NOW),
        (core::MODULE, core::YIELD),
        (core::MODULE, core::SLEEP),
        (core::MODULE, core::EXIT),
        (core::MODULE, core::INSTANCE_ID),
        (ipc::MODULE, ipc::CHANNEL_CREATE),
        (ipc::MODULE, ipc::SEND),
        (ipc::MODULE, ipc::RECV),
        (ipc::MODULE, ipc::TRY_RECV),
        (ipc::MODULE, ipc::WAIT_ANY),
        (memory::MODULE, memory::MEMOBJ_CREATE),
        (memory::MODULE, memory::MEMOBJ_SIZE),
        (memory::MODULE, memory::MEMOBJ_READ),
        (memory::MODULE, memory::MEMOBJ_WRITE),
        (spawn::MODULE, spawn::SPAWN),
        (spawn::MODULE, spawn::INSTANCE_WAIT),
        (spawn::MODULE, spawn::INSTANCE_KILL),
        (spawn::MODULE, spawn::MODULE_COUNT),
        (spawn::MODULE, spawn::INSTANCE_SNAPSHOT),
        (spawn::MODULE, spawn::SPAWN_SNAPSHOT),
    ];

    /// Whether `module` is an import namespace the kernel knows about.
    pub fn is_known_module(module: &str) -> bool {
        MODULES.contains(&module)
    }

    /// The position of `(module, name)` in [`ALL_IMPORTS`], if it is a host
    /// function. The position is stable across kernel and SDK and is a
    /// convenient `HostFuncId`.
    pub fn lookup(module: &str, name: &str) -> Option<usize> {
        ALL_IMPORTS
            .iter()
            .position(|(m, n)| *m == module && *n == name)
    }
}

/// Wire layouts of the structures host functions write into linear memory.
///
/// All multi-byte fields are little-endian and there is no padding. The
/// structs here are plain Rust values; `to_le_bytes`/`from_le_bytes` define
/// the layout, so neither side needs `repr(packed)` or `unsafe`.
pub mod layout {
    /// Out-parameter of `cap_info`:
    /// > Writes `{kind: u32, rights: u64}` (12 bytes, little-endian, no
    /// > padding) at `out`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct CapInfo {
        /// The object kind, one of [`crate::kind`].
        pub kind: u32,
        /// The rights bits, a union of [`crate::rights`].
        pub rights: u64,
    }

    /// Size in bytes of [`CapInfo`] on the wire.
    pub const CAP_INFO_SIZE: usize = 12;
    /// Byte offset of `CapInfo::kind`.
    pub const CAP_INFO_KIND_OFFSET: usize = 0;
    /// Byte offset of `CapInfo::rights`.
    pub const CAP_INFO_RIGHTS_OFFSET: usize = 4;

    impl CapInfo {
        /// Encodes as the kernel writes it at `out`.
        pub const fn to_le_bytes(self) -> [u8; CAP_INFO_SIZE] {
            let k = self.kind.to_le_bytes();
            let r = self.rights.to_le_bytes();
            [
                k[0], k[1], k[2], k[3], r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7],
            ]
        }

        /// Decodes what the kernel wrote at `out`.
        pub const fn from_le_bytes(b: [u8; CAP_INFO_SIZE]) -> Self {
            CapInfo {
                kind: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                rights: u64::from_le_bytes([b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11]]),
            }
        }
    }

    /// Out-parameter `out_hdr` of `recv` and `try_recv`:
    /// > `out_hdr` receives `{len: u32, ncaps: u32}` with the full message
    /// > length (so truncation is detectable).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct RecvHeader {
        /// Full length of the message in bytes, even if fewer were copied.
        pub len: u32,
        /// Number of capabilities transferred with the message.
        pub ncaps: u32,
    }

    /// Size in bytes of [`RecvHeader`] on the wire.
    pub const RECV_HEADER_SIZE: usize = 8;
    /// Byte offset of `RecvHeader::len`.
    pub const RECV_HEADER_LEN_OFFSET: usize = 0;
    /// Byte offset of `RecvHeader::ncaps`.
    pub const RECV_HEADER_NCAPS_OFFSET: usize = 4;

    impl RecvHeader {
        /// Encodes as the kernel writes it at `out_hdr`.
        pub const fn to_le_bytes(self) -> [u8; RECV_HEADER_SIZE] {
            let l = self.len.to_le_bytes();
            let n = self.ncaps.to_le_bytes();
            [l[0], l[1], l[2], l[3], n[0], n[1], n[2], n[3]]
        }

        /// Decodes what the kernel wrote at `out_hdr`.
        pub const fn from_le_bytes(b: [u8; RECV_HEADER_SIZE]) -> Self {
            RecvHeader {
                len: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                ncaps: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            }
        }
    }

    /// Size in bytes of one capability index as passed in `caps` arrays
    /// (`send`, `recv`, `wait_any`, `spawn`) and written at `out_cap`.
    pub const CAP_INDEX_SIZE: usize = 4;
}

/// Queue, message and object limits.
pub mod limits {
    /// Channel queue depth:
    /// > `EAGAIN` if the queue is full (queue depth 16, ...)
    pub const CHANNEL_QUEUE_DEPTH: usize = 16;
    /// Maximum message payload in bytes:
    /// > ... message limit 4096 bytes, ...
    pub const MAX_MESSAGE_LEN: usize = 4096;
    /// Maximum capabilities transferred in one message:
    /// > ... up to 4 caps
    pub const MAX_MESSAGE_CAPS: usize = 4;
    /// Maximum memory object size:
    /// > Size limit 16 MiB in M1.
    pub const MAX_MEMOBJ_SIZE: u64 = 16 * 1024 * 1024;
    /// Maximum number of live instances, kernel-wide and counting `init`:
    /// > At most 32 instances may be live at once ... `spawn` fails with
    /// > `ENOMEM` beyond that
    ///
    /// Bounds what a holder of a `Spawn` capability can make the kernel
    /// allocate, whatever it does with the `Instance` capabilities it gets
    /// back. An exited instance's slot is reused.
    pub const MAX_INSTANCES: usize = 32;
    /// Longest chain of derivations from a capability the kernel minted
    /// fresh (`cap_derive` of a `cap_derive` of ...): the fresh one is at
    /// depth 0 and `cap_derive` fails with `ENOMEM` on one at this depth.
    /// It bounds the lineage walk a capability check makes.
    pub const MAX_DERIVE_DEPTH: u32 = 16;

    /// Per-instance quota on memory objects, in bytes: the sizes of the
    /// objects an instance created with `memobj_create` that still exist,
    /// summed. `memobj_create` fails with `ENOMEM` once a new object would
    /// take the creator past it (the object size limit
    /// [`MAX_MEMOBJ_SIZE`] still applies first, as `ETOOBIG`). An object is
    /// charged to its creator until it is freed (last capability gone) or
    /// the creator exits; see `docs/prototype-plan.md`, "Resource quotas".
    pub const MAX_MEMOBJ_BYTES_PER_INSTANCE: u64 = 32 * 1024 * 1024;
    /// Per-instance quota on snapshots, in bytes (as `Snapshot::to_bytes`
    /// would encode them): the snapshots an instance took with
    /// `instance_snapshot` that still exist, summed. `instance_snapshot`
    /// fails with `ENOMEM` beyond it. Charged like a memory object.
    pub const MAX_SNAPSHOT_BYTES_PER_INSTANCE: u64 = 32 * 1024 * 1024;
    /// Per-instance quota on channels: the channels an instance created
    /// with `channel_create` that still exist. `channel_create` fails with
    /// `ENOMEM` beyond it. With [`CHANNEL_QUEUE_DEPTH`] and
    /// [`MAX_MESSAGE_LEN`] this bounds what the queues of an instance's
    /// channels can hold (4 MiB of payload).
    pub const MAX_CHANNELS_PER_INSTANCE: usize = 64;
}

/// The WASI preview1 compatibility layer (`docs/prototype-plan.md`, "WASI
/// preview1 compatibility"): the import module the kernel resolves for
/// every instance and the capability slots its standard streams map onto.
///
/// These are additions beside the host ABI, not part of it: the module is
/// deliberately absent from [`imports::MODULES`] and [`imports::ALL_IMPORTS`],
/// whose function table is Kalium's own. The function names and signatures
/// of the snapshot are declared in the kernel (`kernel/src/wasi.rs`).
pub mod wasi {
    /// The import module name of WASI preview1.
    pub const MODULE: &str = "wasi_snapshot_preview1";

    /// Capability slot that backs fds 1 and 2 (stdout, stderr): a `Console`
    /// with `WRITE`. Without it `fd_write` fails with WASI `EBADF`.
    pub const STDOUT_CONSOLE_SLOT: i32 = 0;
    /// Capability slot that backs fd 0 (stdin): a `Channel` with `RECV`,
    /// each message a chunk of input. Without it `fd_read` fails with WASI
    /// `EBADF`.
    pub const STDIN_CHANNEL_SLOT: i32 = 1;

    /// WASI file descriptor numbers of the standard streams, the only fds
    /// that exist.
    pub const FD_STDIN: i32 = 0;
    /// See [`FD_STDIN`].
    pub const FD_STDOUT: i32 = 1;
    /// See [`FD_STDIN`].
    pub const FD_STDERR: i32 = 2;
}

/// Entry points of an instance created by `spawn_snapshot`.
///
/// > `entry` chooses the export the instance starts at, by convention:
/// > `0` runs the export `_start` (the WASI command convention, which
/// > `spawn` always uses), `1` runs the export named `resume`. Either must
/// > be of type `[] -> []`; `ENOENT` if it is missing or mistyped.
///
/// A service that is meant to be shelved exports both: `_start` builds its
/// state and parks, `resume` continues from the restored state.
pub mod entry {
    /// Start at the export `_start`.
    pub const START: i32 = 0;
    /// Start at the export `resume`.
    pub const RESUME: i32 = 1;
    /// The export name [`START`] runs (and `spawn` always runs).
    pub const START_EXPORT: &str = "_start";
    /// The export name [`RESUME`] runs.
    pub const RESUME_EXPORT: &str = "resume";
}

/// Scheduling class values passed to `spawn`.
///
/// > `class` is `0` fair, `1` real-time.
pub mod sched {
    /// Weighted fair queuing; `period_ns` and `budget_ns` are ignored.
    pub const CLASS_FAIR: i32 = 0;
    /// Real-time with `period_ns` and `budget_ns`.
    pub const CLASS_REAL_TIME: i32 = 1;
}

/// Exit code conventions as reported through `instance_wait`.
pub mod exit {
    /// Normal termination of a `_start` that returns without calling `exit`.
    pub const SUCCESS: i32 = 0;
    /// The instance trapped:
    /// > traps report `-1`
    pub const TRAP: i32 = -1;
    /// The instance was killed through `instance_kill`:
    /// > kill reports `-2`
    pub const KILLED: i32 = -2;
    /// The SDK's panic handler:
    /// > a panic handler calling `exit(101)`
    pub const PANIC: i32 = 101;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rights_are_distinct_powers_of_two() {
        for (i, (bit, name)) in rights::ALL.iter().enumerate() {
            assert_eq!(bit.count_ones(), 1, "{name} is not a power of two");
            assert_eq!(*bit, 1u64 << i, "{name} is out of order");
            for (other, other_name) in &rights::ALL[..i] {
                assert_ne!(bit, other, "{name} and {other_name} share a bit");
            }
        }
        assert_eq!(rights::ALL.len(), 10);
        assert_eq!(rights::MASK, 0x3FF);
        assert_eq!(rights::READ, 1);
        assert_eq!(rights::WRITE, 2);
        assert_eq!(rights::SEND, 4);
        assert_eq!(rights::RECV, 8);
        assert_eq!(rights::DERIVE, 16);
        assert_eq!(rights::TRANSFER, 32);
        assert_eq!(rights::KILL, 64);
        assert_eq!(rights::WAIT, 128);
        assert_eq!(rights::SPAWN, 256);
        assert_eq!(rights::SNAPSHOT, 512);
    }

    #[test]
    fn preset_rights_are_within_mask() {
        for preset in [
            rights::CHANNEL_SEND_END,
            rights::CHANNEL_RECV_END,
            rights::MEMOBJ_OWNER,
            rights::INSTANCE_OWNER,
            rights::SNAPSHOT_OWNER,
            rights::INIT_CONSOLE,
            rights::INIT_SPAWN,
        ] {
            assert_eq!(preset & !rights::MASK, 0);
            assert_ne!(preset, 0);
        }
        assert_eq!(rights::INSTANCE_OWNER & rights::SNAPSHOT, rights::SNAPSHOT);
        assert_eq!(rights::SNAPSHOT_OWNER, 1 | 16 | 32);
    }

    #[test]
    fn errno_values_match_plan() {
        let expected: &[(i32, &str)] = &[
            (0, "OK"),
            (1, "EBADCAP"),
            (2, "EPERM"),
            (3, "EINVAL"),
            (4, "ENOMEM"),
            (5, "EAGAIN"),
            (6, "ECLOSED"),
            (7, "ETOOBIG"),
            (8, "ENOENT"),
            (9, "EFAULT"),
            (10, "EBUSY"),
        ];
        assert_eq!(errno::ALL, expected);
        for (i, (value, _)) in errno::ALL.iter().enumerate() {
            assert_eq!(*value, i as i32);
        }
        assert_eq!(errno::name(errno::EPERM), Some("EPERM"));
        assert_eq!(errno::name(11), None);
        assert_eq!(errno::name(-1), None);
    }

    #[test]
    fn kinds_match_plan() {
        let expected: &[(u32, &str)] = &[
            (1, "Console"),
            (2, "Channel"),
            (3, "MemObj"),
            (4, "Instance"),
            (5, "Spawn"),
            (6, "Snapshot"),
        ];
        assert_eq!(kind::ALL, expected);
        for (i, (value, _)) in kind::ALL.iter().enumerate() {
            assert_eq!(*value as usize, i + 1, "kinds are dense from 1");
        }
    }

    #[test]
    fn module_names_match_plan() {
        assert_eq!(imports::core::MODULE, "kalium:core");
        assert_eq!(imports::ipc::MODULE, "kalium:ipc");
        assert_eq!(imports::memory::MODULE, "kalium:memory");
        assert_eq!(imports::spawn::MODULE, "kalium:spawn");
        assert_eq!(
            imports::MODULES,
            &["kalium:core", "kalium:ipc", "kalium:memory", "kalium:spawn"]
        );
        for (i, m) in imports::MODULES.iter().enumerate() {
            assert!(m.starts_with("kalium:"));
            assert!(imports::is_known_module(m));
            assert!(!imports::MODULES[..i].contains(m), "duplicate module {m}");
        }
        assert!(!imports::is_known_module("kalium:device"));
        assert!(!imports::is_known_module("env"));
    }

    #[test]
    fn every_function_name_is_listed_exactly_once() {
        // The per-namespace lists, concatenated, are exactly ALL_IMPORTS.
        let namespaces: &[(&str, &[&str])] = &[
            (imports::core::MODULE, imports::core::FUNCTIONS),
            (imports::ipc::MODULE, imports::ipc::FUNCTIONS),
            (imports::memory::MODULE, imports::memory::FUNCTIONS),
            (imports::spawn::MODULE, imports::spawn::FUNCTIONS),
        ];
        let mut flat_len = 0;
        for (module, functions) in namespaces {
            for name in functions.iter() {
                assert_eq!(
                    imports::ALL_IMPORTS[flat_len],
                    (*module, *name),
                    "ALL_IMPORTS is out of order at {flat_len}"
                );
                flat_len += 1;
            }
        }
        assert_eq!(flat_len, imports::ALL_IMPORTS.len());
        assert_eq!(imports::ALL_IMPORTS.len(), 10 + 5 + 4 + 6);

        // Every name is unique across all namespaces, not just within one.
        for (i, (module, name)) in imports::ALL_IMPORTS.iter().enumerate() {
            assert!(imports::is_known_module(module));
            assert!(!name.is_empty());
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit()),
                "{name} is not a plain identifier"
            );
            for (_, other) in &imports::ALL_IMPORTS[..i] {
                assert_ne!(name, other, "{name} is listed more than once");
            }
            assert_eq!(imports::lookup(module, name), Some(i));
        }
    }

    #[test]
    fn every_table_function_is_present() {
        // The function names from the plan's ABI tables, verbatim.
        let plan: &[(&str, &str)] = &[
            ("kalium:core", "console_write"),
            ("kalium:core", "cap_drop"),
            ("kalium:core", "cap_derive"),
            ("kalium:core", "cap_revoke"),
            ("kalium:core", "cap_info"),
            ("kalium:core", "time_now"),
            ("kalium:core", "yield_"),
            ("kalium:core", "sleep"),
            ("kalium:core", "exit"),
            ("kalium:core", "instance_id"),
            ("kalium:ipc", "channel_create"),
            ("kalium:ipc", "send"),
            ("kalium:ipc", "recv"),
            ("kalium:ipc", "try_recv"),
            ("kalium:ipc", "wait_any"),
            ("kalium:memory", "memobj_create"),
            ("kalium:memory", "memobj_size"),
            ("kalium:memory", "memobj_read"),
            ("kalium:memory", "memobj_write"),
            ("kalium:spawn", "spawn"),
            ("kalium:spawn", "instance_wait"),
            ("kalium:spawn", "instance_kill"),
            ("kalium:spawn", "module_count"),
            ("kalium:spawn", "instance_snapshot"),
            ("kalium:spawn", "spawn_snapshot"),
        ];
        assert_eq!(imports::ALL_IMPORTS, plan);
    }

    #[test]
    fn lookup_rejects_unknown_pairs() {
        assert_eq!(imports::lookup("kalium:core", "send"), None);
        assert_eq!(imports::lookup("kalium:ipc", "console_write"), None);
        assert_eq!(imports::lookup("env", "exit"), None);
        assert_eq!(imports::lookup("kalium:core", ""), None);
        assert_eq!(imports::lookup("kalium:core", "yield"), None);
    }

    #[test]
    fn cap_info_layout() {
        assert_eq!(layout::CAP_INFO_SIZE, 12);
        let info = layout::CapInfo {
            kind: kind::CHANNEL,
            rights: rights::CHANNEL_RECV_END,
        };
        let bytes = info.to_le_bytes();
        assert_eq!(&bytes[..4], &2u32.to_le_bytes());
        assert_eq!(&bytes[4..], &(8u64 | 128 | 16 | 32).to_le_bytes());
        assert_eq!(layout::CapInfo::from_le_bytes(bytes), info);

        let info = layout::CapInfo {
            kind: 0xA1B2_C3D4,
            rights: 0x0102_0304_0506_0708,
        };
        let bytes = info.to_le_bytes();
        assert_eq!(
            bytes,
            [0xD4, 0xC3, 0xB2, 0xA1, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
        assert_eq!(layout::CapInfo::from_le_bytes(bytes), info);
        assert_eq!(layout::CAP_INFO_KIND_OFFSET, 0);
        assert_eq!(layout::CAP_INFO_RIGHTS_OFFSET, 4);
    }

    #[test]
    fn recv_header_layout() {
        assert_eq!(layout::RECV_HEADER_SIZE, 8);
        let hdr = layout::RecvHeader {
            len: 0x0000_1000,
            ncaps: 3,
        };
        let bytes = hdr.to_le_bytes();
        assert_eq!(bytes, [0x00, 0x10, 0, 0, 3, 0, 0, 0]);
        assert_eq!(layout::RecvHeader::from_le_bytes(bytes), hdr);
        assert_eq!(layout::RECV_HEADER_LEN_OFFSET, 0);
        assert_eq!(layout::RECV_HEADER_NCAPS_OFFSET, 4);
        assert_eq!(layout::CAP_INDEX_SIZE, core::mem::size_of::<i32>());
    }

    #[test]
    fn wasi_constants() {
        assert_eq!(wasi::MODULE, "wasi_snapshot_preview1");
        // WASI is beside the host ABI, not in its import table.
        assert!(!imports::is_known_module(wasi::MODULE));
        assert_eq!(imports::lookup(wasi::MODULE, "fd_write"), None);
        assert_eq!(wasi::STDOUT_CONSOLE_SLOT, 0);
        assert_eq!(wasi::STDIN_CHANNEL_SLOT, 1);
        assert_eq!(
            [wasi::FD_STDIN, wasi::FD_STDOUT, wasi::FD_STDERR],
            [0, 1, 2]
        );
    }

    #[test]
    fn limits_and_misc_match_plan() {
        assert_eq!(limits::CHANNEL_QUEUE_DEPTH, 16);
        assert_eq!(limits::MAX_MESSAGE_LEN, 4096);
        assert_eq!(limits::MAX_MESSAGE_CAPS, 4);
        assert_eq!(limits::MAX_MEMOBJ_SIZE, 16 << 20);
        assert_eq!(limits::MAX_INSTANCES, 32);
        assert_eq!(limits::MAX_DERIVE_DEPTH, 16);
        assert_eq!(limits::MAX_MEMOBJ_BYTES_PER_INSTANCE, 32 << 20);
        assert_eq!(limits::MAX_SNAPSHOT_BYTES_PER_INSTANCE, 32 << 20);
        assert_eq!(limits::MAX_CHANNELS_PER_INSTANCE, 64);
        // The quotas hold whole objects of the largest size and bound the
        // queued payload of an instance's channels to 4 MiB.
        assert_eq!(
            limits::MAX_MEMOBJ_BYTES_PER_INSTANCE % limits::MAX_MEMOBJ_SIZE,
            0
        );
        assert_eq!(
            limits::MAX_CHANNELS_PER_INSTANCE
                * limits::CHANNEL_QUEUE_DEPTH
                * limits::MAX_MESSAGE_LEN,
            4 << 20
        );
        assert_eq!(sched::CLASS_FAIR, 0);
        assert_eq!(sched::CLASS_REAL_TIME, 1);
        assert_eq!(entry::START, 0);
        assert_eq!(entry::RESUME, 1);
        assert_eq!(entry::START_EXPORT, "_start");
        assert_eq!(entry::RESUME_EXPORT, "resume");
        assert_eq!(exit::SUCCESS, 0);
        assert_eq!(exit::TRAP, -1);
        assert_eq!(exit::KILLED, -2);
        assert_eq!(exit::PANIC, 101);
        assert_eq!(CAP_NONE, -1);
    }
}
