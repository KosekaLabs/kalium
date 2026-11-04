//! Channels: bounded message queues with capability transfer.

use kalium_abi::layout::{RecvHeader, CAP_INDEX_SIZE, RECV_HEADER_SIZE};

use crate::{abi_len, raw, Cap, Errno};

/// Queue depth of a channel; `send` reports [`Errno::Again`] when full.
pub const QUEUE_DEPTH: usize = kalium_abi::limits::CHANNEL_QUEUE_DEPTH;
/// Largest message payload in bytes.
pub const MAX_MESSAGE_LEN: usize = kalium_abi::limits::MAX_MESSAGE_LEN;
/// Most capabilities one message can carry.
pub const MAX_MESSAGE_CAPS: usize = kalium_abi::limits::MAX_MESSAGE_CAPS;

/// A channel; see [`Channel::create`].
pub struct Channel;

impl Channel {
    /// Creates a channel and returns its two ends (`channel_create`).
    ///
    /// The sender has `SEND|DERIVE|TRANSFER`, the receiver
    /// `RECV|WAIT|DERIVE|TRANSFER`; either can be passed to another instance
    /// with [`spawn`](crate::spawn) or [`Sender::send`].
    pub fn create() -> Result<(Sender, Receiver), Errno> {
        let mut out_send: i32 = Cap::NONE.raw();
        let mut out_recv: i32 = Cap::NONE.raw();
        // SAFETY: both out-parameters are live, writable `i32`s for the
        // duration of the call; the kernel writes 4 bytes at each.
        let r = unsafe {
            raw::channel_create(raw::addr_mut(&mut out_send), raw::addr_mut(&mut out_recv))
        };
        Errno::check(r)?;
        if out_send < 0 || out_recv < 0 {
            return Err(Errno::Other(r));
        }
        Ok((Sender(Cap(out_send)), Receiver(Cap(out_recv))))
    }
}

/// The sending end of a channel.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sender(Cap);

impl Sender {
    /// Wraps a capability that is (expected to be) a channel with `SEND`.
    pub const fn from_cap(cap: Cap) -> Sender {
        Sender(cap)
    }

    /// The underlying capability.
    pub const fn cap(self) -> Cap {
        self.0
    }

    /// Sends `bytes` with `caps` attached (`send`). Does not block.
    ///
    /// On success the `caps` are removed from this instance's table (each
    /// needs `TRANSFER`). Fails with [`Errno::Again`] when the queue is
    /// full (yield and retry), [`Errno::Closed`] when no receiver exists,
    /// [`Errno::TooBig`] over [`MAX_MESSAGE_LEN`] / [`MAX_MESSAGE_CAPS`].
    pub fn send(self, bytes: &[u8], caps: &[Cap]) -> Result<(), Errno> {
        let len = abi_len(bytes.len())?;
        let ncaps = abi_len(caps.len())?;
        // SAFETY: `bytes` is a live slice the kernel reads `len` bytes
        // from; `caps` is a live slice of `ncaps` `i32`s (`Cap` is
        // `repr(transparent)` over `i32`) the kernel reads; nothing is
        // written.
        let r = unsafe {
            raw::send(
                self.0.raw(),
                raw::addr(bytes.as_ptr()),
                len,
                raw::addr(caps.as_ptr()),
                ncaps,
            )
        };
        Errno::check(r)
    }
}

/// The receiving end of a channel.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receiver(Cap);

impl Receiver {
    /// Wraps a capability that is (expected to be) a channel with `RECV`.
    pub const fn from_cap(cap: Cap) -> Receiver {
        Receiver(cap)
    }

    /// The underlying capability.
    pub const fn cap(self) -> Cap {
        self.0
    }

    /// Blocks until a message arrives and copies it into `buf` (`recv`).
    ///
    /// Returns `(len, ncaps)`: the number of bytes placed in `buf` and the
    /// number of transferred capabilities placed in `caps_out` (both
    /// clamped to the buffers, so slicing with them cannot panic). Use
    /// [`recv_header`](Receiver::recv_header) to detect truncation.
    /// Fails with [`Errno::Closed`] once the queue is empty and no sender
    /// exists.
    pub fn recv(self, buf: &mut [u8], caps_out: &mut [Cap]) -> Result<(usize, usize), Errno> {
        let hdr = self.recv_header(buf, caps_out)?;
        Ok(clamp(hdr, buf.len(), caps_out.len()))
    }

    /// Like [`recv`](Receiver::recv) but fails with [`Errno::Again`]
    /// instead of blocking (`try_recv`).
    pub fn try_recv(self, buf: &mut [u8], caps_out: &mut [Cap]) -> Result<(usize, usize), Errno> {
        let hdr = self.try_recv_header(buf, caps_out)?;
        Ok(clamp(hdr, buf.len(), caps_out.len()))
    }

    /// [`recv`](Receiver::recv) returning the kernel's header: the *full*
    /// message length and capability count, which exceed `buf.len()` /
    /// `caps_out.len()` when the message was truncated.
    pub fn recv_header(self, buf: &mut [u8], caps_out: &mut [Cap]) -> Result<RecvHeader, Errno> {
        self.recv_raw(buf, caps_out, false)
    }

    /// [`try_recv`](Receiver::try_recv) returning the kernel's header.
    pub fn try_recv_header(
        self,
        buf: &mut [u8],
        caps_out: &mut [Cap],
    ) -> Result<RecvHeader, Errno> {
        self.recv_raw(buf, caps_out, true)
    }

    fn recv_raw(
        self,
        buf: &mut [u8],
        caps_out: &mut [Cap],
        nonblocking: bool,
    ) -> Result<RecvHeader, Errno> {
        let buf_len = abi_len(buf.len())?;
        let ncaps_max = abi_len(caps_out.len())?;
        let mut hdr = [0u8; RECV_HEADER_SIZE];
        let cap = self.0.raw();
        let (bufp, capsp, hdrp) = (
            raw::addr_mut(buf.as_mut_ptr()),
            raw::addr_mut(caps_out.as_mut_ptr()),
            raw::addr_mut(hdr.as_mut_ptr()),
        );
        // SAFETY: `buf` is a live, writable slice of `buf_len` bytes;
        // `caps_out` a live, writable slice of `ncaps_max` `i32`s (`Cap` is
        // `repr(transparent)` over `i32`, and any `i32` is a valid `Cap`);
        // `hdr` a live 8-byte buffer of the header's wire size. The kernel
        // writes at most those lengths at those offsets.
        let r = unsafe {
            if nonblocking {
                raw::try_recv(cap, bufp, buf_len, capsp, ncaps_max, hdrp)
            } else {
                raw::recv(cap, bufp, buf_len, capsp, ncaps_max, hdrp)
            }
        };
        Errno::check(r)?;
        Ok(RecvHeader::from_le_bytes(hdr))
    }
}

/// How much of a message landed in the caller's buffers.
fn clamp(hdr: RecvHeader, buf_len: usize, caps_len: usize) -> (usize, usize) {
    let len = usize::try_from(hdr.len).unwrap_or(usize::MAX).min(buf_len);
    let ncaps = usize::try_from(hdr.ncaps)
        .unwrap_or(usize::MAX)
        .min(caps_len);
    (len, ncaps)
}

/// Blocks until one of `receivers` has a message and returns its position
/// (`wait_any`). All need `WAIT`. An empty slice fails with
/// [`Errno::Inval`] without calling the kernel.
pub fn wait_any(receivers: &[Receiver]) -> Result<usize, Errno> {
    if receivers.is_empty() {
        return Err(Errno::Inval);
    }
    let n = abi_len(receivers.len())?;
    let mut index: i32 = -1;
    // SAFETY: `receivers` is a live slice of `n` `i32`s (`Receiver` is
    // `repr(transparent)` over `Cap` over `i32`) the kernel reads; `index`
    // is a live, writable `i32` the kernel writes.
    let r = unsafe { raw::wait_any(raw::addr(receivers.as_ptr()), n, raw::addr_mut(&mut index)) };
    Errno::check(r)?;
    match usize::try_from(index) {
        Ok(i) if i < receivers.len() => Ok(i),
        _ => Err(Errno::Other(r)),
    }
}

const _: () = assert!(core::mem::size_of::<Cap>() == CAP_INDEX_SIZE);
const _: () = assert!(core::mem::size_of::<Receiver>() == CAP_INDEX_SIZE);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_never_exceeds_buffers() {
        let hdr = RecvHeader { len: 10, ncaps: 2 };
        assert_eq!(clamp(hdr, 64, 4), (10, 2));
        assert_eq!(clamp(hdr, 4, 1), (4, 1));
        assert_eq!(clamp(hdr, 0, 0), (0, 0));
        let hdr = RecvHeader {
            len: u32::MAX,
            ncaps: u32::MAX,
        };
        assert_eq!(clamp(hdr, 8, 4), (8, 4));
    }

    #[test]
    fn wrappers_report_stub_errors() {
        assert_eq!(Channel::create().err(), Some(Errno::Fault));
        let s = Sender::from_cap(Cap::slot(1));
        assert_eq!(s.send(b"hi", &[]), Err(Errno::Fault));
        assert_eq!(s.cap(), Cap(1));
        let r = Receiver::from_cap(Cap::slot(2));
        let mut buf = [0u8; 8];
        let mut caps = [Cap::NONE; 2];
        assert_eq!(r.recv(&mut buf, &mut caps), Err(Errno::Fault));
        assert_eq!(r.try_recv(&mut buf, &mut caps), Err(Errno::Fault));
        assert_eq!(r.recv(&mut [], &mut []), Err(Errno::Fault));
        assert_eq!(wait_any(&[]), Err(Errno::Inval));
        assert_eq!(wait_any(&[r]), Err(Errno::Fault));
    }
}
