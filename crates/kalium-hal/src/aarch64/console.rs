//! PL011 UART console at `0x0900_0000` (QEMU `virt`) and the `print!` /
//! `println!` macros.
//!
//! Output is serialized by a spinlock. IRQs are masked while the lock is
//! held so that an interrupt handler printing on the same core cannot
//! deadlock against the interrupted context. The panic handler bypasses the
//! lock with [`force_unlock`] because the panicking context may own it.

use core::fmt;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};

use super::cpu;

const UART_BASE: usize = 0x0900_0000;

const DR: usize = 0x00;
const FR: usize = 0x18;
const IBRD: usize = 0x24;
const FBRD: usize = 0x28;
const LCR_H: usize = 0x2c;
const CR: usize = 0x30;
const IMSC: usize = 0x38;
const ICR: usize = 0x44;

const FR_TXFF: u32 = 1 << 5;
const FR_RXFE: u32 = 1 << 4;
const FR_BUSY: u32 = 1 << 3;

const LCR_H_FEN: u32 = 1 << 4;
const LCR_H_WLEN_8: u32 = 0b11 << 5;

const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

#[inline(always)]
fn reg_write(offset: usize, value: u32) {
    // SAFETY: the PL011 lives at this fixed MMIO address on QEMU virt and
    // the address is mapped as device memory by the MMU (identity map).
    unsafe { write_volatile((UART_BASE + offset) as *mut u32, value) }
}

#[inline(always)]
fn reg_read(offset: usize) -> u32 {
    // SAFETY: see `reg_write`.
    unsafe { read_volatile((UART_BASE + offset) as *const u32) }
}

/// Initialize the UART: 8N1, FIFOs enabled, TX and RX on, no interrupts.
/// The divisor values assume the 24 MHz reference clock QEMU advertises;
/// QEMU does not actually care about the baud rate.
pub fn init() {
    reg_write(CR, 0);
    reg_write(ICR, 0x7ff);
    reg_write(IMSC, 0);
    reg_write(IBRD, 13);
    reg_write(FBRD, 1);
    reg_write(LCR_H, LCR_H_WLEN_8 | LCR_H_FEN);
    reg_write(CR, CR_UARTEN | CR_TXE | CR_RXE);
}

/// Write one raw byte, blocking while the TX FIFO is full.
pub fn putc(byte: u8) {
    while reg_read(FR) & FR_TXFF != 0 {
        cpu::spin_hint();
    }
    reg_write(DR, u32::from(byte));
}

/// Read one byte if one is available, without blocking.
pub fn try_getc() -> Option<u8> {
    if reg_read(FR) & FR_RXFE != 0 { None } else { Some((reg_read(DR) & 0xff) as u8) }
}

/// Block until every byte has left the transmitter.
pub fn flush() {
    while reg_read(FR) & FR_BUSY != 0 {
        cpu::spin_hint();
    }
}

/// Write a string, translating `\n` to `\r\n` for terminals.
pub fn write_str(s: &str) {
    write_bytes(s.as_bytes());
}

/// Write raw bytes, translating `\n` to `\r\n` for terminals.
pub fn write_bytes(bytes: &[u8]) {
    for &b in bytes {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}

/// A `fmt::Write` sink over the UART. Obtain one through [`with_console`].
pub struct Console {
    _private: (),
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}

static LOCK: AtomicBool = AtomicBool::new(false);

/// Run `f` with exclusive access to the console, IRQs masked.
pub fn with_console<R>(f: impl FnOnce(&mut Console) -> R) -> R {
    let daif = cpu::save_and_disable_irqs();
    while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        cpu::spin_hint();
    }
    let r = f(&mut Console { _private: () });
    LOCK.store(false, Ordering::Release);
    cpu::restore_irqs(daif);
    r
}

/// Release the console lock unconditionally. Only for the panic handler.
pub fn force_unlock() {
    LOCK.store(false, Ordering::Release);
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    with_console(|c| {
        let _ = fmt::Write::write_fmt(c, args);
    });
}

/// Print without taking the lock. Used by the panic path only.
#[doc(hidden)]
pub fn _print_unlocked(args: fmt::Arguments<'_>) {
    let _ = fmt::Write::write_fmt(&mut Console { _private: () }, args);
}

/// Print to the UART console.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::_print(::core::format_args!($($arg)*))
    };
}

/// Print a line to the UART console.
#[macro_export]
macro_rules! println {
    () => {
        $crate::console::_print(::core::format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::console::_print(::core::format_args!("{}\n", ::core::format_args!($($arg)*)))
    };
}
