//! Creating and controlling instances, and snapshots: capturing the state
//! of a parked instance ([`instance_snapshot`]) and starting new instances
//! from it ([`spawn_snapshot`]) instead of running their initialisation
//! again. Importing anything from here links
//! `kalium:spawn`, which the kernel only resolves for instances that hold a
//! `Spawn` capability at instantiation; a service without one must not
//! call these (its instantiation would fail with `ENOENT` to the spawner).

use crate::{Cap, Errno, abi_len, raw};

/// The scheduling class of a new instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Weighted fair scheduling; `period_ns` and `budget_ns` are ignored.
    Fair,
    /// Real-time: released every `period_ns` with `budget_ns` of CPU time.
    RealTime,
}

impl Class {
    /// The ABI value.
    pub const fn code(self) -> i32 {
        match self {
            Class::Fair => kalium_abi::sched::CLASS_FAIR,
            Class::RealTime => kalium_abi::sched::CLASS_REAL_TIME,
        }
    }
}

/// The export an instance created by [`spawn_snapshot`] starts at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// The export `_start`, as [`spawn`] runs it.
    Start,
    /// The export `resume`: the convention for continuing from a restored
    /// state. Both must be of type `[] -> []`.
    Resume,
}

impl Entry {
    /// The ABI value.
    pub const fn code(self) -> i32 {
        match self {
            Entry::Start => kalium_abi::entry::START,
            Entry::Resume => kalium_abi::entry::RESUME,
        }
    }
}

/// An `Instance` capability returned by [`spawn`] or [`spawn_snapshot`]:
/// `WAIT|KILL|SNAPSHOT|DERIVE|TRANSFER`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instance(Cap);

impl Instance {
    /// Wraps a capability that is (expected to be) an instance.
    pub const fn from_cap(cap: Cap) -> Instance {
        Instance(cap)
    }

    /// The underlying capability.
    pub const fn cap(self) -> Cap {
        self.0
    }

    /// See [`instance_wait`].
    pub fn wait(self) -> Result<i32, Errno> {
        instance_wait(self)
    }

    /// See [`instance_kill`].
    pub fn kill(self) -> Result<(), Errno> {
        instance_kill(self)
    }

    /// See [`instance_snapshot`].
    pub fn snapshot(self) -> Result<Snapshot, Errno> {
        instance_snapshot(self)
    }
}

/// A `Snapshot` capability returned by [`instance_snapshot`]:
/// `READ|DERIVE|TRANSFER`. The snapshot object (the captured memory,
/// globals and tables of an instance, and the module it came from) lives
/// as long as a capability on it does.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot(Cap);

impl Snapshot {
    /// Wraps a capability that is (expected to be) a snapshot.
    pub const fn from_cap(cap: Cap) -> Snapshot {
        Snapshot(cap)
    }

    /// The underlying capability.
    pub const fn cap(self) -> Cap {
        self.0
    }

    /// See [`spawn_snapshot`].
    pub fn spawn(
        self,
        spawn_cap: Cap,
        caps: &[Cap],
        class: Class,
        period_ns: i64,
        budget_ns: i64,
        entry: Entry,
    ) -> Result<Instance, Errno> {
        spawn_snapshot(spawn_cap, self, caps, class, period_ns, budget_ns, entry)
    }
}

/// Creates an instance of boot-image module `module` (`spawn`).
///
/// `spawn_cap` needs kind `Spawn` with `SPAWN`. The `caps` (each needs
/// `TRANSFER`) are moved to the child and become its slots `0..caps.len()`
/// in order; the values are stale afterwards. `period_ns` and `budget_ns`
/// matter only for [`Class::RealTime`]. Fails with [`Errno::NoEnt`] if the
/// module does not exist or an import of it does not resolve.
pub fn spawn(
    spawn_cap: Cap,
    module: u32,
    caps: &[Cap],
    class: Class,
    period_ns: i64,
    budget_ns: i64,
) -> Result<Instance, Errno> {
    let module = i32::try_from(module).map_err(|_| Errno::NoEnt)?;
    let ncaps = abi_len(caps.len())?;
    let mut out: i32 = Cap::NONE.raw();
    // SAFETY: `caps` is a live slice of `ncaps` `i32`s (`Cap` is
    // `repr(transparent)` over `i32`) the kernel reads; `out` is a live,
    // writable `i32` the kernel writes.
    let r = unsafe {
        raw::spawn(
            spawn_cap.raw(),
            module,
            raw::addr(caps.as_ptr()),
            ncaps,
            class.code(),
            period_ns,
            budget_ns,
            raw::addr_mut(&mut out),
        )
    };
    Errno::check(r)?;
    if out < 0 {
        return Err(Errno::Other(r));
    }
    Ok(Instance(Cap(out)))
}

/// Blocks until the instance exits and returns its exit code
/// (`instance_wait`); needs `WAIT`. A trap reports `-1`, a kill `-2`.
pub fn instance_wait(instance: Instance) -> Result<i32, Errno> {
    let mut code: i32 = 0;
    // SAFETY: `code` is a live, writable `i32` the kernel writes.
    let r = unsafe { raw::instance_wait(instance.0.raw(), raw::addr_mut(&mut code)) };
    Errno::check(r)?;
    Ok(code)
}

/// Terminates the instance (`instance_kill`); needs `KILL`.
pub fn instance_kill(instance: Instance) -> Result<(), Errno> {
    // SAFETY: no pointers are passed.
    Errno::check(unsafe { raw::instance_kill(instance.0.raw()) })
}

/// Number of modules in the boot image (`module_count`).
pub fn module_count() -> i32 {
    // SAFETY: no pointers are passed.
    unsafe { raw::module_count() }
}

/// Captures the state of `instance` (`instance_snapshot`): its linear
/// memory, globals and tables, nothing of the execution in flight. Needs
/// `SNAPSHOT` on the instance capability, and the instance must be parked
/// in a host call (blocked in `recv`/`wait_any`/`instance_wait` or
/// sleeping): [`Errno::Busy`] while it is runnable (retry after
/// [`yield_now`](crate::yield_now)), is the caller, or shares its memory or
/// tables with another instance; [`Errno::Closed`] once it has exited.
/// Returns a [`Snapshot`] capability (`READ|DERIVE|TRANSFER`).
pub fn instance_snapshot(instance: Instance) -> Result<Snapshot, Errno> {
    let mut out: i32 = Cap::NONE.raw();
    // SAFETY: `out` is a live, writable `i32` the kernel writes.
    let r = unsafe { raw::instance_snapshot(instance.0.raw(), raw::addr_mut(&mut out)) };
    Errno::check(r)?;
    if out < 0 {
        return Err(Errno::Other(r));
    }
    Ok(Snapshot(Cap(out)))
}

/// Creates an instance of the snapshot's module with the snapshot's state
/// (`spawn_snapshot`): its memory, globals and tables start as captured,
/// no data or element segment and no start function runs, and the
/// execution starts at `entry`, the export `_start` or `resume`, which
/// must be of type `[] -> []` ([`Errno::NoEnt`] otherwise). `snapshot`
/// needs `READ`; the rest is as for [`spawn`], the transferred `caps`
/// becoming slots `0..caps.len()` of the new instance (the restored memory
/// still holds whatever slot numbers the snapshotted instance stored, so
/// pass the capabilities in the same order).
pub fn spawn_snapshot(
    spawn_cap: Cap,
    snapshot: Snapshot,
    caps: &[Cap],
    class: Class,
    period_ns: i64,
    budget_ns: i64,
    entry: Entry,
) -> Result<Instance, Errno> {
    let ncaps = abi_len(caps.len())?;
    let mut out: i32 = Cap::NONE.raw();
    // SAFETY: `caps` is a live slice of `ncaps` `i32`s (`Cap` is
    // `repr(transparent)` over `i32`) the kernel reads; `out` is a live,
    // writable `i32` the kernel writes.
    let r = unsafe {
        raw::spawn_snapshot(
            spawn_cap.raw(),
            snapshot.0.raw(),
            raw::addr(caps.as_ptr()),
            ncaps,
            class.code(),
            period_ns,
            budget_ns,
            entry.code(),
            raw::addr_mut(&mut out),
        )
    };
    Errno::check(r)?;
    if out < 0 {
        return Err(Errno::Other(r));
    }
    Ok(Instance(Cap(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_codes_match_abi() {
        assert_eq!(Class::Fair.code(), 0);
        assert_eq!(Class::RealTime.code(), 1);
    }

    #[test]
    fn wrappers_report_stub_errors() {
        assert_eq!(spawn(Cap::slot(1), 1, &[Cap::slot(0)], Class::Fair, 0, 0).err(), Some(Errno::Fault));
        assert_eq!(spawn(Cap::slot(1), u32::MAX, &[], Class::RealTime, 1, 1).err(), Some(Errno::NoEnt));
        let i = Instance::from_cap(Cap::slot(5));
        assert_eq!(i.cap(), Cap(5));
        assert_eq!(i.wait(), Err(Errno::Fault));
        assert_eq!(i.kill(), Err(Errno::Fault));
        assert_eq!(i.snapshot(), Err(Errno::Fault));
        let _ = module_count();
        let s = Snapshot::from_cap(Cap::slot(6));
        assert_eq!(s.cap(), Cap(6));
        assert_eq!(s.spawn(Cap::slot(1), &[Cap::slot(0)], Class::Fair, 0, 0, Entry::Resume), Err(Errno::Fault));
        assert_eq!(spawn_snapshot(Cap::slot(1), s, &[], Class::RealTime, 1, 1, Entry::Start), Err(Errno::Fault));
    }

    #[test]
    fn entry_codes_match_abi() {
        assert_eq!(Entry::Start.code(), 0);
        assert_eq!(Entry::Resume.code(), 1);
        assert_eq!(core::mem::size_of::<Snapshot>(), 4);
    }
}
