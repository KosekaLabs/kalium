//! Creating and controlling instances. Importing anything from here links
//! `kalium:spawn`, which the kernel only resolves for instances that hold a
//! `Spawn` capability at instantiation; a service without one must not
//! call these (its instantiation would fail with `ENOENT` to the spawner).

use crate::{abi_len, raw, Cap, Errno};

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

/// An `Instance` capability returned by [`spawn`]: `WAIT|KILL|DERIVE|TRANSFER`.
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
        assert_eq!(
            spawn(Cap::slot(1), 1, &[Cap::slot(0)], Class::Fair, 0, 0).err(),
            Some(Errno::Fault)
        );
        assert_eq!(
            spawn(Cap::slot(1), u32::MAX, &[], Class::RealTime, 1, 1).err(),
            Some(Errno::NoEnt)
        );
        let i = Instance::from_cap(Cap::slot(5));
        assert_eq!(i.cap(), Cap(5));
        assert_eq!(i.wait(), Err(Errno::Fault));
        assert_eq!(i.kill(), Err(Errno::Fault));
        let _ = module_count();
    }
}
