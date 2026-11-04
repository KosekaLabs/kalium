//! `Instance::snapshot` on a short heap: every copy it makes is allocated
//! fallibly and a refusal is `Error::OutOfMemory`, never an abort.
//!
//! The kernel takes a snapshot on a host call (`instance_snapshot`) of a
//! parked instance whose tables may hold the element limit (65536 entries,
//! a 1 MiB copy) and charges it to its budget only once it exists, so the
//! copies run against whatever headroom the kernel has left. Regression:
//! the memory copy used `try_reserve_exact` but the globals, tables, table
//! entries and segment flags were allocated infallibly, so a short heap
//! aborted the kernel on the table copy instead of returning `ENOMEM`.
//!
//! This file is its own test binary because it installs a global allocator
//! that, while armed, refuses (returns null for) every allocation of at
//! least a threshold, as the kernel's heap does when it has nothing left.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kalium_wasm::{Error, Instance, Limits, Module, Value};

/// Refuses allocations of at least `LIMIT` bytes while `LIMIT` is
/// non-zero.
struct Refusing;

static LIMIT: AtomicUsize = AtomicUsize::new(0);

// SAFETY: delegates to `System` for everything it does not refuse, and
// deallocates with the layout it allocated with.
unsafe impl GlobalAlloc for Refusing {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let lim = LIMIT.load(Ordering::Relaxed);
        if lim != 0 && l.size() >= lim {
            return core::ptr::null_mut();
        }
        // SAFETY: `l` has a non-zero size, as required of the caller.
        unsafe { System.alloc(l) }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `System.alloc(l)` above.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static ALLOCATOR: Refusing = Refusing;

/// Takes the snapshot with allocations of at least `refuse_at` bytes
/// refused, and disarms the allocator again before anything else can
/// allocate.
fn snapshot_refusing(inst: &Instance, refuse_at: usize) -> Result<kalium_wasm::Snapshot, Error> {
    LIMIT.store(refuse_at, Ordering::Relaxed);
    let r = inst.snapshot();
    LIMIT.store(0, Ordering::Relaxed);
    r
}

/// One test, so that the armed allocator is never shared with another
/// test thread.
#[test]
fn snapshot_of_a_full_table_on_a_short_heap_is_out_of_memory_not_an_abort() {
    let limits = Limits::default();
    let elements = limits.max_table_elements;
    let wat = r#"(module
        (memory 1)
        (table 1 funcref)
        (global (mut i32) (i32.const 7))
        (global (mut i64) (i64.const 9))
        (data "abc")
        (elem func)
        (func)
    )"#;
    let module = Arc::new(Module::new(&wat::parse_str(wat).unwrap()).unwrap());
    let inst = Instance::new(module, &mut |_, _, _| Err(()), &limits).unwrap();
    // The table grown to the element limit, as a hostile instance would.
    let grown = inst
        .table_ref(0)
        .unwrap()
        .borrow_mut()
        .grow(elements - 1, Value::FuncRef(None));
    assert_eq!(grown, Some(1));
    assert_eq!(inst.table_elements(), elements);
    let entries_bytes = elements as usize * core::mem::size_of::<Value>();
    assert!(entries_bytes >= 1 << 20, "a full table is a 1 MiB copy");

    // Heap short of the table copy (the memory copy, 64 KiB, still fits):
    // ENOMEM, not an abort.
    assert!(matches!(
        snapshot_refusing(&inst, entries_bytes),
        Err(Error::OutOfMemory)
    ));
    // Short of the memory copy as well.
    assert!(matches!(
        snapshot_refusing(&inst, 64 * 1024),
        Err(Error::OutOfMemory)
    ));
    // Short of everything but the smallest allocations: the globals, the
    // tables list and the segment flags refuse too.
    assert!(matches!(
        snapshot_refusing(&inst, 1),
        Err(Error::OutOfMemory)
    ));

    // With the heap back, the snapshot is complete and restores.
    let snapshot = snapshot_refusing(&inst, 0).expect("snapshot with a heap");
    assert_eq!(snapshot.memory_pages(), 1);
    let bytes = snapshot.to_bytes();
    assert_eq!(bytes.len(), snapshot.encoded_len());
    assert!(bytes.len() > entries_bytes / 8, "the full table is encoded");
    let restored = Instance::from_snapshot(
        inst.module().clone(),
        &snapshot,
        &mut |_, _, _| None,
        &limits,
        0,
    )
    .expect("restore");
    assert_eq!(restored.table_elements(), elements);
    assert_eq!(restored.snapshot().unwrap(), snapshot);
}
