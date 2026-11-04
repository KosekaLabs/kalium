//! MMU: identity map with caches enabled.
//!
//! Layout (4 KiB granule, 39-bit VA through TTBR0_EL1, three levels):
//!
//! | Range           | Attributes                                    |
//! |-----------------|-----------------------------------------------|
//! | 0 – 1 GiB       | Device-nGnRnE, never executable (MMIO on virt) |
//! | 1 – 2 GiB       | Normal write-back cacheable, inner shareable  |
//! | 2 – 4 GiB       | Normal write-back cacheable, inner shareable  |
//!
//! The first two GiB are mapped with 2 MiB blocks through level-2 tables so
//! that the memory manager can later refine individual regions; 2–4 GiB use
//! 1 GiB blocks at level 1. QEMU `virt` places RAM at `0x4000_0000`, so with
//! up to 3 GiB of RAM everything is covered. TTBR1 walks are disabled.

use super::cpu::{self, read_sysreg, write_sysreg};

const ENTRIES: usize = 512;
const BLOCK_2M: u64 = 2 << 20;
const BLOCK_1G: u64 = 1 << 30;

/// A 4 KiB-aligned translation table.
#[repr(C, align(4096))]
struct Table([u64; ENTRIES]);

/// Level 1: one entry per GiB.
static mut L1: Table = Table([0; ENTRIES]);
/// Level 2 for 0–1 GiB (device) and 1–2 GiB (normal), 2 MiB per entry.
static mut L2_DEVICE: Table = Table([0; ENTRIES]);
static mut L2_NORMAL: Table = Table([0; ENTRIES]);

// Descriptor bits.
const VALID: u64 = 1 << 0;
const TABLE: u64 = 1 << 1; // at L1/L2: 1 = table, 0 = block
const AF: u64 = 1 << 10;
const SH_OUTER: u64 = 0b10 << 8;
const SH_INNER: u64 = 0b11 << 8;
const AP_RW_EL1: u64 = 0b00 << 6;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const ATTR_INDEX_SHIFT: u64 = 2;

// MAIR attribute indices.
const ATTR_DEVICE: u64 = 0; // 0x00: Device-nGnRnE
const ATTR_NORMAL: u64 = 1; // 0xff: Normal, write-back, read/write-allocate

const MAIR_VALUE: u64 = (0x00 << (8 * ATTR_DEVICE)) | (0xff << (8 * ATTR_NORMAL));

fn device_block(pa: u64) -> u64 {
    pa | VALID | AF | SH_OUTER | AP_RW_EL1 | PXN | UXN | (ATTR_DEVICE << ATTR_INDEX_SHIFT)
}

fn normal_block(pa: u64) -> u64 {
    pa | VALID | AF | SH_INNER | AP_RW_EL1 | (ATTR_NORMAL << ATTR_INDEX_SHIFT)
}

fn table_entry(table: *const Table) -> u64 {
    (table as u64) | VALID | TABLE
}

// TCR_EL1 fields.
const TCR_T0SZ_39BIT: u64 = 64 - 39;
const TCR_IRGN0_WB: u64 = 0b01 << 8;
const TCR_ORGN0_WB: u64 = 0b01 << 10;
const TCR_SH0_INNER: u64 = 0b11 << 12;
const TCR_TG0_4K: u64 = 0b00 << 14;
const TCR_EPD1: u64 = 1 << 23;
const TCR_IPS_SHIFT: u64 = 32;

// SCTLR_EL1 bits.
const SCTLR_M: u64 = 1 << 0;
const SCTLR_A: u64 = 1 << 1;
const SCTLR_C: u64 = 1 << 2;
const SCTLR_I: u64 = 1 << 12;

static ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// True once [`enable`] has run.
pub fn is_enabled() -> bool {
    ENABLED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Build the identity map and turn on the MMU with instruction and data
/// caches. Idempotent. Must run before anything uses atomics or the heap on
/// real hardware; QEMU tolerates earlier use.
pub fn enable() {
    if is_enabled() {
        return;
    }

    let l1 = core::ptr::addr_of_mut!(L1);
    let l2_device = core::ptr::addr_of_mut!(L2_DEVICE);
    let l2_normal = core::ptr::addr_of_mut!(L2_NORMAL);

    // SAFETY: the tables are only written here, before the MMU is on and
    // before any other code could observe them; we go through raw pointers so
    // no references to the `static mut`s are created.
    unsafe {
        for i in 0..ENTRIES {
            (*l2_device).0[i] = device_block(i as u64 * BLOCK_2M);
            (*l2_normal).0[i] = normal_block(BLOCK_1G + i as u64 * BLOCK_2M);
        }
        (*l1).0[0] = table_entry(l2_device);
        (*l1).0[1] = table_entry(l2_normal);
        (*l1).0[2] = normal_block(2 * BLOCK_1G);
        (*l1).0[3] = normal_block(3 * BLOCK_1G);
    }

    // The walks below are done with the MMU off, so the table writes reach
    // memory directly; a DSB orders them before the table walker starts.
    cpu::dsb_sy();

    // Physical address size: use what the CPU reports, capped at 48 bits.
    let parange = read_sysreg!("id_aa64mmfr0_el1") & 0xf;
    let ips = parange.min(0b101);

    write_sysreg!("mair_el1", MAIR_VALUE);
    write_sysreg!(
        "tcr_el1",
        TCR_T0SZ_39BIT
            | TCR_IRGN0_WB
            | TCR_ORGN0_WB
            | TCR_SH0_INNER
            | TCR_TG0_4K
            | TCR_EPD1
            | (ips << TCR_IPS_SHIFT)
    );
    write_sysreg!("ttbr0_el1", l1 as u64);
    cpu::isb();

    // Invalidate TLBs and the instruction cache before turning things on.
    // SAFETY: cache and TLB maintenance only.
    unsafe {
        core::arch::asm!(
            "tlbi vmalle1",
            "dsb nsh",
            "ic iallu",
            "dsb nsh",
            "isb",
            options(nostack)
        );
    }

    let sctlr = read_sysreg!("sctlr_el1");
    let sctlr = (sctlr | SCTLR_M | SCTLR_C | SCTLR_I) & !SCTLR_A;
    write_sysreg!("sctlr_el1", sctlr);
    cpu::isb();

    ENABLED.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// Physical address of the level-1 table (what TTBR0_EL1 holds).
pub fn root_table_address() -> usize {
    core::ptr::addr_of!(L1) as usize
}
