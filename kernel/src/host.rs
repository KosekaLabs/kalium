//! The host ABI: one [`Host`] implementation that dispatches on
//! [`HostFuncId`] to every function of `kalium:core`, `kalium:ipc`,
//! `kalium:memory` and `kalium:spawn` with the semantics of
//! `docs/prototype-plan.md`.
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
//! - capability indices go through [`CapTable::check`], whose errors map
//!   to `EBADCAP`/`EPERM`;
//! - blocking functions return [`HostOutcome::Block`] after parking the
//!   thread; the interpreter rewinds to the call, so the call is re-issued
//!   when the thread runs again and re-checks everything. `yield_` and
//!   `sleep` use the same mechanism with a per-instance flag/deadline so
//!   that the re-issued call completes;
//! - the [`Effect`] tells the run loop how to account the slice when the
//!   outcome is `Block`.

use alloc::vec::Vec;

use kalium_abi::layout::RecvHeader;
use kalium_abi::{errno, exit as abi_exit, imports, rights, sched as abi_sched};
use kalium_cap::{CapError, CapIndex, CapTable, Capability, ObjectKind, Rights};
use kalium_hal::{console, println, timer};
use kalium_ipc::{Channel, IpcError, Message, WaitQueue};
use kalium_mm::{MemError, MemObject};
use kalium_sched::{Class, ThreadId};
use kalium_wasm::{FuncType, Host, HostCtx, HostFuncId, HostOutcome, Memory, Trap, ValType, Value};

use crate::kernel::{Kernel, Object, CAP_TABLE_LIMIT};

// Host function ids: positions in `kalium_abi::imports::ALL_IMPORTS`.
pub const CONSOLE_WRITE: u32 = 0;
pub const CAP_DROP: u32 = 1;
pub const CAP_DERIVE: u32 = 2;
pub const CAP_INFO: u32 = 3;
pub const TIME_NOW: u32 = 4;
pub const YIELD: u32 = 5;
pub const SLEEP: u32 = 6;
pub const EXIT: u32 = 7;
pub const INSTANCE_ID: u32 = 8;
pub const CHANNEL_CREATE: u32 = 9;
pub const SEND: u32 = 10;
pub const RECV: u32 = 11;
pub const TRY_RECV: u32 = 12;
pub const WAIT_ANY: u32 = 13;
pub const MEMOBJ_CREATE: u32 = 14;
pub const MEMOBJ_SIZE: u32 = 15;
pub const MEMOBJ_READ: u32 = 16;
pub const MEMOBJ_WRITE: u32 = 17;
pub const SPAWN: u32 = 18;
pub const INSTANCE_WAIT: u32 = 19;
pub const INSTANCE_KILL: u32 = 20;
pub const MODULE_COUNT: u32 = 21;

const TABLE: [(u32, &str, &str); 22] = [
    (
        CONSOLE_WRITE,
        imports::core::MODULE,
        imports::core::CONSOLE_WRITE,
    ),
    (CAP_DROP, imports::core::MODULE, imports::core::CAP_DROP),
    (CAP_DERIVE, imports::core::MODULE, imports::core::CAP_DERIVE),
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
    }
    Ok(())
}

/// The function type of host function `id`, from the plan's ABI tables.
pub fn signature(id: u32) -> Option<FuncType> {
    use ValType::{I32, I64};
    let (params, results): (&[ValType], &[ValType]) = match id {
        CONSOLE_WRITE => (&[I32, I32, I32], &[I32]),
        CAP_DROP => (&[I32], &[I32]),
        CAP_DERIVE => (&[I32, I64, I32], &[I32]),
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
        _ => return None,
    };
    Some(FuncType::new(params.to_vec(), results.to_vec()))
}

/// Import resolver for `Instance::new`: the import must be in the ABI
/// table with the expected type, and `kalium:spawn` resolves only when the
/// instance holds a `Spawn` capability.
pub fn resolve(module: &str, name: &str, ty: &FuncType, has_spawn: bool) -> Result<HostFuncId, ()> {
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
enum Step {
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

fn cap_index(cap: i32) -> Result<CapIndex, i32> {
    CapIndex::try_from(cap).map_err(cap_errno)
}

/// `[ptr, ptr + len)` inside `mem`, or `EFAULT`. An empty range is accepted
/// whatever the pointer.
fn range(mem: &Memory, ptr: i32, len: i32) -> Result<(u32, usize), i32> {
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
fn range_n(mem: &Memory, ptr: i32, count: i32, size: usize) -> Result<(u32, usize), i32> {
    let count = usize::try_from(count).map_err(|_| errno::EINVAL)?;
    let bytes = count.checked_mul(size).ok_or(errno::EFAULT)?;
    let bytes = i32::try_from(bytes).map_err(|_| errno::EFAULT)?;
    range(mem, ptr, bytes)
}

fn read_i32s(mem: &Memory, ptr: i32, count: i32) -> Result<Vec<i32>, i32> {
    let (p, len) = range_n(mem, ptr, count, 4)?;
    if len == 0 {
        return Ok(Vec::new());
    }
    let bytes = mem.slice(p, len).map_err(|_| errno::EFAULT)?;
    Ok(bytes
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

fn write_bytes(mem: &mut Memory, ptr: u32, bytes: &[u8]) -> Result<(), i32> {
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

struct Args<'a>(&'a [Value]);

impl Args<'_> {
    fn i32(&self, i: usize) -> Result<i32, Trap> {
        self.0.get(i).and_then(Value::as_i32).ok_or(Trap::Host)
    }

    fn i64(&self, i: usize) -> Result<i64, Trap> {
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

    fn tid(&self) -> ThreadId {
        ThreadId(self.cur as u32)
    }

    fn caps(&mut self) -> &mut CapTable {
        &mut self.k.instances[self.cur].caps
    }

    fn block_on(&mut self, objects: &[kalium_cap::ObjectId]) -> Step {
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
        self.caps()
            .check(idx, ObjectKind::Console, Rights::WRITE)
            .map_err(cap_errno)?;
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
        let derived = self
            .caps()
            .get(idx)
            .map_err(cap_errno)?
            .derive(rights)
            .map_err(cap_errno)?;
        let new = self.k.mint(self.cur, derived).map_err(cap_errno)?;
        write_i32(mem, op, new.as_i32())?;
        OK
    }

    fn cap_info(&mut self, mem: &mut Memory, cap: i32, out: i32) -> R {
        let (op, _) = range(mem, out, kalium_abi::layout::CAP_INFO_SIZE as i32)?;
        let idx = cap_index(cap)?;
        let info = self.caps().get(idx).map_err(cap_errno)?.info();
        write_bytes(mem, op, &info.to_le_bytes())?;
        OK
    }

    fn yield_(&mut self) -> HostOutcome {
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

    fn channel_create(&mut self, mem: &mut Memory, out_send: i32, out_recv: i32) -> R {
        let (sp, _) = range(mem, out_send, 4)?;
        let (rp, _) = range(mem, out_recv, 4)?;
        if self.caps().len() + 2 > self.caps().limit() {
            return Err(errno::ENOMEM);
        }
        let id = self.k.new_object(
            Object::Channel {
                channel: Channel::with_abi_limits(),
                waiters: WaitQueue::new(),
            },
            0,
        );
        let send = Capability::new(
            id,
            ObjectKind::Channel,
            Rights::from_bits_truncate(rights::CHANNEL_SEND_END),
        );
        let recv = Capability::new(
            id,
            ObjectKind::Channel,
            Rights::from_bits_truncate(rights::CHANNEL_RECV_END),
        );
        let si = self.k.mint(self.cur, send).map_err(cap_errno)?;
        let ri = match self.k.mint(self.cur, recv) {
            Ok(i) => i,
            Err(e) => {
                let now = timer::now_ns();
                let _ = self.k.destroy_slot(self.cur, si, now);
                return Err(cap_errno(e));
            }
        };
        write_i32(mem, sp, si.as_i32())?;
        write_i32(mem, rp, ri.as_i32())?;
        OK
    }

    fn send(&mut self, mem: &mut Memory, cap: i32, msg: i32, len: i32, caps: i32, ncaps: i32) -> R {
        let idx = cap_index(cap)?;
        let chan = self
            .caps()
            .check(idx, ObjectKind::Channel, Rights::SEND)
            .map_err(cap_errno)?
            .object;
        let (mp, ml) = range(mem, msg, len)?;
        let list = read_i32s(mem, caps, ncaps)?;
        let mut idxs = Vec::with_capacity(list.len());
        for c in list {
            let i = cap_index(c)?;
            self.caps()
                .check_rights(i, Rights::TRANSFER)
                .map_err(cap_errno)?;
            idxs.push(i);
        }
        self.k
            .channel_mut(chan)
            .ok_or(errno::EBADCAP)?
            .can_send(ml, idxs.len())
            .map_err(|e| e.errno())?;
        let bytes = if ml > 0 {
            mem.slice(mp, ml).map_err(|_| errno::EFAULT)?.to_vec()
        } else {
            Vec::new()
        };
        let mut moved = Vec::with_capacity(idxs.len());
        for &i in &idxs {
            match self.caps().take(i) {
                Ok(c) => moved.push(c),
                Err(e) => {
                    restore(self.caps(), moved);
                    return Err(cap_errno(e));
                }
            }
        }
        let now = timer::now_ns();
        let channel = self.k.channel_mut(chan).ok_or(errno::EBADCAP)?;
        match channel.send_or_return(Message::with_caps(bytes, moved)) {
            Ok(()) => {
                self.k.wake_one_on(chan, now);
                OK
            }
            Err((e, msg)) => {
                restore(self.caps(), msg.caps);
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
            .caps()
            .check(idx, ObjectKind::Channel, Rights::RECV)
            .map_err(cap_errno)?
            .object;
        let (bp, bl) = range(mem, buf, buf_len)?;
        let (cp, _) = range_n(mem, caps, ncaps_max, 4)?;
        let (hp, _) = range(mem, out_hdr, kalium_abi::layout::RECV_HEADER_SIZE as i32)?;
        let msg = match self.k.channel_mut(chan).ok_or(errno::EBADCAP)?.try_recv() {
            Ok(m) => m,
            Err(IpcError::Empty) if blocking => return Ok(self.block_on(&[chan])),
            Err(e) => return Err(e.errno()),
        };
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
        let list = read_i32s(mem, caps, n)?;
        let (op, _) = range(mem, out_index, 4)?;
        let mut ids = Vec::with_capacity(list.len());
        for c in list {
            let i = cap_index(c)?;
            let id = self
                .caps()
                .check(i, ObjectKind::Channel, Rights::WAIT)
                .map_err(cap_errno)?
                .object;
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
        if self.caps().len() >= self.caps().limit() {
            return Err(errno::ENOMEM);
        }
        let obj = MemObject::new(size).map_err(mem_errno)?;
        let id = self.k.new_object(Object::MemObj(obj), 0);
        let cap = Capability::new(
            id,
            ObjectKind::MemObj,
            Rights::from_bits_truncate(rights::MEMOBJ_OWNER),
        );
        let ix = self.k.mint(self.cur, cap).map_err(cap_errno)?;
        write_i32(mem, op, ix.as_i32())?;
        OK
    }

    fn memobj_size(&mut self, mem: &mut Memory, cap: i32, out_size: i32) -> R {
        let (op, _) = range(mem, out_size, 8)?;
        let idx = cap_index(cap)?;
        let id = self
            .caps()
            .check(idx, ObjectKind::MemObj, Rights::NONE)
            .map_err(cap_errno)?
            .object;
        let size = self.k.memobj(id).ok_or(errno::EBADCAP)?.size();
        write_i64(mem, op, size as i64)?;
        OK
    }

    fn memobj_read(&mut self, mem: &mut Memory, cap: i32, offset: i64, ptr: i32, len: i32) -> R {
        let idx = cap_index(cap)?;
        let id = self
            .caps()
            .check(idx, ObjectKind::MemObj, Rights::READ)
            .map_err(cap_errno)?
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
            .caps()
            .check(idx, ObjectKind::MemObj, Rights::WRITE)
            .map_err(cap_errno)?
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
        self.caps()
            .check(idx, ObjectKind::Spawn, Rights::SPAWN)
            .map_err(cap_errno)?;
        let (op, _) = range(mem, out, 4)?;
        let list = read_i32s(mem, caps, ncaps)?;
        let module = usize::try_from(module)
            .ok()
            .filter(|&m| m < self.k.modules.len())
            .ok_or(errno::ENOENT)?;
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
        let mut idxs = Vec::with_capacity(list.len());
        for c in list {
            let i = cap_index(c)?;
            self.caps()
                .check_rights(i, Rights::TRANSFER)
                .map_err(cap_errno)?;
            idxs.push(i);
        }
        // Room for the Instance capability once the transferred ones are gone.
        if self.caps().len().saturating_sub(idxs.len()) >= self.caps().limit() {
            return Err(errno::ENOMEM);
        }
        let mut moved = Vec::with_capacity(idxs.len());
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
        let id = match self.k.create_instance(module, child, class, now) {
            Ok(id) => id,
            Err((e, child)) => {
                let back: Vec<Capability> = child.iter().map(|(_, c)| *c).collect();
                restore(self.caps(), back);
                return Err(e.errno());
            }
        };
        let cap = Capability::new(
            self.k.instances[id].object,
            ObjectKind::Instance,
            Rights::from_bits_truncate(rights::INSTANCE_OWNER),
        );
        let ix = match self.k.mint(self.cur, cap) {
            Ok(ix) => ix,
            Err(e) => {
                self.k.finish_instance(id, abi_exit::KILLED, now);
                return Err(cap_errno(e));
            }
        };
        write_i32(mem, op, ix.as_i32())?;
        let (parent, name) = (self.k.instances[self.cur].name, self.k.instances[id].name);
        match class {
            Class::Fair { .. } => println!(
                "kalium: instance {} ({}) spawned instance {} ({}, fair)",
                self.cur, parent, id, name
            ),
            Class::RealTime {
                period_ns,
                budget_ns,
            } => println!(
                "kalium: instance {} ({}) spawned instance {} ({}, rt period {} us budget {} us)",
                self.cur,
                parent,
                id,
                name,
                period_ns / 1000,
                budget_ns / 1000
            ),
        }
        OK
    }

    fn instance_wait(&mut self, mem: &mut Memory, cap: i32, out_code: i32) -> R {
        let idx = cap_index(cap)?;
        let id = self
            .caps()
            .check(idx, ObjectKind::Instance, Rights::WAIT)
            .map_err(cap_errno)?
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
            .caps()
            .check(idx, ObjectKind::Instance, Rights::KILL)
            .map_err(cap_errno)?
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
        let a = Args(args);
        let step = match func.0 {
            CONSOLE_WRITE => self.console_write(mem, a.i32(0)?, a.i32(1)?, a.i32(2)?),
            CAP_DROP => self.cap_drop(a.i32(0)?),
            CAP_DERIVE => self.cap_derive(mem, a.i32(0)?, a.i64(1)?, a.i32(2)?),
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

fn set_result(results: &mut [Value], v: Value) -> Result<(), Trap> {
    match results.first_mut() {
        Some(slot) => {
            *slot = v;
            Ok(())
        }
        None => Err(Trap::Host),
    }
}
