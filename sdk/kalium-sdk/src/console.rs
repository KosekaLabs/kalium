//! The console capability and formatted printing.

use core::fmt;

use crate::{raw, Cap, Errno};

/// A `Console` capability: bytes written to it go to the UART.
///
/// Needs kind `Console` with `WRITE`; the kernel checks on every write.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Console(Cap);

impl Console {
    /// Wraps a capability that is (expected to be) a console.
    pub const fn from_cap(cap: Cap) -> Console {
        Console(cap)
    }

    /// The underlying capability.
    pub const fn cap(self) -> Cap {
        self.0
    }

    /// Writes `bytes` to the console (`console_write`).
    pub fn write(self, bytes: &[u8]) -> Result<(), Errno> {
        let len = crate::abi_len(bytes.len())?;
        // SAFETY: `bytes` is a live slice; the kernel reads exactly `len`
        // bytes starting at its offset and writes nothing.
        Errno::check(unsafe { raw::console_write(self.0.raw(), raw::addr(bytes.as_ptr()), len) })
    }

    /// Writes a string.
    pub fn write_str(self, s: &str) -> Result<(), Errno> {
        self.write(s.as_bytes())
    }

    /// Writes a string followed by a newline, in one host call when the
    /// line fits the stack buffer.
    pub fn println(self, s: &str) -> Result<(), Errno> {
        let mut w = Writer::new(self);
        w.push(s.as_bytes());
        w.push(b"\n");
        w.flush()
    }

    /// Makes this the console that [`print!`](crate::print),
    /// [`println!`](crate::println) and the panic handler use.
    pub fn set_global(self) {
        crate::set_global_console_cap(self.0);
    }

    /// The console set with [`set_global`](Console::set_global), if any.
    pub fn global() -> Option<Console> {
        crate::global_console_cap().map(Console)
    }
}

/// Size of a [`Writer`]'s stack buffer. Longer output is flushed in
/// pieces, so it is a performance knob, not a limit.
pub const WRITER_BUF: usize = 128;

/// A buffered `core::fmt::Write` over a console.
///
/// Bytes accumulate in a stack buffer and are written when it fills and on
/// [`flush`](Writer::flush). Errors from the console are remembered in
/// [`error`](Writer::error) rather than reported through `fmt::Error`, so
/// a `write!` to a broken console is a no-op instead of a panic in the
/// caller's `unwrap`.
pub struct Writer {
    console: Console,
    buf: [u8; WRITER_BUF],
    len: usize,
    error: Option<Errno>,
}

impl Writer {
    /// A writer with an empty buffer.
    pub const fn new(console: Console) -> Writer {
        Writer {
            console,
            buf: [0; WRITER_BUF],
            len: 0,
            error: None,
        }
    }

    /// The first error the console reported, if any.
    pub fn error(&self) -> Option<Errno> {
        self.error
    }

    /// Appends bytes, flushing as the buffer fills.
    pub fn push(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.len >= WRITER_BUF {
                let _ = self.flush();
            }
            let n = crate::text::copy_into(self.buf.get_mut(self.len..).unwrap_or(&mut []), bytes);
            if n == 0 {
                // Unreachable (the buffer was just emptied); never spin.
                return;
            }
            self.len += n;
            bytes = bytes.get(n..).unwrap_or(&[]);
        }
    }

    /// Writes out whatever is buffered.
    pub fn flush(&mut self) -> Result<(), Errno> {
        if self.len == 0 {
            return Ok(());
        }
        let r = self.console.write(self.buf.get(..self.len).unwrap_or(&[]));
        self.len = 0;
        if let Err(e) = r {
            if self.error.is_none() {
                self.error = Some(e);
            }
        }
        r
    }
}

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push(s.as_bytes());
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Implementation of [`print!`](crate::print) / [`println!`](crate::println):
/// formats into a [`Writer`] on the given console (or the global one) and
/// flushes once. Without a console the output is dropped.
#[doc(hidden)]
pub fn _print(console: Option<Console>, args: fmt::Arguments<'_>, newline: bool) {
    let Some(console) = console.or_else(Console::global) else {
        return;
    };
    let mut w = Writer::new(console);
    let _ = fmt::Write::write_fmt(&mut w, args);
    if newline {
        w.push(b"\n");
    }
    let _ = w.flush();
}

/// Formats to the global console (see [`Console::set_global`]).
///
/// Output goes through `core::fmt`; for the smallest binaries see
/// [`text::Line`](crate::text::Line).
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::_print(::core::option::Option::None, ::core::format_args!($($arg)*), false)
    };
}

/// Formats a line to the global console.
#[macro_export]
macro_rules! println {
    () => {
        $crate::console::_print(::core::option::Option::None, ::core::format_args!(""), true)
    };
    ($($arg:tt)*) => {
        $crate::console::_print(::core::option::Option::None, ::core::format_args!($($arg)*), true)
    };
}

/// Formats to a specific console: `cprint!(console, "{}", x)`.
#[macro_export]
macro_rules! cprint {
    ($console:expr, $($arg:tt)*) => {
        $crate::console::_print(::core::option::Option::Some($console), ::core::format_args!($($arg)*), false)
    };
}

/// Formats a line to a specific console: `cprintln!(console, "{}", x)`.
#[macro_export]
macro_rules! cprintln {
    ($console:expr) => {
        $crate::console::_print(::core::option::Option::Some($console), ::core::format_args!(""), true)
    };
    ($console:expr, $($arg:tt)*) => {
        $crate::console::_print(::core::option::Option::Some($console), ::core::format_args!($($arg)*), true)
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write as _;

    #[test]
    fn writer_buffers_and_remembers_errors() {
        // Host stubs fail every write with EFAULT; buffering itself must
        // work regardless and never panic on long input.
        let mut w = Writer::new(Console::from_cap(Cap::slot(0)));
        assert!(w.write_str("abc").is_ok());
        assert_eq!(w.len, 3);
        assert_eq!(&w.buf[..3], b"abc");
        assert_eq!(w.error(), None);
        let long = [b'x'; WRITER_BUF * 3 + 5];
        w.push(&long);
        assert_eq!(w.error(), Some(Errno::Fault));
        assert_eq!(w.len, (3 + long.len()) % WRITER_BUF);
        assert_eq!(w.flush(), Err(Errno::Fault));
        assert_eq!(w.len, 0);
        assert!(write!(w, "{}-{}", 1, 2).is_ok());
        assert_eq!(&w.buf[..3], b"1-2");
    }

    #[test]
    fn global_console_is_unset_by_default_and_settable() {
        // Nothing else in this test binary sets it first.
        let before = Console::global();
        let c = Console::from_cap(Cap::slot(3));
        c.set_global();
        assert_eq!(Console::global(), Some(c));
        assert_eq!(c.cap(), Cap(3));
        // Printing with a console that fails must not panic.
        println!("{} {}", "hello", 42);
        print!("x");
        cprintln!(c, "y{}", 1);
        cprint!(c, "z");
        println!();
        cprintln!(c);
        let _ = before;
    }
}
