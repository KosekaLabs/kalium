//! GICv2 interrupt controller (QEMU `virt`: distributor at `0x0800_0000`,
//! CPU interface at `0x0801_0000`).
//!
//! The distributor is programmed for a single CPU: every interrupt disabled,
//! priority `0x80`, SPIs targeted at CPU 0. [`enable_irq`] turns one on, and
//! [`handle_irq`] (called from the IRQ vector) acknowledges, dispatches to
//! the handler registered with [`set_irq_handler`], and signals EOI.

use core::ptr::{read_volatile, write_volatile};

use super::IrqSafe;

const GICD_BASE: usize = 0x0800_0000;
const GICC_BASE: usize = 0x0801_0000;

// Distributor registers.
const GICD_CTLR: usize = 0x000;
const GICD_TYPER: usize = 0x004;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_ICPENDR: usize = 0x280;
const GICD_ICACTIVER: usize = 0x380;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;
const GICD_ICFGR: usize = 0xc00;

// CPU interface registers.
const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_BPR: usize = 0x008;
const GICC_IAR: usize = 0x00c;
const GICC_EOIR: usize = 0x010;

const GICD_CTLR_ENABLE: u32 = 1;
const GICC_CTLR_ENABLE: u32 = 1;

/// IAR values at or above this are spurious.
const SPURIOUS: u32 = 1020;

/// Highest interrupt ID the GICv2 architecture supports.
pub const MAX_IRQS: u32 = 1020;

/// Interrupts that can have a handler registered. SGIs and PPIs are 0..32;
/// QEMU virt's SPIs of interest (UART 33, virtio 48..) fit comfortably.
const HANDLER_SLOTS: usize = 256;

/// Default priority for every interrupt (lower is more urgent; 0xff masked).
const DEFAULT_PRIORITY: u8 = 0x80;

#[inline(always)]
fn dist_write(offset: usize, value: u32) {
    // SAFETY: fixed MMIO address of the GICv2 distributor on QEMU virt,
    // mapped as device memory.
    unsafe { write_volatile((GICD_BASE + offset) as *mut u32, value) }
}

#[inline(always)]
fn dist_read(offset: usize) -> u32 {
    // SAFETY: see `dist_write`.
    unsafe { read_volatile((GICD_BASE + offset) as *const u32) }
}

#[inline(always)]
fn dist_write_byte(offset: usize, value: u8) {
    // SAFETY: see `dist_write`; the priority and target registers are
    // byte-accessible.
    unsafe { write_volatile((GICD_BASE + offset) as *mut u8, value) }
}

#[inline(always)]
fn cpu_write(offset: usize, value: u32) {
    // SAFETY: fixed MMIO address of the GICv2 CPU interface on QEMU virt.
    unsafe { write_volatile((GICC_BASE + offset) as *mut u32, value) }
}

#[inline(always)]
fn cpu_read(offset: usize) -> u32 {
    // SAFETY: see `cpu_write`.
    unsafe { read_volatile((GICC_BASE + offset) as *const u32) }
}

/// One optional handler per interrupt ID.
type HandlerTable = [Option<fn()>; HANDLER_SLOTS];

static HANDLERS: IrqSafe<HandlerTable> = IrqSafe::new([None; HANDLER_SLOTS]);

/// Number of interrupt lines the distributor implements.
pub fn num_irqs() -> u32 {
    ((dist_read(GICD_TYPER) & 0x1f) + 1) * 32
}

/// Initialize distributor and CPU interface with everything disabled.
pub fn init() {
    dist_write(GICD_CTLR, 0);

    let lines = num_irqs().min(MAX_IRQS);
    for reg in 0..(lines / 32) as usize {
        dist_write(GICD_ICENABLER + 4 * reg, 0xffff_ffff);
        dist_write(GICD_ICPENDR + 4 * reg, 0xffff_ffff);
        dist_write(GICD_ICACTIVER + 4 * reg, 0xffff_ffff);
    }
    for irq in 0..lines as usize {
        dist_write_byte(GICD_IPRIORITYR + irq, DEFAULT_PRIORITY);
    }
    // SPIs (32..) go to CPU 0; the PPI/SGI target registers are read-only.
    for irq in 32..lines as usize {
        dist_write_byte(GICD_ITARGETSR + irq, 0x01);
    }
    // SPIs level-sensitive by default (0b00 per interrupt, two bits each).
    for reg in 2..(lines / 16) as usize {
        dist_write(GICD_ICFGR + 4 * reg, 0);
    }

    dist_write(GICD_CTLR, GICD_CTLR_ENABLE);

    // CPU interface: accept every priority, no preemption groups.
    cpu_write(GICC_PMR, 0xff);
    cpu_write(GICC_BPR, 0);
    cpu_write(GICC_CTLR, GICC_CTLR_ENABLE);
}

/// Enable delivery of `irq` to this CPU.
pub fn enable_irq(irq: u32) {
    if irq >= MAX_IRQS {
        return;
    }
    dist_write(GICD_ISENABLER + 4 * (irq / 32) as usize, 1 << (irq % 32));
}

/// Disable delivery of `irq`.
pub fn disable_irq(irq: u32) {
    if irq >= MAX_IRQS {
        return;
    }
    dist_write(GICD_ICENABLER + 4 * (irq / 32) as usize, 1 << (irq % 32));
}

/// Set the priority (0 = highest, 0xff = masked) of `irq`.
pub fn set_priority(irq: u32, priority: u8) {
    if irq >= MAX_IRQS {
        return;
    }
    dist_write_byte(GICD_IPRIORITYR + irq as usize, priority);
}

/// Register the function called when `irq` fires. Replaces any previous
/// handler. Returns `false` if `irq` is outside the handler table.
pub fn set_irq_handler(irq: u32, handler: fn()) -> bool {
    HANDLERS.with(|h| match h.get_mut(irq as usize) {
        Some(slot) => {
            *slot = Some(handler);
            true
        }
        None => false,
    })
}

/// Remove the handler for `irq`.
pub fn clear_irq_handler(irq: u32) {
    HANDLERS.with(|h| {
        if let Some(slot) = h.get_mut(irq as usize) {
            *slot = None;
        }
    });
}

/// Acknowledge the highest-priority pending interrupt. Returns `None` on a
/// spurious interrupt.
pub fn acknowledge() -> Option<u32> {
    let iar = cpu_read(GICC_IAR);
    let id = iar & 0x3ff;
    if id >= SPURIOUS { None } else { Some(id) }
}

/// Signal end of interrupt for `irq`.
pub fn end_of_interrupt(irq: u32) {
    cpu_write(GICC_EOIR, irq);
}

/// Drain and dispatch every pending interrupt. Called from the IRQ vector
/// with IRQs masked.
pub fn handle_irq() {
    while let Some(irq) = acknowledge() {
        let handler = HANDLERS.with(|h| h.get(irq as usize).copied().flatten());
        match handler {
            Some(f) => f(),
            None => {
                // Unexpected interrupt: mask it so it cannot storm.
                disable_irq(irq);
                crate::println!("kalium: unexpected IRQ {irq}, disabled");
            }
        }
        end_of_interrupt(irq);
    }
}
