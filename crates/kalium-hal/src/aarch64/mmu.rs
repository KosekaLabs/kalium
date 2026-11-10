//! MMU: identity map with caches enabled and W^X page attributes.
//!
//! Layout (4 KiB granule, 39-bit VA through TTBR0_EL1, three levels):
//!
//! | Range                              | Attributes                                      |
//! |------------------------------------|-------------------------------------------------|
//! | 0 – 1 GiB (MMIO on virt)           | Device-nGnRnE, RW, never executable, 2 MiB blocks |
//! | RAM_START .. kernel image          | Normal WB, read-only, XN (the device tree), 2 MiB block |
//! | kernel image (4 KiB pages)         | `.text` RO+X; `.rodata` RO+XN; `.data`/`.bss` RW+XN; stack guards unmapped; stacks RW+XN; the rest of the last 2 MiB RW+XN (heap) |
//! | rest of RAM up to the DTB's size   | Normal WB, RW, never executable, 2 MiB blocks; the last, partial block with 4 KiB pages |
//! | everything else below 4 GiB        | unmapped: a stray access is a translation fault |
//!
//! The RAM mapping ends exactly at the size the device tree reports (or the
//! linker default without one, see [`super::boot::ram_info`], which is 4 KiB
//! granular): when RAM does not end on a 2 MiB boundary, the last block is
//! mapped through a level-3 table up to the end of RAM and no further, so
//! nothing without backing memory is ever mapped and an access past the end
//! of RAM is a translation fault, not an external abort or an SError.
//! `SCTLR_EL1.WXN` is set as well, so even a mistaken writable mapping is
//! not executable at EL1. TTBR1 walks are disabled.
//!
//! Only the kernel image (at most `IMAGE_L3_TABLES` × 2 MiB from
//! `__kernel_start`; `kernel/link.ld` asserts the image fits) and the
//! partial last block of RAM use level-3 tables. Everything else is blocks,
//! so the tables are a few pages of `.bss`.

use super::boot;
use super::cpu::{self, read_sysreg, write_sysreg};

const ENTRIES: usize = 512;
const PAGE_4K: usize = 4 << 10;
const BLOCK_2M: usize = 2 << 20;
const BLOCK_1G: usize = 1 << 30;

/// Level-3 tables available for the kernel image: 2 MiB each. Must match
/// `MAPPED_IMAGE_LIMIT` in `kernel/link.ld`.
const IMAGE_L3_TABLES: usize = 4;

/// Level-2 tables for RAM: 1–2, 2–3 and 3–4 GiB.
const RAM_L2_TABLES: usize = 3;

/// A 4 KiB-aligned translation table.
#[repr(C, align(4096))]
struct Table([u64; ENTRIES]);

/// Level 1: one entry per GiB.
static mut L1: Table = Table([0; ENTRIES]);
/// Level 2 for 0–1 GiB (device), 2 MiB per entry.
static mut L2_DEVICE: Table = Table([0; ENTRIES]);
/// Level 2 for 1–4 GiB (RAM), 2 MiB per entry.
static mut L2_RAM: [Table; RAM_L2_TABLES] = [const { Table([0; ENTRIES]) }; RAM_L2_TABLES];
/// Level 3 for the kernel image, 4 KiB per entry.
static mut L3_IMAGE: [Table; IMAGE_L3_TABLES] = [const { Table([0; ENTRIES]) }; IMAGE_L3_TABLES];
/// Level 3 for the last 2 MiB block of RAM when RAM does not end on a 2 MiB
/// boundary: pages up to the end of RAM are mapped, the rest are not.
static mut L3_RAM_TAIL: Table = Table([0; ENTRIES]);

// Descriptor bits.
const VALID: u64 = 1 << 0;
const TABLE: u64 = 1 << 1; // at L1/L2: 1 = table, 0 = block; at L3: 1 = page
const AF: u64 = 1 << 10;
const SH_OUTER: u64 = 0b10 << 8;
const SH_INNER: u64 = 0b11 << 8;
const AP_RW_EL1: u64 = 0b00 << 6;
const AP_RO_EL1: u64 = 0b10 << 6;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const ATTR_INDEX_SHIFT: u64 = 2;

// MAIR attribute indices and their encodings.
const ATTR_DEVICE: u64 = 0;
const ATTR_NORMAL: u64 = 1;
/// MAIR encoding of attribute index 0: Device-nGnRnE.
pub const MAIR_DEVICE_NGNRNE: u64 = 0x00;
/// MAIR encoding of attribute index 1: Normal, write-back, read/write-allocate.
pub const MAIR_NORMAL_WB: u64 = 0xff;

const MAIR_VALUE: u64 = (MAIR_DEVICE_NGNRNE << (8 * ATTR_DEVICE)) | (MAIR_NORMAL_WB << (8 * ATTR_NORMAL));

/// Access permissions and executability of a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Perm {
    /// Read-only, executable at EL1 (`.text`).
    ReadExec,
    /// Read-only, never executable.
    ReadOnly,
    /// Read-write, never executable.
    ReadWrite,
}

impl Perm {
    fn bits(self) -> u64 {
        match self {
            Perm::ReadExec => AP_RO_EL1 | UXN,
            Perm::ReadOnly => AP_RO_EL1 | PXN | UXN,
            Perm::ReadWrite => AP_RW_EL1 | PXN | UXN,
        }
    }
}

fn device_block(pa: usize) -> u64 {
    pa as u64 | VALID | AF | SH_OUTER | AP_RW_EL1 | PXN | UXN | (ATTR_DEVICE << ATTR_INDEX_SHIFT)
}

fn normal_block(pa: usize, perm: Perm) -> u64 {
    pa as u64 | VALID | AF | SH_INNER | perm.bits() | (ATTR_NORMAL << ATTR_INDEX_SHIFT)
}

fn normal_page(pa: usize, perm: Perm) -> u64 {
    normal_block(pa, perm) | TABLE
}

fn table_entry(table: *const Table) -> u64 {
    (table as u64) | VALID | TABLE
}

/// The permission a 4 KiB page of the kernel image region gets, or `None`
/// for an unmapped guard page. `pa` is page aligned.
fn image_page_perm(pa: usize) -> Option<Perm> {
    let page = pa..pa + PAGE_4K;
    let overlaps = |r: core::ops::Range<usize>| r.start < page.end && page.start < r.end;
    if overlaps(boot::boot_stack_guard_range()) || overlaps(boot::exception_stack_guard_range()) {
        None
    } else if overlaps(boot::text_range()) {
        Some(Perm::ReadExec)
    } else if overlaps(boot::rodata_range()) {
        Some(Perm::ReadOnly)
    } else if pa < boot::kernel_image_range().start {
        // Cannot happen (the image starts on a 2 MiB boundary) but keep the
        // device tree area read-only if it ever did.
        Some(Perm::ReadOnly)
    } else {
        // .data, .bss, the stacks, and heap pages in the image's last 2 MiB.
        Some(Perm::ReadWrite)
    }
}

/// The permission a 4 KiB page of RAM outside the kernel image gets: the
/// device tree area is read-only, everything else is heap.
fn ram_page_perm(pa: usize) -> Perm {
    if boot::dtb_area_range().contains(&pa) { Perm::ReadOnly } else { Perm::ReadWrite }
}

/// Fill the level-3 table `l3` for the 2 MiB block at `block` with the pages
/// below `ram_end` (those at or above it stay unmapped), each with the
/// permission `perm` gives it (`None` = unmapped).
///
/// # Safety
///
/// `l3` must point at a table nothing else is using, and the MMU must be
/// off (the writes bypass no cache then).
unsafe fn fill_l3(l3: *mut Table, block: usize, ram_end: usize, perm: impl Fn(usize) -> Option<Perm>) {
    unsafe {
        for j in 0..ENTRIES {
            let page = block + j * PAGE_4K;
            (*l3).0[j] = match perm(page) {
                Some(p) if page < ram_end => normal_page(page, p),
                _ => 0,
            };
        }
    }
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
const SCTLR_WXN: u64 = 1 << 19;

static ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// True once [`enable`] has run.
pub fn is_enabled() -> bool {
    ENABLED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Build the identity map and turn on the MMU with instruction and data
/// caches. Idempotent. Must run before anything uses atomics or the heap on
/// real hardware; QEMU tolerates earlier use. Sizes the RAM mapping from
/// [`boot::ram_info`], so this is also where the device tree is read.
///
/// Panics (before enabling anything) if the kernel image does not fit the
/// level-3 tables; the linker script makes that impossible for an image it
/// accepts.
pub fn enable() {
    if is_enabled() {
        return;
    }

    let ram = boot::ram_info();
    let image = boot::kernel_image_range();
    let dtb_area = boot::dtb_area_range();
    // `ram_info` rounds the end of RAM down to a page, so the mapping can
    // end exactly there: whole 2 MiB blocks up to `ram.end`, the last,
    // partial block (if any) through `L3_RAM_TAIL`.
    let ram_end = ram.end;
    assert!(ram_end.is_multiple_of(PAGE_4K), "end of RAM {:#x} is not page aligned", ram_end);
    let image_blocks = (image.end - image.start).div_ceil(BLOCK_2M);
    assert!(
        image.start.is_multiple_of(BLOCK_2M) && image_blocks <= IMAGE_L3_TABLES,
        "kernel image {:#x}..{:#x} does not fit {} level-3 tables",
        image.start,
        image.end,
        IMAGE_L3_TABLES
    );

    let l1 = core::ptr::addr_of_mut!(L1);
    let l2_device = core::ptr::addr_of_mut!(L2_DEVICE);
    let l2_ram = core::ptr::addr_of_mut!(L2_RAM) as *mut Table;
    let l3_image = core::ptr::addr_of_mut!(L3_IMAGE) as *mut Table;
    let l3_tail = core::ptr::addr_of_mut!(L3_RAM_TAIL);

    // SAFETY: the tables are only written here, before the MMU is on and
    // before any other code could observe them; we go through raw pointers so
    // no references to the `static mut`s are created.
    unsafe {
        // 0–1 GiB: device memory.
        for i in 0..ENTRIES {
            (*l2_device).0[i] = device_block(i * BLOCK_2M);
        }
        (*l1).0[0] = table_entry(l2_device);

        // RAM: 2 MiB blocks from RAM_START up to the end of RAM, with the
        // device tree area read-only, the kernel image through level 3 and
        // a partial last block through level 3 as well (cut at `ram_end`).
        for (t, gib) in (1..=RAM_L2_TABLES).enumerate() {
            let table = l2_ram.add(t);
            for i in 0..ENTRIES {
                let pa = gib * BLOCK_1G + i * BLOCK_2M;
                (*table).0[i] = if pa < boot::RAM_START || pa >= ram_end {
                    0
                } else if pa >= image.start && pa < image.start + image_blocks * BLOCK_2M {
                    let l3 = l3_image.add((pa - image.start) / BLOCK_2M);
                    fill_l3(l3, pa, ram_end, image_page_perm);
                    table_entry(l3)
                } else if pa + BLOCK_2M > ram_end {
                    fill_l3(l3_tail, pa, ram_end, |page| Some(ram_page_perm(page)));
                    table_entry(l3_tail)
                } else if pa >= dtb_area.start && pa < dtb_area.end {
                    normal_block(pa, Perm::ReadOnly)
                } else {
                    normal_block(pa, Perm::ReadWrite)
                };
            }
            (*l1).0[gib] = table_entry(table);
        }
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
        TCR_T0SZ_39BIT | TCR_IRGN0_WB | TCR_ORGN0_WB | TCR_SH0_INNER | TCR_TG0_4K | TCR_EPD1 | (ips << TCR_IPS_SHIFT)
    );
    write_sysreg!("ttbr0_el1", l1 as u64);
    cpu::isb();

    // Invalidate TLBs and the instruction cache before turning things on.
    // SAFETY: cache and TLB maintenance only.
    unsafe {
        core::arch::asm!("tlbi vmalle1", "dsb nsh", "ic iallu", "dsb nsh", "isb", options(nostack));
    }

    let sctlr = read_sysreg!("sctlr_el1");
    let sctlr = (sctlr | SCTLR_M | SCTLR_C | SCTLR_I | SCTLR_WXN) & !SCTLR_A;
    write_sysreg!("sctlr_el1", sctlr);
    cpu::isb();

    ENABLED.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// Physical address of the level-1 table (what TTBR0_EL1 holds).
pub fn root_table_address() -> usize {
    core::ptr::addr_of!(L1) as usize
}

/// End of the RAM mapping: the first address above RAM_START that is not
/// mapped. This is exactly the end of RAM ([`boot::ram_info`], 4 KiB
/// granular): the last, partial 2 MiB block is mapped with pages, so no
/// address without backing memory is mapped and the heap's end and the
/// mapping's end coincide.
pub fn ram_mapped_end() -> usize {
    boot::ram_info().end
}

/// Result of a successful address translation (`PAR_EL1` with `F == 0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Translation {
    /// Physical address of the byte.
    pub pa: usize,
    /// The MAIR attribute encoding of the mapping ([`MAIR_NORMAL_WB`] or
    /// [`MAIR_DEVICE_NGNRNE`] here).
    pub memory_attr: u8,
    /// Shareability: 0 non, 2 outer, 3 inner.
    pub shareability: u8,
}

/// Ask the MMU how `va` translates for an EL1 read (`write == false`) or
/// write, without touching the memory. `Err(fst)` carries the fault status
/// code the access would raise (see `exceptions::fault_status_name`).
/// Meaningful only once the MMU is on.
pub fn translate(va: usize, write: bool) -> Result<Translation, u64> {
    let par: u64;
    // SAFETY: address translation instructions have no side effects besides
    // writing PAR_EL1, which is read right after.
    unsafe {
        if write {
            core::arch::asm!("at s1e1w, {0}", "isb", "mrs {1}, par_el1",
                in(reg) va as u64, out(reg) par, options(nomem, nostack));
        } else {
            core::arch::asm!("at s1e1r, {0}", "isb", "mrs {1}, par_el1",
                in(reg) va as u64, out(reg) par, options(nomem, nostack));
        }
    }
    if par & 1 != 0 {
        return Err((par >> 1) & 0x3f);
    }
    Ok(Translation {
        pa: ((par & 0x0000_ffff_ffff_f000) | (va as u64 & 0xfff)) as usize,
        memory_attr: (par >> 56) as u8,
        shareability: ((par >> 7) & 0b11) as u8,
    })
}
