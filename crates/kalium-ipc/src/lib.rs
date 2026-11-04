//! IPC channels with capability transfer.
//!
//! A [`Channel`] is a bounded FIFO of [`Message`]s. Each message carries a
//! byte payload and up to a few capabilities that move from the sender's
//! table to the receiver's. The kernel hands out two kinds of capability on a
//! channel object: send ends and receive ends. The channel counts how many of
//! each exist so that it can report the peer as gone:
//!
//! - [`Channel::send`] fails with [`IpcError::Closed`] when no receive end
//!   exists any more (nobody could ever read the message);
//! - [`Channel::try_recv`] fails with [`IpcError::Closed`] only when the queue
//!   is empty *and* no send end exists any more; while messages are queued
//!   they can still be drained, and while a sender exists an empty queue is
//!   reported as [`IpcError::Empty`] (the ABI's `EAGAIN`).
//!
//! The counts are maintained by the kernel through
//! [`Channel::sender_added`], [`Channel::sender_dropped`],
//! [`Channel::receiver_added`] and [`Channel::receiver_dropped`]: one call per
//! capability minted (`channel_create`, `cap_derive`, a cap arriving in a
//! message) or destroyed (`cap_drop`, instance exit, a cap taken out of a
//! table for transfer is still live while in flight, so it does not count as
//! dropped).
//!
//! Blocking is not done here. The kernel's host functions return `Block` and
//! park the calling thread on the channel's [`WaitQueue`]; a later send (or a
//! close) wakes the parked threads in FIFO order.
//!
//! Everything in this crate treats its inputs as untrusted: an oversized
//! message, a full queue or a closed peer is an error value, never a panic.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt;

use kalium_abi::{errno, limits};
pub use kalium_cap::Capability;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from channel operations, each mapping to one ABI errno.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IpcError {
    /// The queue holds `depth` messages already (`EAGAIN`).
    Full,
    /// The payload exceeds the channel's maximum length, or the message
    /// carries more capabilities than the channel allows (`ETOOBIG`).
    TooBig,
    /// The peer is gone: no receive end exists (on send), or the queue is
    /// empty and no send end exists (on receive) (`ECLOSED`).
    Closed,
    /// The queue is empty but a sender still exists, so a message may yet
    /// arrive (`EAGAIN`). A blocking `recv` parks on this.
    Empty,
}

impl IpcError {
    /// The ABI errno this error is reported as.
    pub const fn errno(self) -> i32 {
        match self {
            IpcError::Full => errno::EAGAIN,
            IpcError::TooBig => errno::ETOOBIG,
            IpcError::Closed => errno::ECLOSED,
            IpcError::Empty => errno::EAGAIN,
        }
    }

    /// The symbolic errno name, for diagnostics.
    pub const fn errno_name(self) -> &'static str {
        match errno::name(self.errno()) {
            Some(name) => name,
            None => "?",
        }
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self {
            IpcError::Full => "queue full",
            IpcError::TooBig => "message too big",
            IpcError::Closed => "peer closed",
            IpcError::Empty => "queue empty",
        };
        write!(f, "{} ({})", what, self.errno_name())
    }
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// One message in a channel: a byte payload plus the capabilities that travel
/// with it. The capabilities have already been taken out of the sender's
/// table when the message is queued; the receiver inserts them into its own
/// table on delivery.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    /// The payload. At most the channel's `max_len` bytes.
    pub bytes: Vec<u8>,
    /// Transferred capabilities, in the order the sender listed them. At
    /// most the channel's `max_caps` entries.
    pub caps: Vec<Capability>,
}

impl Message {
    /// A message with a payload and no capabilities.
    pub fn new(bytes: Vec<u8>) -> Message {
        Message {
            bytes,
            caps: Vec::new(),
        }
    }

    /// A message with a payload and capabilities.
    pub fn with_caps(bytes: Vec<u8>, caps: Vec<Capability>) -> Message {
        Message { bytes, caps }
    }

    /// Payload length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// `true` if the payload is empty (a message may still carry caps).
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

/// A bounded message queue with endpoint reference counts.
///
/// Invariants:
/// - `queue.len() <= depth` at all times;
/// - every queued message has `bytes.len() <= max_len` and
///   `caps.len() <= max_caps`;
/// - `senders` / `receivers` are the number of live send / receive
///   capabilities on this object as reported by the kernel; the counts
///   saturate at zero rather than underflow if the kernel over-reports drops.
///
/// A freshly created channel has **zero** endpoints of either kind: the
/// kernel calls [`sender_added`](Channel::sender_added) and
/// [`receiver_added`](Channel::receiver_added) once it has minted the caps
/// that `channel_create` returns. Until then `send` reports `Closed`.
#[derive(Clone, Debug)]
pub struct Channel {
    queue: VecDeque<Message>,
    depth: usize,
    max_len: usize,
    max_caps: usize,
    senders: usize,
    receivers: usize,
}

impl Channel {
    /// Creates an empty channel holding at most `depth` messages of at most
    /// `max_len` bytes and `max_caps` capabilities each, with no endpoints.
    pub fn new(depth: usize, max_len: usize, max_caps: usize) -> Channel {
        Channel {
            queue: VecDeque::new(),
            depth,
            max_len,
            max_caps,
            senders: 0,
            receivers: 0,
        }
    }

    /// Creates a channel with the ABI's M1 limits: depth
    /// [`limits::CHANNEL_QUEUE_DEPTH`], payload [`limits::MAX_MESSAGE_LEN`],
    /// capabilities [`limits::MAX_MESSAGE_CAPS`].
    pub fn with_abi_limits() -> Channel {
        Channel::new(
            limits::CHANNEL_QUEUE_DEPTH,
            limits::MAX_MESSAGE_LEN,
            limits::MAX_MESSAGE_CAPS,
        )
    }

    /// Checks whether a message of `len` bytes carrying `ncaps` capabilities
    /// could be queued right now, without queueing anything. The checks are
    /// the same as [`send`](Channel::send)'s, in the same order. The kernel
    /// calls this before taking the transferred capabilities out of the
    /// sender's table so that a failing send leaves the sender untouched.
    pub fn can_send(&self, len: usize, ncaps: usize) -> Result<(), IpcError> {
        if len > self.max_len || ncaps > self.max_caps {
            return Err(IpcError::TooBig);
        }
        if self.receivers == 0 {
            return Err(IpcError::Closed);
        }
        if self.queue.len() >= self.depth {
            return Err(IpcError::Full);
        }
        Ok(())
    }

    /// Queues a message.
    ///
    /// Checks, in order: the size limits ([`IpcError::TooBig`]), then whether
    /// a receiver exists ([`IpcError::Closed`]), then whether there is room
    /// ([`IpcError::Full`]). On error the message is dropped; use
    /// [`can_send`](Channel::can_send) first or
    /// [`send_or_return`](Channel::send_or_return) if its capabilities must
    /// be recovered.
    pub fn send(&mut self, msg: Message) -> Result<(), IpcError> {
        self.send_or_return(msg).map_err(|(err, _)| err)
    }

    /// Like [`send`](Channel::send), but hands the message back on error so
    /// the kernel can restore the transferred capabilities to the sender.
    pub fn send_or_return(&mut self, msg: Message) -> Result<(), (IpcError, Message)> {
        if let Err(err) = self.can_send(msg.bytes.len(), msg.caps.len()) {
            return Err((err, msg));
        }
        self.queue.push_back(msg);
        Ok(())
    }

    /// Dequeues the oldest message, if any.
    ///
    /// An empty queue is [`IpcError::Closed`] when no sender exists any more
    /// and [`IpcError::Empty`] otherwise. Queued messages are delivered even
    /// after the last sender is gone.
    pub fn try_recv(&mut self) -> Result<Message, IpcError> {
        match self.queue.pop_front() {
            Some(msg) => Ok(msg),
            None if self.senders == 0 => Err(IpcError::Closed),
            None => Err(IpcError::Empty),
        }
    }

    /// A send capability on this channel came into existence.
    pub fn sender_added(&mut self) {
        self.senders = self.senders.saturating_add(1);
    }

    /// A send capability on this channel was destroyed.
    pub fn sender_dropped(&mut self) {
        self.senders = self.senders.saturating_sub(1);
    }

    /// A receive capability on this channel came into existence.
    pub fn receiver_added(&mut self) {
        self.receivers = self.receivers.saturating_add(1);
    }

    /// A receive capability on this channel was destroyed.
    pub fn receiver_dropped(&mut self) {
        self.receivers = self.receivers.saturating_sub(1);
    }

    /// `true` if a `try_recv` would not return [`IpcError::Empty`]: either a
    /// message is queued or the channel is closed for receiving (so the
    /// receiver should be woken to observe `ECLOSED` rather than wait
    /// forever). This is what `wait_any` tests.
    pub fn is_readable(&self) -> bool {
        !self.queue.is_empty() || self.senders == 0
    }

    /// `true` if at least one message is queued.
    pub fn has_message(&self) -> bool {
        !self.queue.is_empty()
    }

    /// `true` if a send would fail with [`IpcError::Full`] (assuming a
    /// receiver exists).
    pub fn is_full(&self) -> bool {
        self.queue.len() >= self.depth
    }

    /// `true` if no receive end exists: sends fail with `Closed`.
    pub fn is_send_closed(&self) -> bool {
        self.receivers == 0
    }

    /// `true` if no send end exists: once drained, receives fail with
    /// `Closed`.
    pub fn is_recv_closed(&self) -> bool {
        self.senders == 0
    }

    /// Number of queued messages.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// `true` if no message is queued.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Maximum number of queued messages.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Maximum payload length.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// Maximum capabilities per message.
    pub fn max_caps(&self) -> usize {
        self.max_caps
    }

    /// Number of live send capabilities.
    pub fn senders(&self) -> usize {
        self.senders
    }

    /// Number of live receive capabilities.
    pub fn receivers(&self) -> usize {
        self.receivers
    }

    /// Removes and returns every queued message, oldest first. The kernel
    /// uses this when it destroys a channel whose queue still holds
    /// capabilities that must be released.
    pub fn drain(&mut self) -> Vec<Message> {
        self.queue.drain(..).collect()
    }

    /// The queued messages, oldest first, without removing them. The kernel
    /// uses this to find capabilities in flight when one is revoked.
    pub fn messages(&self) -> impl Iterator<Item = &Message> + '_ {
        self.queue.iter()
    }
}

// ---------------------------------------------------------------------------
// WaitQueue
// ---------------------------------------------------------------------------

/// A FIFO of parked waiters, keyed by whatever the kernel uses to name a
/// thread (`kalium_sched::ThreadId` in practice; this crate does not depend
/// on the scheduler).
///
/// Parking is idempotent: a waiter that is already in the queue keeps its
/// position rather than being added twice, so a thread that re-issues a
/// blocking host call after a spurious wake-up cannot accumulate entries.
/// Wake-ups are strictly in park order, which is what makes `recv` fair
/// among several blocked receivers.
#[derive(Clone, Debug)]
pub struct WaitQueue<T = u32> {
    waiters: VecDeque<T>,
}

impl<T> Default for WaitQueue<T> {
    fn default() -> Self {
        WaitQueue {
            waiters: VecDeque::new(),
        }
    }
}

impl<T: Copy + PartialEq> WaitQueue<T> {
    /// An empty wait queue.
    pub fn new() -> WaitQueue<T> {
        Self::default()
    }

    /// Parks `thread_id` at the back of the queue. Returns `false` (and does
    /// nothing) if it is already parked.
    pub fn park(&mut self, thread_id: T) -> bool {
        if self.waiters.contains(&thread_id) {
            return false;
        }
        self.waiters.push_back(thread_id);
        true
    }

    /// Removes and returns the longest-waiting thread, if any. The caller is
    /// responsible for making it runnable (`Scheduler::wake`).
    pub fn wake_one(&mut self) -> Option<T> {
        self.waiters.pop_front()
    }

    /// Removes and returns every waiter, longest-waiting first.
    pub fn wake_all(&mut self) -> Vec<T> {
        self.waiters.drain(..).collect()
    }

    /// Removes a specific waiter (a thread that was killed or whose wait was
    /// cancelled). Returns `true` if it was parked.
    pub fn remove(&mut self, thread_id: T) -> bool {
        match self.waiters.iter().position(|w| *w == thread_id) {
            Some(pos) => {
                self.waiters.remove(pos);
                true
            }
            None => false,
        }
    }

    /// `true` if `thread_id` is parked here.
    pub fn contains(&self, thread_id: T) -> bool {
        self.waiters.contains(&thread_id)
    }

    /// Number of parked threads.
    pub fn len(&self) -> usize {
        self.waiters.len()
    }

    /// `true` if nobody is parked.
    pub fn is_empty(&self) -> bool {
        self.waiters.is_empty()
    }

    /// The parked threads in wake order, without removing them.
    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.waiters.iter()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::vec;
    use kalium_cap::{NodeId, ObjectId, ObjectKind, Rights};

    fn cap(id: u64) -> Capability {
        Capability::new(
            ObjectId(id),
            ObjectKind::MemObj,
            Rights::READ | Rights::TRANSFER,
            NodeId(0),
        )
    }

    /// A channel with one sender and one receiver, as `channel_create`
    /// leaves it.
    fn open(depth: usize, max_len: usize, max_caps: usize) -> Channel {
        let mut ch = Channel::new(depth, max_len, max_caps);
        ch.sender_added();
        ch.receiver_added();
        ch
    }

    fn msg(n: u8) -> Message {
        Message::new(vec![n])
    }

    #[test]
    fn errno_mapping() {
        assert_eq!(IpcError::Full.errno(), errno::EAGAIN);
        assert_eq!(IpcError::Empty.errno(), errno::EAGAIN);
        assert_eq!(IpcError::TooBig.errno(), errno::ETOOBIG);
        assert_eq!(IpcError::Closed.errno(), errno::ECLOSED);
        assert_eq!(IpcError::Full.errno_name(), "EAGAIN");
        assert_eq!(IpcError::Closed.errno_name(), "ECLOSED");
        assert_eq!(
            std::format!("{}", IpcError::TooBig),
            "message too big (ETOOBIG)"
        );
    }

    #[test]
    fn new_channel_has_no_endpoints_and_abi_limits() {
        let ch = Channel::with_abi_limits();
        assert_eq!(ch.depth(), limits::CHANNEL_QUEUE_DEPTH);
        assert_eq!(ch.max_len(), limits::MAX_MESSAGE_LEN);
        assert_eq!(ch.max_caps(), limits::MAX_MESSAGE_CAPS);
        assert_eq!(ch.senders(), 0);
        assert_eq!(ch.receivers(), 0);
        assert!(ch.is_send_closed());
        assert!(ch.is_recv_closed());
        assert!(ch.is_empty());
        assert_eq!(ch.len(), 0);
    }

    #[test]
    fn send_without_receiver_is_closed() {
        let mut ch = Channel::new(4, 16, 1);
        ch.sender_added();
        let m = msg(1);
        let (err, back) = ch.send_or_return(m.clone()).unwrap_err();
        assert_eq!(err, IpcError::Closed);
        assert_eq!(back, m);
        assert!(ch.is_empty());
    }

    #[test]
    fn send_recv_roundtrip_preserves_order_and_caps() {
        let mut ch = open(4, 16, 2);
        ch.send(Message::with_caps(vec![1, 2, 3], vec![cap(7), cap(8)]))
            .unwrap();
        ch.send(msg(2)).unwrap();
        assert_eq!(ch.len(), 2);
        assert!(ch.is_readable());
        assert!(ch.has_message());
        let first = ch.try_recv().unwrap();
        assert_eq!(first.bytes, vec![1, 2, 3]);
        assert_eq!(first.caps, vec![cap(7), cap(8)]);
        assert_eq!(first.len(), 3);
        assert!(!first.is_empty());
        let second = ch.try_recv().unwrap();
        assert_eq!(second.bytes, vec![2]);
        assert!(second.caps.is_empty());
        assert_eq!(ch.try_recv(), Err(IpcError::Empty));
    }

    #[test]
    fn full_queue_is_eagain_and_message_is_returned() {
        let mut ch = open(2, 16, 0);
        ch.send(msg(1)).unwrap();
        ch.send(msg(2)).unwrap();
        assert!(ch.is_full());
        let (err, back) = ch.send_or_return(msg(3)).unwrap_err();
        assert_eq!(err, IpcError::Full);
        assert_eq!(back, msg(3));
        assert_eq!(ch.len(), 2);
        // Draining one makes room for exactly one.
        assert_eq!(ch.try_recv().unwrap(), msg(1));
        assert!(!ch.is_full());
        ch.send(msg(3)).unwrap();
        assert_eq!(ch.send(msg(4)).unwrap_err(), IpcError::Full);
    }

    #[test]
    fn zero_depth_channel_is_always_full() {
        let mut ch = open(0, 16, 0);
        assert!(ch.is_full());
        assert_eq!(ch.send(msg(1)).unwrap_err(), IpcError::Full);
    }

    #[test]
    fn oversized_payload_is_etoobig() {
        let mut ch = open(4, 3, 1);
        ch.send(Message::new(vec![0; 3])).unwrap();
        let (err, back) = ch.send_or_return(Message::new(vec![0; 4])).unwrap_err();
        assert_eq!(err, IpcError::TooBig);
        assert_eq!(back.bytes.len(), 4);
        assert_eq!(ch.len(), 1);
    }

    #[test]
    fn too_many_caps_is_etoobig() {
        let mut ch = open(4, 16, 1);
        ch.send(Message::with_caps(vec![], vec![cap(1)])).unwrap();
        let (err, back) = ch
            .send_or_return(Message::with_caps(vec![], vec![cap(1), cap(2)]))
            .unwrap_err();
        assert_eq!(err, IpcError::TooBig);
        assert_eq!(back.caps.len(), 2);
        // A channel that allows no caps rejects a single one.
        let mut none = open(4, 16, 0);
        assert_eq!(
            none.send(Message::with_caps(vec![], vec![cap(1)]))
                .unwrap_err(),
            IpcError::TooBig
        );
        none.send(Message::new(vec![])).unwrap();
    }

    #[test]
    fn too_big_is_reported_before_closed_and_full() {
        // Closed channel, oversized message: TooBig wins (bad argument).
        let mut ch = Channel::new(1, 2, 0);
        ch.sender_added();
        assert_eq!(
            ch.send(Message::new(vec![0; 3])).unwrap_err(),
            IpcError::TooBig
        );
        // Full channel, oversized message: TooBig wins.
        let mut ch = open(1, 2, 0);
        ch.send(msg(1)).unwrap();
        assert_eq!(
            ch.send(Message::new(vec![0; 3])).unwrap_err(),
            IpcError::TooBig
        );
        // Full and closed: Closed wins over Full.
        ch.receiver_dropped();
        assert_eq!(ch.send(msg(2)).unwrap_err(), IpcError::Closed);
    }

    #[test]
    fn empty_with_sender_is_eagain_not_closed() {
        let mut ch = open(4, 16, 0);
        assert_eq!(ch.try_recv(), Err(IpcError::Empty));
        assert!(!ch.is_readable());
        assert!(!ch.is_recv_closed());
    }

    #[test]
    fn recv_after_last_sender_drops_drains_then_closes() {
        let mut ch = open(4, 16, 0);
        ch.send(msg(1)).unwrap();
        ch.send(msg(2)).unwrap();
        ch.sender_dropped();
        assert!(ch.is_recv_closed());
        // Queued messages are still delivered...
        assert!(ch.is_readable());
        assert_eq!(ch.try_recv().unwrap(), msg(1));
        assert_eq!(ch.try_recv().unwrap(), msg(2));
        // ...and only then is the channel closed.
        assert_eq!(ch.try_recv(), Err(IpcError::Closed));
        assert_eq!(ch.try_recv(), Err(IpcError::Closed));
        // Closed-for-receive counts as readable so waiters observe ECLOSED.
        assert!(ch.is_readable());
        assert!(!ch.has_message());
    }

    #[test]
    fn sender_counts_track_multiple_endpoints() {
        let mut ch = open(4, 16, 0);
        ch.sender_added(); // e.g. cap_derive on the send end
        ch.sender_added(); // e.g. a send cap arriving in another instance
        assert_eq!(ch.senders(), 3);
        ch.sender_dropped();
        ch.sender_dropped();
        assert_eq!(ch.try_recv(), Err(IpcError::Empty));
        ch.sender_dropped();
        assert_eq!(ch.try_recv(), Err(IpcError::Closed));
        // A new sender reopens the channel.
        ch.sender_added();
        assert_eq!(ch.try_recv(), Err(IpcError::Empty));
    }

    #[test]
    fn receiver_counts_track_multiple_endpoints() {
        let mut ch = open(4, 16, 0);
        ch.receiver_added();
        ch.receiver_dropped();
        ch.send(msg(1)).unwrap();
        ch.receiver_dropped();
        assert!(ch.is_send_closed());
        assert_eq!(ch.send(msg(2)).unwrap_err(), IpcError::Closed);
        // The queued message survives for a receiver that appears later
        // (e.g. a receive cap that was in flight in another channel).
        ch.receiver_added();
        ch.send(msg(2)).unwrap();
        assert_eq!(ch.try_recv().unwrap(), msg(1));
        assert_eq!(ch.try_recv().unwrap(), msg(2));
    }

    #[test]
    fn dropping_below_zero_saturates() {
        let mut ch = Channel::new(4, 16, 0);
        ch.sender_dropped();
        ch.receiver_dropped();
        assert_eq!(ch.senders(), 0);
        assert_eq!(ch.receivers(), 0);
        ch.sender_added();
        ch.receiver_added();
        assert_eq!(ch.senders(), 1);
        assert_eq!(ch.receivers(), 1);
        ch.send(msg(1)).unwrap();
    }

    #[test]
    fn both_ends_gone() {
        let mut ch = open(4, 16, 0);
        ch.send(msg(1)).unwrap();
        ch.sender_dropped();
        ch.receiver_dropped();
        assert_eq!(ch.send(msg(2)).unwrap_err(), IpcError::Closed);
        assert_eq!(ch.try_recv().unwrap(), msg(1));
        assert_eq!(ch.try_recv(), Err(IpcError::Closed));
    }

    #[test]
    fn drain_returns_everything_in_order() {
        let mut ch = open(4, 16, 1);
        ch.send(Message::with_caps(vec![1], vec![cap(1)])).unwrap();
        ch.send(Message::with_caps(vec![2], vec![cap(2)])).unwrap();
        let all = ch.drain();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].caps, vec![cap(1)]);
        assert_eq!(all[1].caps, vec![cap(2)]);
        assert!(ch.is_empty());
        assert!(ch.drain().is_empty());
    }

    #[test]
    fn fill_to_abi_depth() {
        let mut ch = Channel::with_abi_limits();
        ch.sender_added();
        ch.receiver_added();
        for i in 0..limits::CHANNEL_QUEUE_DEPTH {
            ch.send(Message::new(vec![0; limits::MAX_MESSAGE_LEN]))
                .unwrap_or_else(|_| panic!("send {i}"));
        }
        assert_eq!(ch.send(msg(0)).unwrap_err(), IpcError::Full);
        assert_eq!(
            ch.send(Message::new(vec![0; limits::MAX_MESSAGE_LEN + 1]))
                .unwrap_err(),
            IpcError::TooBig
        );
        let caps: Vec<_> = (0..limits::MAX_MESSAGE_CAPS as u64 + 1).map(cap).collect();
        assert_eq!(
            ch.send(Message::with_caps(vec![], caps)).unwrap_err(),
            IpcError::TooBig
        );
    }

    #[test]
    fn can_send_mirrors_send() {
        let mut ch = open(1, 4, 1);
        assert_eq!(ch.can_send(5, 0), Err(IpcError::TooBig));
        assert_eq!(ch.can_send(0, 2), Err(IpcError::TooBig));
        assert_eq!(ch.can_send(4, 1), Ok(()));
        ch.send(msg(1)).unwrap();
        assert_eq!(ch.can_send(0, 0), Err(IpcError::Full));
        ch.receiver_dropped();
        assert_eq!(ch.can_send(0, 0), Err(IpcError::Closed));
        assert_eq!(ch.can_send(9, 0), Err(IpcError::TooBig));
        // can_send never changes the channel.
        assert_eq!(ch.len(), 1);
    }

    // -- WaitQueue ----------------------------------------------------------

    #[test]
    fn wait_queue_is_fifo() {
        let mut q: WaitQueue<u32> = WaitQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.wake_one(), None);
        assert!(q.park(3));
        assert!(q.park(1));
        assert!(q.park(2));
        assert_eq!(q.len(), 3);
        assert!(q.contains(1));
        assert_eq!(q.wake_one(), Some(3));
        assert_eq!(q.wake_one(), Some(1));
        assert!(q.park(3));
        assert_eq!(q.wake_one(), Some(2));
        assert_eq!(q.wake_one(), Some(3));
        assert_eq!(q.wake_one(), None);
    }

    #[test]
    fn wait_queue_park_is_idempotent() {
        let mut q: WaitQueue<u32> = WaitQueue::new();
        assert!(q.park(5));
        assert!(q.park(6));
        assert!(!q.park(5));
        assert_eq!(q.len(), 2);
        assert_eq!(q.wake_all(), vec![5, 6]);
        assert!(q.is_empty());
        assert!(q.wake_all().is_empty());
    }

    #[test]
    fn wait_queue_remove() {
        let mut q: WaitQueue<u32> = WaitQueue::new();
        q.park(1);
        q.park(2);
        q.park(3);
        assert!(q.remove(2));
        assert!(!q.remove(2));
        assert!(!q.contains(2));
        assert_eq!(q.iter().copied().collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(q.wake_all(), vec![1, 3]);
    }

    /// The kernel's blocking pattern: receivers park while the channel is
    /// not readable; a send wakes one, the last sender going away wakes all
    /// so they observe `Closed` instead of waiting forever.
    #[test]
    fn parked_receivers_are_woken_by_send_and_by_close() {
        let mut ch = open(4, 16, 0);
        let mut q: WaitQueue<u32> = WaitQueue::new();
        // Two receivers find the queue empty and park, in order.
        assert_eq!(ch.try_recv(), Err(IpcError::Empty));
        assert!(!ch.is_readable());
        assert!(q.park(1));
        assert_eq!(ch.try_recv(), Err(IpcError::Empty));
        assert!(q.park(2));
        // A send wakes exactly the first one, which gets the message.
        ch.send(msg(7)).unwrap();
        assert!(ch.is_readable());
        assert_eq!(q.wake_one(), Some(1));
        assert_eq!(ch.try_recv().unwrap(), msg(7));
        assert!(!ch.is_readable());
        assert_eq!(q.len(), 1);
        // The last sender drops: the channel is readable (closed) and every
        // waiter is woken to observe Closed.
        ch.sender_dropped();
        assert!(ch.is_readable());
        assert_eq!(q.wake_all(), vec![2]);
        assert_eq!(ch.try_recv(), Err(IpcError::Closed));
        // A thread re-issuing its wait after a spurious wake is parked once.
        assert!(q.park(2));
        assert!(!q.park(2));
        assert!(q.remove(2));
        assert!(q.is_empty());
    }

    #[test]
    fn wait_queue_generic_over_key_type() {
        #[derive(Clone, Copy, PartialEq, Debug)]
        struct Tid(u32);
        let mut q: WaitQueue<Tid> = WaitQueue::default();
        q.park(Tid(9));
        assert_eq!(q.wake_one(), Some(Tid(9)));
    }
}
