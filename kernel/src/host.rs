//! The host ABI: one [`Host`] implementation that dispatches on
//! [`HostFuncId`] to every function of `kalium:core`, `kalium:ipc`,
//! `kalium:memory` and `kalium:spawn` (including the snapshot pair,
//! `instance_snapshot`/`spawn_snapshot`, see `kernel.rs`, "Snapshots")
//! with the semantics of
//! `docs/prototype-plan.md`, and hands the `wasi_snapshot_preview1` ids
//! (the range starting at [`crate::wasi::BASE`]) to [`crate::wasi`].
//!
//! Conventions shared by every function:
//!
//! - a `HostFuncId` is the position of the import in
//!   [`kalium_abi::imports::ALL_IMPORTS`] ([`check_table`] verifies the
//!   constants below against it at boot);
//! - every pointer/length pair is bounds-checked against the calling
//!   instance's linear memory before anything else happens (`EFAULT`); an
//!   empty range is accepted whatever its pointer; a negative count is
//!   `EINVAL`;
//! - capability indices go through [`HostCall::check_cap`] (or
//!   [`HostCall::get_cap`] / [`HostCall::check_cap_rights`]): the slot must
//!   be live and the capability valid in its object's lineage, i.e. not
//!   revoked (`EBADCAP`, see `kernel.rs`), then of the right kind with the
//!   right rights (`EPERM`);
//! - blocking functions return [`HostOutcome::Block`] after parking the
//!   thread; the interpreter rewinds to the call, so the call is re-issued
//!   when the thread runs again and re-checks everything. `yield_` and
//!   `sleep` use the same mechanism with a per-instance flag/deadline so
//!   that the re-issued call completes;
//! - the [`Effect`] tells the run loop how to account the slice when the
//!   outcome is `Block`;
//! - nothing sized by the instance's arguments is allocated infallibly:
//!   message copies, capability lists and the like go through
//!   `try_reserve` ([`try_vec`]) and report `ENOMEM`, the objects an
//!   instance creates are charged to its quotas and the kernel-wide
//!   budget first (`kernel.rs`, "Quotas and the instance budget"), and
//!   capability lists longer than a table (`wait_any`, `spawn`) or a
//!   message (`send`) could hold are refused before they are read.

use alloc::sync::Arc;
use alloc::vec::Vec;

use kalium_abi::layout::RecvHeader;
use kalium_abi::{
    entry as abi_entry, errno, exit as abi_exit, imports, rights, sched as abi_sched,
};
use kalium_cap::{CapError, CapIndex, CapTable, Capability, ObjectKind, Rights};
use kalium_hal::{console, println, timer};
use kalium_ipc::{Channel, IpcError, Message, WaitQueue};
use kalium_mm::{MemError, MemObject};
use kalium_sched::{Class, State, ThreadId};
use kalium_wasm::{
    Error as WasmError, FuncType, Host, HostCtx, HostFuncId, HostOutcome, Memory, Snapshot, Trap,
    ValType, Value,
};

use crate::kernel::{Kernel, Object, Quota, CAP_TABLE_LIMIT};

// Host function ids: positions in `kalium_abi::imports::ALL_IMPORTS`.
pub const CONSOLE_WRITE: u32 = 0;
pub const CAP_DROP: u32 = 1;
pub const CAP_DERIVE: u32 = 2;
pub const CAP_REVOKE: u32 = 3;
pub const CAP_INFO: u32 = 4;
pub const TIME_NOW: u32 = 5;
pub const YIELD: u32 = 6;
pub const SLEEP: u32 = 7;
pub const EXIT: u32 = 8;
pub const INSTANCE_ID: u32 = 9;
pub const CHANNEL_CREATE: u32 = 10;
pub const SEND: u32 = 11;
pub const RECV: u32 = 12;
pub const TRY_RECV: u32 = 13;
pub const WAIT_ANY: u32 = 14;
pub const MEMOBJ_CREATE: u32 = 15;
pub const MEMOBJ_SIZE: u32 = 16;
pub const MEMOBJ_READ: u32 = 17;
pub const MEMOBJ_WRITE: u32 = 18;
pub const SPAWN: u32 = 19;
pub const INSTANCE_WAIT: u32 = 20;
pub const INSTANCE_KILL: u32 = 21;
pub const MODULE_COUNT: u32 = 22;
pub const INSTANCE_SNAPSHOT: u32 = 23;
pub const SPAWN_SNAPSHOT: u32 = 24;

const TABLE: [(u32, &str, &str); 25] = [
    (
        CONSOLE_WRITE,
        imports::core::MODULE,
        imports::core::CONSOLE_WRITE,
    ),
    (CAP_DROP, imports::core::MODULE, imports::core::CAP_DROP),
    (CAP_DERIVE, imports::core::MODULE, imports::core::CAP_DERIVE),
    (CAP_REVOKE, imports::core::MODULE, imports::core::CAP_REVOKE),
    (CAP_INFO, imports::core::MODULE, imports::core::CAP_INFO),
    (TIME_NOW, imports::core::MODULE, imports::core::TIME_NOW),
    (YIELD, imports::core::MODULE, imports::core::YIELD),
    (SLEEP, imports::core::MODULE, imports::core::SLEEP),
    (EXIT, imports::core::MODULE, imports::core::EXIT),
    (
        INSTANCE_ID,
        imports::core::MODULE,
        imports::core::INSTANCE_ID,
    ),
    (
        CHANNEL_CREATE,
        imports::ipc::MODULE,
        imports::ipc::CHANNEL_CREATE,
    ),
    (SEND, imports::ipc::MODULE, imports::ipc::SEND),
    (RECV, imports::ipc::MODULE, imports::ipc::RECV),
    (TRY_RECV, imports::ipc::MODULE, imports::ipc::TRY_RECV),
    (WAIT_ANY, imports::ipc::MODULE, imports::ipc::WAIT_ANY),
    (
        MEMOBJ_CREATE,
        imports::memory::MODULE,
        imports::memory::MEMOBJ_CREATE,
    ),
    (
        MEMOBJ_SIZE,
        imports::memory::MODULE,
        imports::memory::MEMOBJ_SIZE,
    ),
    (
        MEMOBJ_READ,
        imports::memory::MODULE,
        imports::memory::MEMOBJ_READ,
    ),
    (
        MEMOBJ_WRITE,
        imports::memory::MODULE,
        imports::memory::MEMOBJ_WRITE,
    ),
    (SPAWN, imports::spawn::MODULE, imports::spawn::SPAWN),
    (
        INSTANCE_WAIT,
        imports::spawn::MODULE,
        imports::spawn::INSTANCE_WAIT,
    ),
    (
        INSTANCE_KILL,
        imports::spawn::MODULE,
        imports::spawn::INSTANCE_KILL,
    ),
    (
        MODULE_COUNT,
        imports::spawn::MODULE,
        imports::spawn::MODULE_COUNT,
    ),
    (
        INSTANCE_SNAPSHOT,
        imports::spawn::MODULE,
        imports::spawn::INSTANCE_SNAPSHOT,
    ),
    (
        SPAWN_SNAPSHOT,
        imports::spawn::MODULE,
        imports::spawn::SPAWN_SNAPSHOT,
    ),
];

/// Verifies that the id constants agree with `kalium-abi`'s import table.
pub fn check_table() -> Result<(), &'static str> {
    if imports::ALL_IMPORTS.len() != TABLE.len() {
        return Err("host function table length differs from kalium-abi");
    }
    for (id, module, name) in TABLE {
        if imports::lookup(module, name) != Some(id as usize) {
            return Err("host function id differs from kalium-abi");
        }
        if signature(id).is_none() {
            return Err("host function without a signature");
        }
        if crate::wasi::is_wasi(HostFuncId(id)) {
            return Err("host function id collides with the WASI range");
        }
    }
    crate::wasi::check_table()
}

/// The function type of host function `id`, from the plan's ABI tables.
pub fn signature(id: u32) -> Option<FuncType> {
    use ValType::{I32, I64};
    let (params, results): (&[ValType], &[ValType]) = match id {
        CONSOLE_WRITE => (&[I32, I32, I32], &[I32]),
        CAP_DROP => (&[I32], &[I32]),
        CAP_DERIVE => (&[I32, I64, I32], &[I32]),
        CAP_REVOKE => (&[I32], &[I32]),
        CAP_INFO => (&[I32, I32], &[I32]),
        TIME_NOW => (&[], &[I64]),
        YIELD => (&[], &[]),
        SLEEP => (&[I64], &[]),
        EXIT => (&[I32], &[]),
        INSTANCE_ID => (&[], &[I32]),
        CHANNEL_CREATE => (&[I32, I32], &[I32]),
        SEND => (&[I32, I32, I32, I32, I32], &[I32]),
        RECV | TRY_RECV => (&[I32, I32, I32, I32, I32, I32], &[I32]),
        WAIT_ANY => (&[I32, I32, I32], &[I32]),
        MEMOBJ_CREATE => (&[I64, I32], &[I32]),
        MEMOBJ_SIZE => (&[I32, I32], &[I32]),
        MEMOBJ_READ | MEMOBJ_WRITE => (&[I32, I64, I32, I32], &[I32]),
        SPAWN => (&[I32, I32, I32, I32, I32, I64, I64, I32], &[I32]),
        INSTANCE_WAIT => (&[I32, I32], &[I32]),
        INSTANCE_KILL => (&[I32], &[I32]),
        MODULE_COUNT => (&[], &[I32]),
        INSTANCE_SNAPSHOT => (&[I32, I32], &[I32]),
        SPAWN_SNAPSHOT => (&[I32, I32, I32, I32, I32, I64, I64, I32, I32], &[I32]),
        _ => return None,
    };
    Some(FuncType::new(params.to_vec(), results.to_vec()))
}

/// Import resolver for `Instance::new`: the import must be in the ABI
/// table with the expected type, and `kalium:spawn` resolves only when the
/// instance holds a `Spawn` capability. `wasi_snapshot_preview1` is handed
/// to [`crate::wasi`] and always resolves.
pub fn resolve(module: &str, name: &str, ty: &FuncType, has_spawn: bool) -> Result<HostFuncId, ()> {
    if module == crate::wasi::MODULE {
        return crate::wasi::resolve(name, ty);
    }
    let id = imports::lookup(module, name).ok_or(())?;
    if module == imports::spawn::MODULE && !has_spawn {
        return Err(());
    }
    let expected = signature(id as u32).ok_or(())?;
    if *ty != expected {
        return Err(());
    }
    Ok(HostFuncId(id as u32))
}

/// What a `Block` outcome means for scheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    /// `yield_`: the thread stays runnable.
    Yielded,
    /// `sleep`: the thread sleeps until its deadline.
    Slept,
    /// `recv`/`wait_any`/`instance_wait`: the thread is parked.
    Blocked,
}

/// One instance's view of the kernel during a host call.
pub struct HostCall<'a> {
    pub k: &'a mut Kernel,
    /// The calling instance.
    pub cur: usize,
    pub effect: Effect,
}

/// The result of an errno-returning host function.
pub(crate) enum Step {
    Ret(i32),
    Block,
    Exit(i32),
}

type R = Result<Step, i32>;

const OK: R = Ok(Step::Ret(errno::OK));

fn cap_errno(e: CapError) -> i32 {
    e.errno()
}

fn mem_errno(e: MemError) -> i32 {
    match e {
        MemError::TooBig => errno::ETOOBIG,
        MemError::OutOfMemory => errno::ENOMEM,
        MemError::OutOfBounds => errno::EINVAL,
    }
}

pub(crate) fn cap_index(cap: i32) -> Result<CapIndex, i32> {
    CapIndex::try_from(cap).map_err(cap_errno)
}

/// `[ptr, ptr + len)` inside `mem`, or `EFAULT`. An empty range is accepted
/// whatever the pointer.
pub(crate) fn range(mem: &Memory, ptr: i32, len: i32) -> Result<(u32, usize), i32> {
    let len = usize::try_from(len).map_err(|_| errno::EFAULT)?;
    let ptr = ptr as u32;
    if len == 0 {
        return Ok((ptr, 0));
    }
    match (ptr as usize).checked_add(len) {
        Some(end) if end <= mem.size_bytes() => Ok((ptr, len)),
        _ => Err(errno::EFAULT),
    }
}

/// `count` elements of `size` bytes at `ptr`; a negative count is `EINVAL`.
pub(crate) fn range_n(
    mem: &Memory,
    ptr: i32,
    count: i32,
    size: usize,
) -> Result<(u32, usize), i32> {
    let count = usize::try_from(count).map_err(|_| errno::EINVAL)?;
    let bytes = count.checked_mul(size).ok_or(errno::EFAULT)?;
    let bytes = i32::try_from(bytes).map_err(|_| errno::EFAULT)?;
    range(mem, ptr, bytes)
}

/// An empty vector with room for `n` elements, or `ENOMEM`: the host
/// never lets an allocation sized by an instance's arguments abort the
/// kernel.
pub(crate) fn try_vec<T>(n: usize) -> Result<Vec<T>, i32> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| errno::ENOMEM)?;
    Ok(v)
}

/// A copy of `bytes`, or `ENOMEM`.
pub(crate) fn try_copy(bytes: &[u8]) -> Result<Vec<u8>, i32> {
    let mut v = try_vec(bytes.len())?;
    v.extend_from_slice(bytes);
    Ok(v)
}

fn read_i32s(mem: &Memory, ptr: i32, count: i32) -> Result<Vec<i32>, i32> {
    let (p, len) = range_n(mem, ptr, count, 4)?;
    if len == 0 {
        return Ok(Vec::new());
    }
    let bytes = mem.slice(p, len).map_err(|_| errno::EFAULT)?;
    let mut v = try_vec(len / 4)?;
    v.extend(
        bytes
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]])),
    );
    Ok(v)
}

pub(crate) fn write_bytes(mem: &mut Memory, ptr: u32, bytes: &[u8]) -> Result<(), i32> {
    if bytes.is_empty() {
        return Ok(());
    }
    mem.write(ptr, bytes).map_err(|_| errno::EFAULT)
}

fn write_i32(mem: &mut Memory, ptr: u32, v: i32) -> Result<(), i32> {
    write_bytes(mem, ptr, &v.to_le_bytes())
}

fn write_i64(mem: &mut Memory, ptr: u32, v: i64) -> Result<(), i32> {
    write_bytes(mem, ptr, &v.to_le_bytes())
}

/// Puts capabilities taken out of `table` back, last taken first, so each
/// regains the slot it came from (the free list is LIFO).
fn restore(table: &mut CapTable, caps: Vec<Capability>) {
    for cap in caps.into_iter().rev() {
        let _ = table.insert(cap);
    }
}

pub(crate) struct Args<'a>(pub(crate) &'a [Value]);

impl Args<'_> {
    pub(crate) fn i32(&self, i: usize) -> Result<i32, Trap> {
        self.0.get(i).and_then(Value::as_i32).ok_or(Trap::Host)
    }

    pub(crate) fn i64(&self, i: usize) -> Result<i64, Trap> {
        self.0.get(i).and_then(Value::as_i64).ok_or(Trap::Host)
    }
}

impl<'a> HostCall<'a> {
    pub fn new(k: &'a mut Kernel, cur: usize) -> HostCall<'a> {
        HostCall {
            k,
            cur,
            effect: Effect::None,
        }
    }

    pub(crate) fn tid(&self) -> ThreadId {
        ThreadId(self.cur as u32)
    }

    pub(crate) fn caps(&mut self) -> &mut CapTable {
        &mut self.k.instances[self.cur].caps
    }

    /// The capability in `idx`: `EBADCAP` unless the slot is live and the
    /// capability has not been revoked.
    pub(crate) fn get_cap(&self, idx: CapIndex) -> Result<Capability, i32> {
        let cap = *self.k.instances[self.cur]
            .caps
            .get(idx)
            .map_err(cap_errno)?;
        if self.k.is_valid(cap) {
            Ok(cap)
        } else {
            Err(errno::EBADCAP)
        }
    }

    /// [`get_cap`](HostCall::get_cap), then kind and rights (`EPERM`).
    pub(crate) fn check_cap(
        &self,
        idx: CapIndex,
        kind: ObjectKind,
        rights: Rights,
    ) -> Result<Capability, i32> {
        let cap = self.get_cap(idx)?;
        cap.check(kind, rights).map_err(cap_errno)?;
        Ok(cap)
    }

    /// [`get_cap`](HostCall::get_cap), then rights of any kind (`EPERM`).
    pub(crate) fn check_cap_rights(
        &self,
        idx: CapIndex,
        rights: Rights,
    ) -> Result<Capability, i32> {
        let cap = self.get_cap(idx)?;
        if cap.has(rights) {
            Ok(cap)
        } else {
            Err(errno::EPERM)
        }
    }

    pub(crate) fn block_on(&mut self, objects: &[kalium_cap::ObjectId]) -> Step {
        let tid = self.tid();
        for &o in objects {
            self.k.park(tid, o);
        }
        self.k.sched.block(tid);
        self.effect = Effect::Blocked;
        Step::Block
    }

    // -- kalium:core -----------------------------------------------------------

    fn console_write(&mut self, mem: &mut Memory, cap: i32, ptr: i32, len: i32) -> R {
        let idx = cap_index(cap)?;
        self.check_cap(idx, ObjectKind::Console, Rights::WRITE)?;
        let (p, l) = range(mem, ptr, len)?;
        if l > 0 {
            let bytes = mem.slice(p, l).map_err(|_| errno::EFAULT)?;
            console::with_console(|_| console::write_bytes(bytes));
        }
        OK
    }

    fn cap_drop(&mut self, cap: i32) -> R {
        let idx = cap_index(cap)?;
        let now = timer::now_ns();
        self.k.destroy_slot(self.cur, idx, now).map_err(cap_errno)?;
        OK
    }

    fn cap_derive(&mut self, mem: &mut Memory, cap: i32, rights: i64, out: i32) -> R {
        let (op, _) = range(mem, out, 4)?;
        let idx = cap_index(cap)?;
        let rights = Rights::from_bits(rights as u64).ok_or(errno::EINVAL)?;
        let new = self
            .k
            .derive_into(self.cur, idx, rights)
            .map_err(cap_errno)?;
        write_i32(mem, op, new.as_i32())?;
        OK
    }

    /// `cap_revoke`: see `kernel.rs`, "Lineages and revocation". Needs a
    /// live, unrevoked slot and no right; the slot stays valid. A lineage
    /// error (`ENOMEM` once the node's generation would wrap) is reported
    /// as such, like `cap_derive`'s depth limit, since nothing was
    /// invalidated then.
    fn cap_revoke(&mut self, cap: i32) -> R {
        let idx = cap_index(cap)?;
        let target = self.get_cap(idx)?;
        let now = timer::now_ns();
        let n = self.k.revoke(target, now).map_err(cap_errno)?;
        println!(
            "kalium: instance {} ({}) revoked slot {}: {} derived capabilit{} invalidated",
            self.cur,
            self.k.instances[self.cur].name,
            idx.as_i32(),
            n,
            if n == 1 { "y" } else { "ies" }
        );
        OK
    }

    fn cap_info(&mut self, mem: &mut Memory, cap: i32, out: i32) -> R {
        let (op, _) = range(mem, out, kalium_abi::layout::CAP_INFO_SIZE as i32)?;
        let idx = cap_index(cap)?;
        let info = self.get_cap(idx)?.info();
        write_bytes(mem, op, &info.to_le_bytes())?;
        OK
    }

    pub(crate) fn yield_(&mut self) -> HostOutcome {
        let inst = &mut self.k.instances[self.cur];
        if inst.yield_pending {
            inst.yield_pending = false;
            HostOutcome::Continue
        } else {
            inst.yield_pending = true;
            self.effect = Effect::Yielded;
            HostOutcome::Block
        }
    }

    fn sleep(&mut self, ns: i64) -> HostOutcome {
        let now = timer::now_ns();
        let tid = self.tid();
        let pending = self.k.instances[self.cur].sleep_until;
        let until = match pending {
            Some(until) if now >= until => {
                self.k.instances[self.cur].sleep_until = None;
                return HostOutcome::Continue;
            }
            Some(until) => until,
            None => {
                if ns <= 0 {
                    return self.yield_();
                }
                let until = now.saturating_add(ns as u64);
                self.k.instances[self.cur].sleep_until = Some(until);
                until
            }
        };
        self.k.sched.sleep(tid, until);
        self.effect = Effect::Slept;
        HostOutcome::Block
    }

    // -- kalium:ipc ------------------------------------------------------------

    /// `channel_create`: the channel counts against the caller's channel
    /// quota (`MAX_CHANNELS_PER_INSTANCE`, `ENOMEM`) until it is freed or
    /// the caller exits.
    fn channel_create(&mut self, mem: &mut Memory, out_send: i32, out_recv: i32) -> R {
        let (sp, _) = range(mem, out_send, 4)?;
        let (rp, _) = range(mem, out_recv, 4)?;
        if self.caps().len() + 2 > self.caps().limit() {
            return Err(errno::ENOMEM);
        }
        self.k.charge(self.cur, Quota::Channel, 0)?;
        // Pinned while the two capabilities are minted, so that a failure
        // half-way frees the object (and its charge) through the unpin.
        let id = self.k.new_owned_object(
            Object::Channel {
                channel: Channel::with_abi_limits(),
                waiters: WaitQueue::new(),
            },
            1,
            self.cur,
            0,
        );
        let now = timer::now_ns();
        let send = Rights::from_bits_truncate(rights::CHANNEL_SEND_END);
        let recv = Rights::from_bits_truncate(rights::CHANNEL_RECV_END);
        let si = match self.k.mint_root(self.cur, id, ObjectKind::Channel, send) {
            Ok(i) => i,
            Err(e) => {
                self.k.unpin(id, now);
                return Err(cap_errno(e));
            }
        };
        let ri = match self.k.mint_root(self.cur, id, ObjectKind::Channel, recv) {
            Ok(i) => i,
            Err(e) => {
                let _ = self.k.destroy_slot(self.cur, si, now);
                self.k.unpin(id, now);
                return Err(cap_errno(e));
            }
        };
        self.k.unpin(id, now);
        write_i32(mem, sp, si.as_i32())?;
        write_i32(mem, rp, ri.as_i32())?;
        OK
    }

    fn send(&mut self, mem: &mut Memory, cap: i32, msg: i32, len: i32, caps: i32, ncaps: i32) -> R {
        let idx = cap_index(cap)?;
        let chan = self
            .check_cap(idx, ObjectKind::Channel, Rights::SEND)?
            .object;
        let (mp, ml) = range(mem, msg, len)?;
        range_n(mem, caps, ncaps, 4)?;
        // More capabilities than a message may carry: refused before the
        // list is read (and allocated), with the error `can_send` gives.
        if ncaps as usize > kalium_abi::limits::MAX_MESSAGE_CAPS {
            return Err(errno::ETOOBIG);
        }
        let list = read_i32s(mem, caps, ncaps)?;
        let mut idxs = try_vec(list.len())?;
        for c in list {
            let i = cap_index(c)?;
            self.check_cap_rights(i, Rights::TRANSFER)?;
            idxs.push(i);
        }
        self.k
            .channel_mut(chan)
            .ok_or(errno::EBADCAP)?
            .can_send(ml, idxs.len())
            .map_err(|e| e.errno())?;
        // The kernel's copy of the message (its queue slot, the payload and
        // the capabilities it carries: `Message::cost_of`) is charged to
        // the kernel-wide budget until it is received (or the channel is
        // destroyed with it still queued). A message with no payload but
        // capabilities is not free.
        let cost = Message::cost_of(ml, idxs.len());
        if !self.k.budget.reserve(cost) {
            return Err(errno::ENOMEM);
        }
        let bytes = match if ml > 0 {
            mem.slice(mp, ml)
                .map_err(|_| errno::EFAULT)
                .and_then(try_copy)
        } else {
            Ok(Vec::new())
        } {
            Ok(b) => b,
            Err(e) => {
                self.k.budget.release(cost);
                return Err(e);
            }
        };
        let mut moved = match try_vec(idxs.len()) {
            Ok(v) => v,
            Err(e) => {
                self.k.budget.release(cost);
                return Err(e);
            }
        };
        for &i in &idxs {
            match self.caps().take(i) {
                Ok(c) => moved.push(c),
                Err(e) => {
                    restore(self.caps(), moved);
                    self.k.budget.release(cost);
                    return Err(cap_errno(e));
                }
            }
        }
        let now = timer::now_ns();
        self.k.channel_mut(chan).ok_or(errno::EBADCAP)?;
        match self.k.send_on(chan, Message::with_caps(bytes, moved)) {
            Ok(()) => {
                self.k.wake_one_on(chan, now);
                OK
            }
            Err((e, msg)) => {
                restore(self.caps(), msg.caps);
                self.k.budget.release(cost);
                Err(e.errno())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn recv(
        &mut self,
        mem: &mut Memory,
        cap: i32,
        buf: i32,
        buf_len: i32,
        caps: i32,
        ncaps_max: i32,
        out_hdr: i32,
        blocking: bool,
    ) -> R {
        let idx = cap_index(cap)?;
        let chan = self
            .check_cap(idx, ObjectKind::Channel, Rights::RECV)?
            .object;
        let (bp, bl) = range(mem, buf, buf_len)?;
        let (cp, _) = range_n(mem, caps, ncaps_max, 4)?;
        let (hp, _) = range(mem, out_hdr, kalium_abi::layout::RECV_HEADER_SIZE as i32)?;
        self.k.channel_mut(chan).ok_or(errno::EBADCAP)?;
        let msg = match self.k.recv_from(chan) {
            Ok(m) => m,
            Err(IpcError::Empty) if blocking => return Ok(self.block_on(&[chan])),
            Err(e) => return Err(e.errno()),
        };
        // Charged at `send`: the queue slot, the payload and the caps.
        self.k.budget.release(msg.cost());
        let Message { bytes, caps: mcaps } = msg;
        let n = bytes.len().min(bl);
        write_bytes(mem, bp, &bytes[..n])?;
        let total = mcaps.len();
        let now = timer::now_ns();
        for (i, c) in mcaps.into_iter().enumerate() {
            let slot = if i < ncaps_max as usize {
                self.caps().insert(c).ok()
            } else {
                None
            };
            match slot {
                Some(ix) => {
                    let at = cp.checked_add((i * 4) as u32).ok_or(errno::EFAULT)?;
                    write_i32(mem, at, ix.as_i32())?;
                }
                // Does not fit the receiver's buffer or table: the
                // capability is lost, as truncated bytes are.
                None => self.k.release(c, now),
            }
        }
        let hdr = RecvHeader {
            len: bytes.len() as u32,
            ncaps: total as u32,
        };
        write_bytes(mem, hp, &hdr.to_le_bytes())?;
        OK
    }

    fn wait_any(&mut self, mem: &mut Memory, caps: i32, n: i32, out_index: i32) -> R {
        if n <= 0 {
            return Err(errno::EINVAL);
        }
        range_n(mem, caps, n, 4)?;
        let (op, _) = range(mem, out_index, 4)?;
        // More entries than the table has slots is a bad argument, and
        // keeps the list the call reads bounded.
        if n as usize > CAP_TABLE_LIMIT {
            return Err(errno::EINVAL);
        }
        let list = read_i32s(mem, caps, n)?;
        let mut ids = try_vec(list.len())?;
        for c in list {
            let i = cap_index(c)?;
            let id = self.check_cap(i, ObjectKind::Channel, Rights::WAIT)?.object;
            ids.push(id);
        }
        for (i, &id) in ids.iter().enumerate() {
            let readable = matches!(
                self.k.object(id),
                Some(Object::Channel { channel, .. }) if channel.is_readable()
            );
            if readable {
                write_i32(mem, op, i as i32)?;
                return OK;
            }
        }
        Ok(self.block_on(&ids))
    }

    // -- kalium:memory ---------------------------------------------------------

    fn memobj_create(&mut self, mem: &mut Memory, size: i64, out: i32) -> R {
        let (op, _) = range(mem, out, 4)?;
        let size = u64::try_from(size).map_err(|_| errno::EINVAL)?;
        if size > kalium_mm::MAX_MEMOBJ_BYTES {
            return Err(errno::ETOOBIG);
        }
        if self.caps().len() >= self.caps().limit() {
            return Err(errno::ENOMEM);
        }
        // The object counts against the caller's memory object quota
        // (`MAX_MEMOBJ_BYTES_PER_INSTANCE`) and the kernel-wide budget,
        // both checked before the bytes are allocated.
        let bytes = usize::try_from(size).map_err(|_| errno::ETOOBIG)?;
        self.k.charge(self.cur, Quota::MemObj, bytes)?;
        let obj = match MemObject::new(size) {
            Ok(o) => o,
            Err(e) => {
                self.k.refund(self.cur, Quota::MemObj, bytes);
                return Err(mem_errno(e));
            }
        };
        let now = timer::now_ns();
        let id = self
            .k
            .new_owned_object(Object::MemObj(obj), 1, self.cur, bytes);
        let minted = self.k.mint_root(
            self.cur,
            id,
            ObjectKind::MemObj,
            Rights::from_bits_truncate(rights::MEMOBJ_OWNER),
        );
        // The pin kept the object through minting; without a capability
        // it is freed (and its charge released) here.
        self.k.unpin(id, now);
        let ix = minted.map_err(cap_errno)?;
        write_i32(mem, op, ix.as_i32())?;
        OK
    }

    fn memobj_size(&mut self, mem: &mut Memory, cap: i32, out_size: i32) -> R {
        let (op, _) = range(mem, out_size, 8)?;
        let idx = cap_index(cap)?;
        let id = self
            .check_cap(idx, ObjectKind::MemObj, Rights::NONE)?
            .object;
        let size = self.k.memobj(id).ok_or(errno::EBADCAP)?.size();
        write_i64(mem, op, size as i64)?;
        OK
    }

    fn memobj_read(&mut self, mem: &mut Memory, cap: i32, offset: i64, ptr: i32, len: i32) -> R {
        let idx = cap_index(cap)?;
        let id = self
            .check_cap(idx, ObjectKind::MemObj, Rights::READ)?
            .object;
        let (p, l) = range(mem, ptr, len)?;
        let offset = u64::try_from(offset).map_err(|_| errno::EINVAL)?;
        if l == 0 {
            return OK;
        }
        let src = self
            .k
            .memobj(id)
            .ok_or(errno::EBADCAP)?
            .slice(offset, l)
            .map_err(mem_errno)?;
        mem.write(p, src).map_err(|_| errno::EFAULT)?;
        OK
    }

    fn memobj_write(&mut self, mem: &mut Memory, cap: i32, offset: i64, ptr: i32, len: i32) -> R {
        let idx = cap_index(cap)?;
        let id = self
            .check_cap(idx, ObjectKind::MemObj, Rights::WRITE)?
            .object;
        let (p, l) = range(mem, ptr, len)?;
        let offset = u64::try_from(offset).map_err(|_| errno::EINVAL)?;
        if l == 0 {
            return OK;
        }
        let data = mem.slice(p, l).map_err(|_| errno::EFAULT)?;
        self.k
            .memobj_mut(id)
            .ok_or(errno::EBADCAP)?
            .write(offset, data)
            .map_err(mem_errno)?;
        OK
    }

    // -- kalium:spawn ----------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &mut self,
        mem: &mut Memory,
        cap: i32,
        module: i32,
        caps: i32,
        ncaps: i32,
        class: i32,
        period_ns: i64,
        budget_ns: i64,
        out: i32,
    ) -> R {
        let idx = cap_index(cap)?;
        self.check_cap(idx, ObjectKind::Spawn, Rights::SPAWN)?;
        let module = usize::try_from(module)
            .ok()
            .filter(|&m| m < self.k.modules.len())
            .ok_or(errno::ENOENT)?;
        self.spawn_from(
            mem, module, None, caps, ncaps, class, period_ns, budget_ns, out,
        )
    }

    /// `instance_snapshot`: see `kernel.rs`, "Snapshots". The instance
    /// must be parked (blocked or sleeping): `EBUSY` while it is runnable,
    /// is the caller, or has an object the snapshot cannot take (shared,
    /// or borrowed by a host call); `ECLOSED` once it has exited.
    fn instance_snapshot(&mut self, mem: &mut Memory, cap: i32, out: i32) -> R {
        let (op, _) = range(mem, out, 4)?;
        let idx = cap_index(cap)?;
        let id = self
            .check_cap(idx, ObjectKind::Instance, Rights::SNAPSHOT)?
            .object;
        if self.caps().len() >= self.caps().limit() {
            return Err(errno::ENOMEM);
        }
        let target = match self.k.object(id) {
            Some(Object::Instance { exit: Some(_), .. }) => return Err(errno::ECLOSED),
            Some(Object::Instance {
                instance,
                exit: None,
                ..
            }) => *instance,
            _ => return Err(errno::EBADCAP),
        };
        if target == self.cur {
            return Err(errno::EBUSY);
        }
        match self.k.sched.state(ThreadId(target as u32)) {
            Some(State::Blocked) | Some(State::Sleeping { .. }) => {}
            _ => return Err(errno::EBUSY),
        }
        // The snapshot counts against the caller's snapshot quota
        // (`MAX_SNAPSHOT_BYTES_PER_INSTANCE`) and the kernel-wide budget:
        // checked against the memory size before the copy is made, and
        // charged exactly once the encoded size is known.
        let estimate = self.k.memory_size_of(target).ok_or(errno::EBUSY)?;
        if !self.k.can_charge(self.cur, Quota::Snapshot, estimate) {
            return Err(errno::ENOMEM);
        }
        let snapshot = match self.k.snapshot_instance(target) {
            Some(Ok(s)) => s,
            Some(Err(WasmError::OutOfMemory)) => return Err(errno::ENOMEM),
            // Shared or borrowed objects (`Unsupported`, `Trap::Host`), or
            // no Wasm state at all.
            Some(Err(_)) | None => return Err(errno::EBUSY),
        };
        let module = self.k.instances[target].module;
        let (pages, bytes) = (snapshot.memory_pages(), snapshot.encoded_len());
        self.k.charge(self.cur, Quota::Snapshot, bytes)?;
        let now = timer::now_ns();
        let sid = self.k.new_owned_object(
            Object::Snapshot {
                module,
                snapshot: Arc::new(snapshot),
            },
            1,
            self.cur,
            bytes,
        );
        let minted = self.k.mint_root(
            self.cur,
            sid,
            ObjectKind::Snapshot,
            Rights::from_bits_truncate(rights::SNAPSHOT_OWNER),
        );
        // The pin kept the object through minting; without a capability it
        // is freed here.
        self.k.unpin(sid, now);
        let ix = minted.map_err(cap_errno)?;
        write_i32(mem, op, ix.as_i32())?;
        println!(
            "kalium: instance {} ({}) snapshotted instance {} ({}): {} pages, {} bytes",
            self.cur,
            self.k.instances[self.cur].name,
            target,
            self.k.instances[target].name,
            pages,
            bytes
        );
        OK
    }

    /// `spawn_snapshot`: `spawn` from a `Snapshot` capability (`READ`),
    /// starting at `entry` (`kalium_abi::entry`).
    #[allow(clippy::too_many_arguments)]
    fn spawn_snapshot(
        &mut self,
        mem: &mut Memory,
        cap: i32,
        snapshot_cap: i32,
        caps: i32,
        ncaps: i32,
        class: i32,
        period_ns: i64,
        budget_ns: i64,
        entry: i32,
        out: i32,
    ) -> R {
        let idx = cap_index(cap)?;
        self.check_cap(idx, ObjectKind::Spawn, Rights::SPAWN)?;
        let sidx = cap_index(snapshot_cap)?;
        let sid = self
            .check_cap(sidx, ObjectKind::Snapshot, Rights::READ)?
            .object;
        let entry = match entry {
            abi_entry::START => abi_entry::START_EXPORT,
            abi_entry::RESUME => abi_entry::RESUME_EXPORT,
            _ => return Err(errno::EINVAL),
        };
        let (module, snapshot) = match self.k.object(sid) {
            Some(Object::Snapshot { module, snapshot }) => (*module, snapshot.clone()),
            _ => return Err(errno::EBADCAP),
        };
        self.spawn_from(
            mem,
            module,
            Some((&snapshot, entry)),
            caps,
            ncaps,
            class,
            period_ns,
            budget_ns,
            out,
        )
    }

    /// The common part of `spawn` and `spawn_snapshot`, after the `Spawn`
    /// capability and the module are known.
    #[allow(clippy::too_many_arguments)]
    fn spawn_from(
        &mut self,
        mem: &mut Memory,
        module: usize,
        source: Option<(&Snapshot, &'static str)>,
        caps: i32,
        ncaps: i32,
        class: i32,
        period_ns: i64,
        budget_ns: i64,
        out: i32,
    ) -> R {
        let (op, _) = range(mem, out, 4)?;
        range_n(mem, caps, ncaps, 4)?;
        // More capabilities than the child's table could hold: refused
        // before the list is read (and allocated), with the error the
        // child's table would give.
        if ncaps as usize > CAP_TABLE_LIMIT {
            return Err(errno::ENOMEM);
        }
        let list = read_i32s(mem, caps, ncaps)?;
        let class = match class {
            abi_sched::CLASS_FAIR => Class::Fair { weight: 1 },
            abi_sched::CLASS_REAL_TIME => Class::RealTime {
                period_ns: u64::try_from(period_ns).map_err(|_| errno::EINVAL)?,
                budget_ns: u64::try_from(budget_ns).map_err(|_| errno::EINVAL)?,
            },
            _ => return Err(errno::EINVAL),
        };
        if !class.is_valid() {
            return Err(errno::EINVAL);
        }
        let mut idxs = try_vec(list.len())?;
        for c in list {
            let i = cap_index(c)?;
            self.check_cap_rights(i, Rights::TRANSFER)?;
            idxs.push(i);
        }
        // Room for the Instance capability once the transferred ones are gone.
        if self.caps().len().saturating_sub(idxs.len()) >= self.caps().limit() {
            return Err(errno::ENOMEM);
        }
        let mut moved = try_vec(idxs.len())?;
        for &i in &idxs {
            match self.caps().take(i) {
                Ok(c) => moved.push(c),
                Err(e) => {
                    restore(self.caps(), moved);
                    return Err(cap_errno(e));
                }
            }
        }
        let mut child = CapTable::new(CAP_TABLE_LIMIT);
        for &c in &moved {
            if child.insert(c).is_err() {
                restore(self.caps(), moved);
                return Err(errno::ENOMEM);
            }
        }
        let now = timer::now_ns();
        let created = match source {
            None => self.k.create_instance(module, child, class, now),
            Some((snapshot, entry)) => self
                .k
                .restore_instance(module, snapshot, entry, child, class, now),
        };
        let id = match created {
            Ok(id) => id,
            Err((e, mut child)) => {
                // Back to the caller, last taken first so that each
                // regains its slot (as `restore` does): the child's slots
                // `0..n` hold them in the order they were taken.
                for i in (0..child.len()).rev() {
                    if let Ok(c) = CapIndex::try_from(i).and_then(|i| child.take(i)) {
                        let _ = self.caps().insert(c);
                    }
                }
                return Err(e.errno());
            }
        };
        let object = self.k.instances[id].object;
        let ix = match self.k.mint_root(
            self.cur,
            object,
            ObjectKind::Instance,
            Rights::from_bits_truncate(rights::INSTANCE_OWNER),
        ) {
            Ok(ix) => ix,
            Err(e) => {
                self.k.finish_instance(id, abi_exit::KILLED, now);
                return Err(cap_errno(e));
            }
        };
        write_i32(mem, op, ix.as_i32())?;
        let (parent, name) = (self.k.instances[self.cur].name, self.k.instances[id].name);
        let how = match source {
            None => "",
            Some(_) => " from snapshot",
        };
        match class {
            Class::Fair { .. } => println!(
                "kalium: instance {} ({}) spawned instance {} ({}, fair){}",
                self.cur, parent, id, name, how
            ),
            Class::RealTime {
                period_ns,
                budget_ns,
            } => println!(
                "kalium: instance {} ({}) spawned instance {} ({}, rt period {} us budget {} us){}",
                self.cur,
                parent,
                id,
                name,
                period_ns / 1000,
                budget_ns / 1000,
                how
            ),
        }
        OK
    }

    fn instance_wait(&mut self, mem: &mut Memory, cap: i32, out_code: i32) -> R {
        let idx = cap_index(cap)?;
        let id = self
            .check_cap(idx, ObjectKind::Instance, Rights::WAIT)?
            .object;
        let (op, _) = range(mem, out_code, 4)?;
        match self.k.object(id) {
            Some(Object::Instance {
                exit: Some(code), ..
            }) => {
                write_i32(mem, op, *code)?;
                OK
            }
            Some(Object::Instance { exit: None, .. }) => Ok(self.block_on(&[id])),
            _ => Err(errno::EBADCAP),
        }
    }

    fn instance_kill(&mut self, cap: i32) -> R {
        let idx = cap_index(cap)?;
        let id = self
            .check_cap(idx, ObjectKind::Instance, Rights::KILL)?
            .object;
        let target = match self.k.object(id) {
            // Already exited: its slot may belong to another instance by
            // now, so the object's own record decides, not the slot.
            Some(Object::Instance { exit: Some(_), .. }) => return OK,
            Some(Object::Instance {
                instance,
                exit: None,
                ..
            }) => *instance,
            _ => return Err(errno::EBADCAP),
        };
        if target == self.cur {
            // Killing oneself: the run loop finishes the instance.
            return Ok(Step::Exit(abi_exit::KILLED));
        }
        if self.k.instances[target].exited.is_none() {
            let now = timer::now_ns();
            self.k.finish_instance(target, abi_exit::KILLED, now);
        }
        OK
    }
}

impl Host for HostCall<'_> {
    fn call(
        &mut self,
        ctx: &mut HostCtx<'_>,
        func: HostFuncId,
        args: &[Value],
        results: &mut [Value],
    ) -> Result<HostOutcome, Trap> {
        // The interpreter has released its borrow of the memory for the
        // duration of the call (kalium-wasm's `HostCtx` contract).
        let mut mem = ctx.memory.borrow_mut();
        let mem: &mut Memory = &mut mem;
        if crate::wasi::is_wasi(func) {
            return crate::wasi::call(self, mem, func, args, results);
        }
        let a = Args(args);
        let step = match func.0 {
            CONSOLE_WRITE => self.console_write(mem, a.i32(0)?, a.i32(1)?, a.i32(2)?),
            CAP_DROP => self.cap_drop(a.i32(0)?),
            CAP_DERIVE => self.cap_derive(mem, a.i32(0)?, a.i64(1)?, a.i32(2)?),
            CAP_REVOKE => self.cap_revoke(a.i32(0)?),
            CAP_INFO => self.cap_info(mem, a.i32(0)?, a.i32(1)?),
            TIME_NOW => {
                set_result(results, Value::I64(timer::now_ns() as i64))?;
                return Ok(HostOutcome::Continue);
            }
            YIELD => return Ok(self.yield_()),
            SLEEP => return Ok(self.sleep(a.i64(0)?)),
            EXIT => return Ok(HostOutcome::Exit(a.i32(0)?)),
            INSTANCE_ID => {
                set_result(results, Value::I32(self.cur as i32))?;
                return Ok(HostOutcome::Continue);
            }
            CHANNEL_CREATE => self.channel_create(mem, a.i32(0)?, a.i32(1)?),
            SEND => self.send(mem, a.i32(0)?, a.i32(1)?, a.i32(2)?, a.i32(3)?, a.i32(4)?),
            RECV | TRY_RECV => self.recv(
                mem,
                a.i32(0)?,
                a.i32(1)?,
                a.i32(2)?,
                a.i32(3)?,
                a.i32(4)?,
                a.i32(5)?,
                func.0 == RECV,
            ),
            WAIT_ANY => self.wait_any(mem, a.i32(0)?, a.i32(1)?, a.i32(2)?),
            MEMOBJ_CREATE => self.memobj_create(mem, a.i64(0)?, a.i32(1)?),
            MEMOBJ_SIZE => self.memobj_size(mem, a.i32(0)?, a.i32(1)?),
            MEMOBJ_READ => self.memobj_read(mem, a.i32(0)?, a.i64(1)?, a.i32(2)?, a.i32(3)?),
            MEMOBJ_WRITE => self.memobj_write(mem, a.i32(0)?, a.i64(1)?, a.i32(2)?, a.i32(3)?),
            SPAWN => self.spawn(
                mem,
                a.i32(0)?,
                a.i32(1)?,
                a.i32(2)?,
                a.i32(3)?,
                a.i32(4)?,
                a.i64(5)?,
                a.i64(6)?,
                a.i32(7)?,
            ),
            INSTANCE_WAIT => self.instance_wait(mem, a.i32(0)?, a.i32(1)?),
            INSTANCE_KILL => self.instance_kill(a.i32(0)?),
            MODULE_COUNT => {
                set_result(results, Value::I32(self.k.modules.len() as i32))?;
                return Ok(HostOutcome::Continue);
            }
            INSTANCE_SNAPSHOT => self.instance_snapshot(mem, a.i32(0)?, a.i32(1)?),
            SPAWN_SNAPSHOT => self.spawn_snapshot(
                mem,
                a.i32(0)?,
                a.i32(1)?,
                a.i32(2)?,
                a.i32(3)?,
                a.i32(4)?,
                a.i64(5)?,
                a.i64(6)?,
                a.i32(7)?,
                a.i32(8)?,
            ),
            _ => return Err(Trap::Host),
        };
        match step {
            Ok(Step::Ret(code)) | Err(code) => {
                set_result(results, Value::I32(code))?;
                Ok(HostOutcome::Continue)
            }
            Ok(Step::Block) => Ok(HostOutcome::Block),
            Ok(Step::Exit(code)) => Ok(HostOutcome::Exit(code)),
        }
    }
}

pub(crate) fn set_result(results: &mut [Value], v: Value) -> Result<(), Trap> {
    match results.first_mut() {
        Some(slot) => {
            *slot = v;
            Ok(())
        }
        None => Err(Trap::Host),
    }
}
