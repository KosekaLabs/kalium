//! Formatting without `core::fmt`: a fixed-size line builder and integer
//! helpers, for services that want the smallest possible binary.

use crate::{Console, Errno};

/// Longest decimal rendering of an `i64` (`-9223372036854775808`).
pub const MAX_INT_LEN: usize = 20;

/// Writes the decimal digits of `v` (and a sign) at the end of `buf` and
/// returns them. Written with iterators and `get` so that no bounds check
/// can fail: these helpers must not drag the panic machinery into a
/// service that otherwise has none.
fn digits(buf: &mut [u8; MAX_INT_LEN], mut v: u64, negative: bool) -> &[u8] {
    let mut used = 0usize;
    let mut slots = buf.iter_mut().rev();
    for slot in slots.by_ref() {
        *slot = b'0' + (v % 10) as u8;
        used += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    if negative {
        if let Some(slot) = slots.next() {
            *slot = b'-';
            used += 1;
        }
    }
    buf.get(MAX_INT_LEN.saturating_sub(used)..).unwrap_or(&[])
}

/// The bytes are ASCII by construction; an empty string is returned
/// rather than ever panicking.
fn ascii(b: &[u8]) -> &str {
    core::str::from_utf8(b).unwrap_or("")
}

/// Renders `v` in decimal into `buf` and returns the digits.
pub fn fmt_u64_bytes(buf: &mut [u8; MAX_INT_LEN], v: u64) -> &[u8] {
    digits(buf, v, false)
}

/// Renders `v` in decimal (with a leading `-` if negative) into `buf` and
/// returns the bytes.
pub fn fmt_i64_bytes(buf: &mut [u8; MAX_INT_LEN], v: i64) -> &[u8] {
    digits(buf, v.unsigned_abs(), v < 0)
}

/// Renders `v` in decimal into `buf` and returns the text.
pub fn fmt_u64(buf: &mut [u8; MAX_INT_LEN], v: u64) -> &str {
    ascii(fmt_u64_bytes(buf, v))
}

/// Renders `v` in decimal (with a leading `-` if negative) into `buf` and
/// returns the text.
pub fn fmt_i64(buf: &mut [u8; MAX_INT_LEN], v: i64) -> &str {
    ascii(fmt_i64_bytes(buf, v))
}

/// Capacity of a [`Line`].
pub const LINE_CAP: usize = 256;

/// A line of text built on the stack from strings, bytes and integers.
///
/// Appending past [`LINE_CAP`] bytes truncates silently (the builder never
/// panics); check [`truncated`](Line::truncated) if that matters.
///
/// ```ignore
/// Line::new().str("init: ").str(name).str(" exited ").int(code).println(console);
/// ```
pub struct Line {
    buf: [u8; LINE_CAP],
    len: usize,
    truncated: bool,
}

impl Default for Line {
    fn default() -> Self {
        Line::new()
    }
}

impl Line {
    /// An empty line.
    pub const fn new() -> Line {
        Line {
            buf: [0; LINE_CAP],
            len: 0,
            truncated: false,
        }
    }

    /// Appends raw bytes.
    pub fn bytes(mut self, b: &[u8]) -> Line {
        self.push(b);
        self
    }

    /// Appends a string.
    pub fn str(self, s: &str) -> Line {
        self.bytes(s.as_bytes())
    }

    /// Appends a signed integer in decimal.
    pub fn int(self, v: i64) -> Line {
        let mut tmp = [0u8; MAX_INT_LEN];
        let s = fmt_i64_bytes(&mut tmp, v);
        self.bytes(s)
    }

    /// Appends an unsigned integer in decimal.
    pub fn uint(self, v: u64) -> Line {
        let mut tmp = [0u8; MAX_INT_LEN];
        let s = fmt_u64_bytes(&mut tmp, v);
        self.bytes(s)
    }

    /// Appends the name of an error (`EBADCAP`, ...).
    pub fn errno(self, e: Errno) -> Line {
        self.str(e.name())
    }

    /// Appends in place (the non-chaining form of [`bytes`](Line::bytes)).
    pub fn push(&mut self, b: &[u8]) {
        let n = copy_into(self.buf.get_mut(self.len..).unwrap_or(&mut []), b);
        self.len += n;
        if n < b.len() {
            self.truncated = true;
        }
    }

    /// The bytes so far.
    pub fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&[])
    }

    /// Number of bytes so far.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing has been appended.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether something did not fit.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Empties the line for reuse.
    pub fn clear(&mut self) {
        self.len = 0;
        self.truncated = false;
    }

    /// Writes the line to `console` as is.
    pub fn print(&self, console: Console) -> Result<(), Errno> {
        console.write(self.as_bytes())
    }

    /// Writes the line plus a newline to `console` in one host call.
    pub fn println(mut self, console: Console) -> Result<(), Errno> {
        if self.len == LINE_CAP {
            // Make room for the newline rather than drop it.
            self.len -= 1;
            self.truncated = true;
        }
        self.push(b"\n");
        self.print(console)
    }
}

/// Copies as much of `src` as fits into `dst`; returns the count. Cannot
/// panic, whatever the lengths.
pub(crate) fn copy_into(dst: &mut [u8], src: &[u8]) -> usize {
    let mut n = 0;
    for (d, s) in dst.iter_mut().zip(src) {
        *d = *s;
        n += 1;
    }
    n
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.push(s.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cap;
    use std::string::ToString;

    #[test]
    fn integers_match_core_fmt() {
        let cases: [i64; 12] = [
            0,
            1,
            -1,
            9,
            10,
            -10,
            12345,
            -98765,
            i64::MAX,
            i64::MIN,
            i32::MAX as i64,
            i32::MIN as i64,
        ];
        for v in cases {
            let mut buf = [0u8; MAX_INT_LEN];
            assert_eq!(fmt_i64(&mut buf, v), v.to_string());
        }
        for v in [0u64, 7, 10, 4_294_967_296, u64::MAX] {
            let mut buf = [0u8; MAX_INT_LEN];
            assert_eq!(fmt_u64(&mut buf, v), v.to_string());
            assert_eq!(fmt_u64_bytes(&mut buf, v), v.to_string().as_bytes());
        }
        let mut buf = [0u8; MAX_INT_LEN];
        assert_eq!(fmt_i64_bytes(&mut buf, -42), b"-42");
        assert_eq!(copy_into(&mut [0u8; 2], b"abc"), 2);
        assert_eq!(copy_into(&mut [0u8; 4], b"abc"), 3);
        assert_eq!(copy_into(&mut [], b"abc"), 0);
    }

    #[test]
    fn line_builds_and_truncates_without_panicking() {
        let l = Line::new()
            .str("init: ")
            .str("ping")
            .str(" exited ")
            .int(-1);
        assert_eq!(l.as_bytes(), b"init: ping exited -1");
        assert_eq!(l.len(), 20);
        assert!(!l.truncated());
        assert!(!l.is_empty());

        let l = Line::new().uint(100).str(" messages").errno(Errno::Again);
        assert_eq!(l.as_bytes(), b"100 messagesEAGAIN");

        let big = [b'a'; LINE_CAP + 10];
        let mut l = Line::new().bytes(&big);
        assert_eq!(l.len(), LINE_CAP);
        assert!(l.truncated());
        l.clear();
        assert!(l.is_empty());
        assert!(!l.truncated());

        // A full line still ends in a newline when printed.
        let l = Line::new().bytes(&big);
        let console = Console::from_cap(Cap::slot(0));
        assert_eq!(l.println(console), Err(Errno::Fault)); // host stub
        let mut l = Line::new().bytes(&big);
        l.len = LINE_CAP;
        l.push(b"\n");
        assert_eq!(l.as_bytes()[LINE_CAP - 1], b'a');

        // fmt::Write works on it too.
        use core::fmt::Write as _;
        let mut l = Line::new();
        let _ = write!(l, "{}-x", 1);
        assert_eq!(l.as_bytes(), b"1-x");
    }
}
