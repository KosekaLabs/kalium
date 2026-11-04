//! The kernel's data model: the object registry, instances and the
//! bookkeeping that ties capabilities, channels, the scheduler and the Wasm
//! runtime together.
//!
//! Everything lives in one [`Kernel`] value owned by the run loop; host calls
//! borrow it mutably for their duration. M1 is single core and the only
//! interrupt handler (the timer tick) touches an atomic flag, so no lock is
//! needed around this state.
//!
//! # Objects and reference counts
//!
//! Every [`ObjectId`] names an [`Object`] in the registry. The registry keeps
//! a reference count per object: the number of capabilities that refer to
//! it, wherever they are (in a table or in flight inside a queued message),
//! plus one pin for objects that must outlive their capabilities (the
//! console, the spawner and every instance while it runs). A capability
//! comes into existence through [`Kernel::retain`] (`channel_create`,
//! `cap_derive`, `memobj_create`, `spawn`, the boot capabilities) and
//! disappears through [`Kernel::release`] (`cap_drop`, instance exit, a
//! message discarded with its channel). Transfers (`send`/`recv`, `spawn`)
//! move a capability without touching the count. When the count reaches zero
//! the object is destroyed; a destroyed channel releases the capabilities in
//! its queued messages.
//!
//! Channels additionally count their send and receive ends so that `recv`
//! can report `ECLOSED` once every sender is gone: `retain`/`release` call
//! `sender_added`/`sender_dropped` for capabilities with `SEND` and
//! `receiver_added`/`receiver_dropped` for ones with `RECV`, and the last
//! sender going away wakes every thread parked on the channel.
//!
//! # Instances and threads
//!
//! An instance is one Wasm instance plus one thread; its index in
//! [`Kernel::instances`] is both its id and its [`ThreadId`]. There are at
//! most [`kalium_abi::limits::MAX_INSTANCES`] slots: `create_instance`
//! refuses to go past that (`ENOMEM` to the spawner), and the slot of an
//! exited instance is reused by the next one created, like a process id,
//! so a `spawn` loop that drops its `Instance` capabilities cannot grow the
//! kernel's heap. Slot 0 is `init`'s for the kernel's lifetime. Nothing
//! refers to an instance by index across its exit except the
//! `Object::Instance` its capabilities name, which records the exit itself.
//! The Wasm state ([`Runnable`]) is taken out of the slot while the run
//! loop executes it so that host calls can borrow the rest of the kernel. A
//! thread that blocks
//! is parked on the wait queue of the object it waits for and remembers
//! which queues it is in, so a wake-up through any one of them removes it
//! from all (`wait_any` parks on several).

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use kalium_cap::{CapError, CapIndex, CapTable, Capability, ObjectId, ObjectKind, Rights};
use kalium_hal::println;
use kalium_ipc::{Channel, WaitQueue};
use kalium_mm::MemObject;
use kalium_sched::{Class, Scheduler, ThreadId};
use kalium_wasm::{Error as WasmError, Execution, Instance as WasmInstance, Limits, Module};

use crate::boot_image::BootModule;
use crate::host;

/// Capability table size per instance.
pub const CAP_TABLE_LIMIT: usize = 256;

/// Name of the entry point every service exports.
pub const ENTRY_EXPORT: &str = "_start";

/// A kernel object.
pub enum Object {
    /// The UART console.
    Console,
    /// The authority to create instances.
    Spawn,
    /// An IPC channel and the threads blocked on it.
    Channel {
        channel: Channel,
        waiters: WaitQueue<ThreadId>,
    },
    /// A memory object.
    MemObj(MemObject),
    /// An instance: its exit state and the threads waiting for it.
    Instance {
        instance: usize,
        exit: Option<i32>,
        waiters: WaitQueue<ThreadId>,
    },
}

struct Entry {
    object: Object,
    /// Capabilities referring to the object plus pins (see the module docs).
    refs: usize,
}

/// The executable state of an instance: the Wasm instance and the execution
/// of its entry function.
pub struct Runnable {
    pub wasm: WasmInstance,
    pub exec: Execution,
}

/// Which function an instance's execution is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The module's start function; `_start` follows when it returns.
    Start,
    /// `_start`.
    Main,
}

/// One instance.
pub struct Inst {
    pub name: &'static str,
    pub caps: CapTable,
    /// The `Object::Instance` that capabilities on this instance refer to.
    pub object: ObjectId,
    /// `None` while the run loop has it, and after exit.
    pub run: Option<Runnable>,
    pub phase: Phase,
    pub exited: Option<i32>,
    /// Wake time of a `sleep` in progress; the re-issued call completes once
    /// it has passed.
    pub sleep_until: Option<u64>,
    /// A `yield_` was issued and returned `Block`; the re-issued call
    /// completes immediately.
    pub yield_pending: bool,
    /// Objects on whose wait queue this thread is parked.
    pub parked_on: Vec<ObjectId>,
}

/// Why `spawn` failed to create an instance.
#[derive(Debug)]
pub enum SpawnError {
    /// The module index is out of range or the module does not decode.
    NoModule,
    /// An import does not resolve or `_start` is missing or mistyped.
    Link,
    /// Instantiation ran out of memory.
    NoMem,
    /// `MAX_INSTANCES` instances are live.
    TooMany,
}

impl SpawnError {
    pub fn errno(&self) -> i32 {
        match self {
            SpawnError::NoModule | SpawnError::Link => kalium_abi::errno::ENOENT,
            SpawnError::NoMem | SpawnError::TooMany => kalium_abi::errno::ENOMEM,
        }
    }
}

/// The whole kernel state. See the module documentation.
pub struct Kernel {
    pub modules: &'static [BootModule],
    /// Decoded and validated modules, filled in on first use.
    decoded: Vec<Option<Arc<Module>>>,
    objects: BTreeMap<u64, Entry>,
    next_object: u64,
    pub instances: Vec<Inst>,
    pub sched: Scheduler,
    pub console: ObjectId,
    pub spawner: ObjectId,
    pub limits: Limits,
    /// Set by anything that made a thread runnable or added one; the run
    /// loop checks whether the running thread should give way.
    pub reschedule: bool,
}

impl Kernel {
    pub fn new(modules: &'static [BootModule], limits: Limits) -> Kernel {
        let mut k = Kernel {
            modules,
            decoded: (0..modules.len()).map(|_| None).collect(),
            objects: BTreeMap::new(),
            next_object: 1,
            instances: Vec::new(),
            sched: Scheduler::new(),
            console: ObjectId(0),
            spawner: ObjectId(0),
            limits,
            reschedule: false,
        };
        k.console = k.new_object(Object::Console, 1);
        k.spawner = k.new_object(Object::Spawn, 1);
        k
    }

    // -- registry -------------------------------------------------------------

    /// Registers an object with `pins` references that no capability holds.
    pub fn new_object(&mut self, object: Object, pins: usize) -> ObjectId {
        let id = self.next_object;
        self.next_object += 1;
        self.objects.insert(id, Entry { object, refs: pins });
        ObjectId(id)
    }

    pub fn object(&self, id: ObjectId) -> Option<&Object> {
        self.objects.get(&id.0).map(|e| &e.object)
    }

    pub fn object_mut(&mut self, id: ObjectId) -> Option<&mut Object> {
        self.objects.get_mut(&id.0).map(|e| &mut e.object)
    }

    pub fn channel_mut(&mut self, id: ObjectId) -> Option<&mut Channel> {
        match self.object_mut(id) {
            Some(Object::Channel { channel, .. }) => Some(channel),
            _ => None,
        }
    }

    pub fn memobj(&self, id: ObjectId) -> Option<&MemObject> {
        match self.object(id) {
            Some(Object::MemObj(m)) => Some(m),
            _ => None,
        }
    }

    pub fn memobj_mut(&mut self, id: ObjectId) -> Option<&mut MemObject> {
        match self.object_mut(id) {
            Some(Object::MemObj(m)) => Some(m),
            _ => None,
        }
    }

    /// A capability on `cap.object` came into existence.
    pub fn retain(&mut self, cap: Capability) {
        if let Some(e) = self.objects.get_mut(&cap.object.0) {
            e.refs = e.refs.saturating_add(1);
            if let Object::Channel { channel, .. } = &mut e.object {
                if cap.rights.contains(Rights::SEND) {
                    channel.sender_added();
                }
                if cap.rights.contains(Rights::RECV) {
                    channel.receiver_added();
                }
            }
        }
    }

    /// A capability on `cap.object` was destroyed. Wakes receivers when the
    /// last sender goes and destroys the object when nothing refers to it
    /// any more.
    pub fn release(&mut self, cap: Capability, now: u64) {
        let mut pending = alloc::vec![cap];
        while let Some(cap) = pending.pop() {
            let id = cap.object;
            let mut to_wake = Vec::new();
            let mut gone = false;
            if let Some(e) = self.objects.get_mut(&id.0) {
                if let Object::Channel { channel, waiters } = &mut e.object {
                    if cap.rights.contains(Rights::SEND) {
                        channel.sender_dropped();
                        if channel.senders() == 0 {
                            to_wake = waiters.wake_all();
                        }
                    }
                    if cap.rights.contains(Rights::RECV) {
                        channel.receiver_dropped();
                    }
                }
                e.refs = e.refs.saturating_sub(1);
                gone = e.refs == 0;
            }
            for tid in to_wake {
                self.wake_thread(tid, now);
            }
            if gone {
                if let Some(entry) = self.objects.remove(&id.0) {
                    if let Object::Channel { mut channel, .. } = entry.object {
                        for msg in channel.drain() {
                            pending.extend(msg.caps);
                        }
                    }
                }
            }
        }
    }

    /// Drops a pin taken with `new_object`.
    pub fn unpin(&mut self, id: ObjectId, now: u64) {
        self.release(Capability::new(id, ObjectKind::Console, Rights::NONE), now);
    }

    /// Inserts a new capability into an instance's table.
    pub fn mint(&mut self, instance: usize, cap: Capability) -> Result<CapIndex, CapError> {
        let idx = self.instances[instance].caps.insert(cap)?;
        self.retain(cap);
        Ok(idx)
    }

    /// Inserts a new capability into a table that is not (yet) an
    /// instance's, e.g. init's boot capabilities.
    pub fn mint_into(
        &mut self,
        table: &mut CapTable,
        cap: Capability,
    ) -> Result<CapIndex, CapError> {
        let idx = table.insert(cap)?;
        self.retain(cap);
        Ok(idx)
    }

    /// Removes a slot and destroys its capability.
    pub fn destroy_slot(
        &mut self,
        instance: usize,
        idx: CapIndex,
        now: u64,
    ) -> Result<(), CapError> {
        let cap = self.instances[instance].caps.take(idx)?;
        self.release(cap, now);
        Ok(())
    }

    // -- blocking ------------------------------------------------------------

    /// Parks `tid` on the wait queue of `object` (a channel or an instance).
    pub fn park(&mut self, tid: ThreadId, object: ObjectId) {
        let parked = match self.object_mut(object) {
            Some(Object::Channel { waiters, .. }) | Some(Object::Instance { waiters, .. }) => {
                waiters.park(tid);
                true
            }
            _ => false,
        };
        if parked {
            let list = &mut self.instances[tid.0 as usize].parked_on;
            if !list.contains(&object) {
                list.push(object);
            }
        }
    }

    /// Removes `tid` from every wait queue it is parked on.
    fn unpark(&mut self, tid: ThreadId) {
        let parked = core::mem::take(&mut self.instances[tid.0 as usize].parked_on);
        for object in parked {
            match self.object_mut(object) {
                Some(Object::Channel { waiters, .. }) | Some(Object::Instance { waiters, .. }) => {
                    waiters.remove(tid);
                }
                _ => {}
            }
        }
    }

    /// Makes a blocked or sleeping thread runnable.
    pub fn wake_thread(&mut self, tid: ThreadId, now: u64) {
        if (tid.0 as usize) >= self.instances.len() {
            return;
        }
        self.unpark(tid);
        self.sched.wake(tid, now);
        self.reschedule = true;
    }

    /// Wakes the longest-waiting thread parked on a channel, if any.
    pub fn wake_one_on(&mut self, object: ObjectId, now: u64) {
        let tid = match self.object_mut(object) {
            Some(Object::Channel { waiters, .. }) => waiters.wake_one(),
            _ => None,
        };
        if let Some(tid) = tid {
            self.wake_thread(tid, now);
        }
    }

    // -- instances -------------------------------------------------------------

    /// The decoded module `index` of the boot image, decoding it on first use.
    fn decoded_module(&mut self, index: usize) -> Result<Arc<Module>, SpawnError> {
        let slot = self.decoded.get_mut(index).ok_or(SpawnError::NoModule)?;
        if let Some(m) = slot {
            return Ok(m.clone());
        }
        let boot = &self.modules[index];
        match Module::new(boot.bytes) {
            Ok(m) => {
                let m = Arc::new(m);
                *slot = Some(m.clone());
                Ok(m)
            }
            Err(e) => {
                println!(
                    "kalium: module {} ({}) failed to load: {}",
                    index, boot.name, e
                );
                Err(SpawnError::NoModule)
            }
        }
    }

    /// The slot for a new instance: the lowest slot whose instance has
    /// exited and whose thread the scheduler has let go, else a fresh one
    /// while there are fewer than `MAX_INSTANCES`. Slot 0 (`init`) is never
    /// reused: the run loop treats its exit as the end of the kernel.
    fn free_slot(&self) -> Option<usize> {
        let reusable = (1..self.instances.len()).find(|&i| {
            self.instances[i].exited.is_some() && !self.sched.contains(ThreadId(i as u32))
        });
        match reusable {
            Some(i) => Some(i),
            None if self.instances.len() < kalium_abi::limits::MAX_INSTANCES => {
                Some(self.instances.len())
            }
            None => None,
        }
    }

    /// Creates an instance of boot module `module` with `caps` as its
    /// initial capability table (slots `0..n` in order) and schedules it.
    /// Returns the instance id. On error `caps` is handed back untouched.
    /// Fails with `TooMany` once `MAX_INSTANCES` instances are live.
    pub fn create_instance(
        &mut self,
        module: usize,
        caps: CapTable,
        class: Class,
        now: u64,
    ) -> Result<usize, (SpawnError, CapTable)> {
        // Before anything is allocated for the new instance.
        let Some(id) = self.free_slot() else {
            return Err((SpawnError::TooMany, caps));
        };
        let decoded = match self.decoded_module(module) {
            Ok(m) => m,
            Err(e) => return Err((e, caps)),
        };
        let name = self.modules[module].name;
        let has_spawn = caps.holds_kind(ObjectKind::Spawn);
        let mut resolve =
            |m: &str, n: &str, ty: &kalium_wasm::FuncType| host::resolve(m, n, ty, has_spawn);
        let wasm = match WasmInstance::new(decoded.clone(), &mut resolve, &self.limits) {
            Ok(w) => w,
            Err(e) => {
                let err = match e {
                    WasmError::OutOfMemory => SpawnError::NoMem,
                    WasmError::Link { import } => {
                        if let Some(imp) = decoded.imports().get(import as usize) {
                            println!(
                                "kalium: module {} ({}): unresolved import {}:{}",
                                module, name, imp.module, imp.name
                            );
                        }
                        SpawnError::Link
                    }
                    other => {
                        println!(
                            "kalium: module {} ({}) failed to instantiate: {}",
                            module, name, other
                        );
                        SpawnError::Link
                    }
                };
                return Err((err, caps));
            }
        };
        let entry = match wasm.export_func(ENTRY_EXPORT) {
            Some(f)
                if decoded
                    .func_type_of(f)
                    .is_some_and(|t| t.params.is_empty() && t.results.is_empty()) =>
            {
                f
            }
            _ => {
                println!(
                    "kalium: module {} ({}) has no `{}` export of type [] -> []",
                    module, name, ENTRY_EXPORT
                );
                return Err((SpawnError::Link, caps));
            }
        };
        let (exec, phase) = match Execution::start(&wasm) {
            Ok(Some(e)) => (e, Phase::Start),
            Ok(None) => match Execution::new(&wasm, entry, &[]) {
                Ok(e) => (e, Phase::Main),
                Err(_) => return Err((SpawnError::Link, caps)),
            },
            Err(_) => return Err((SpawnError::Link, caps)),
        };

        let object = self.new_object(
            Object::Instance {
                instance: id,
                exit: None,
                waiters: WaitQueue::new(),
            },
            1,
        );
        let inst = Inst {
            name,
            caps,
            object,
            run: Some(Runnable { wasm, exec }),
            phase,
            exited: None,
            sleep_until: None,
            yield_pending: false,
            parked_on: Vec::new(),
        };
        if id == self.instances.len() {
            self.instances.push(inst);
        } else {
            // Reusing the slot of an exited instance; see the module docs.
            self.instances[id] = inst;
        }
        self.sched.add(ThreadId(id as u32), class, now);
        self.reschedule = true;
        Ok(id)
    }

    /// Records the exit of an instance: frees its Wasm state, releases its
    /// capabilities, wakes its waiters and removes its thread (unless it is
    /// the thread the run loop is accounting right now, which the run loop
    /// removes itself).
    pub fn finish_instance(&mut self, instance: usize, code: i32, now: u64) {
        let tid = ThreadId(instance as u32);
        let inst = &mut self.instances[instance];
        if inst.exited.is_some() {
            return;
        }
        inst.exited = Some(code);
        inst.run = None;
        inst.sleep_until = None;
        inst.yield_pending = false;
        let name = inst.name;
        let object = inst.object;
        let caps: Vec<Capability> = inst.caps.iter().map(|(_, c)| *c).collect();
        inst.caps = CapTable::new(CAP_TABLE_LIMIT);

        if let Some(stats) = self.sched.rt_stats(tid) {
            println!(
                "kalium: instance {} ({}) rt periods {} met {} missed {} budget-exhausted {}",
                instance, name, stats.periods, stats.met, stats.missed, stats.budget_exhausted
            );
        }
        println!("kalium: instance {} ({}) exited {}", instance, name, code);

        self.unpark(tid);
        if self.sched.current() != Some(tid) {
            self.sched.remove(tid);
        }
        for cap in caps {
            self.release(cap, now);
        }
        let waiters = match self.object_mut(object) {
            Some(Object::Instance { exit, waiters, .. }) => {
                *exit = Some(code);
                waiters.wake_all()
            }
            _ => Vec::new(),
        };
        for w in waiters {
            self.wake_thread(w, now);
        }
        self.unpin(object, now);
    }

    /// Instances that have not exited.
    pub fn live_instances(&self) -> usize {
        self.instances.iter().filter(|i| i.exited.is_none()).count()
    }
}
