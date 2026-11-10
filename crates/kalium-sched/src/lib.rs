//! Thread scheduler with a real-time (EDF) class and a weighted fair class.
//!
//! The scheduler is pure: it owns no clock, performs no I/O and has no
//! globals. The kernel tells it the time on every call that needs it, asks
//! it which thread to run next and for how long, and reports back what the
//! thread did. The same code is host-tested against a simulated clock.
//!
//! # Policy
//!
//! Two classes, see [`Class`]:
//!
//! - **Real-time.** A thread with a `period_ns` and a `budget_ns`. It is
//!   *released* at every period boundary, that is at `phase + k * period`
//!   where `phase` is the time it was added: its budget is refilled and its
//!   deadline is the end of that period. Among runnable RT threads with
//!   budget left the one with the earliest deadline runs first, ahead of
//!   every fair thread. An RT thread that uses up its budget is *demoted* to
//!   the fair queue (weight 1) until its next release, so a runaway RT
//!   thread cannot starve the rest of the system. Per period the thread is
//!   classified for [`RtStats`]: *met* if it finished its work (blocked,
//!   yielded or slept) before the deadline, else *budget-exhausted* if it
//!   was demoted, else *missed*.
//! - **Fair.** Weighted round robin: each thread runs for
//!   `weight * 1 ms` ([`BASE_SLICE_NS`]) before going to the back of the
//!   queue. A thread preempted mid-slice by an RT release keeps its place
//!   and its remaining slice; a thread that yields gives up the rest of its
//!   slice.
//!
//! [`Scheduler::pick_next`] returns the thread to run and the length of the
//! slice it may run, in nanoseconds: for an RT thread the minimum of its
//! remaining budget and the time to the next RT release that could preempt
//! it (its own, or another RT thread's whose deadline would be earlier); for
//! a fair thread the minimum of its remaining round-robin slice and the time
//! to the next RT release of any kind, so a fair thread never delays an RT
//! release. The kernel converts the slice to interpreter fuel with
//! [`FuelRate`].
//!
//! Sleepers wake when the clock reaches their wake time, which the scheduler
//! notices on [`Scheduler::tick`] (and on any other call that carries the
//! time). [`Scheduler::next_deadline_ns`] is the earliest instant at which
//! the kernel must call back: the next RT release, the end of the running
//! slice or the earliest sleeper wake, whichever is first. That is what the
//! kernel programs the timer with.
//!
//! # Data structures
//!
//! - `threads`: a `BTreeMap<ThreadId, Thread>` with every thread the
//!   scheduler knows about. A `Thread` holds the class, the [`State`], the
//!   remaining round-robin slice and, for RT threads, the `RtState` with
//!   the current period's release, deadline, remaining budget and the
//!   statistics.
//! - `fair_queue`: a `VecDeque<ThreadId>` in round-robin order holding the
//!   fair threads and the demoted RT threads that are ready to run. Each
//!   thread is in it at most once (`Thread::in_fair_queue`); entries whose
//!   thread is no longer eligible (blocked, sleeping, exited, or an RT thread
//!   re-released since its demotion) are skipped and dropped when they reach
//!   the front, so blocking never needs a linear removal.
//! - Runnable RT threads are not kept in a separate structure: the number of
//!   RT threads is small and a scan of `threads` for the earliest deadline is
//!   cheaper than keeping a heap consistent across releases and demotions.
//! - `current`: the thread handed out by the last `pick_next` and the end of
//!   its slice, until `account` reports it back.
//!
//! # Contract with the kernel
//!
//! For every thread returned by `pick_next` the kernel runs it and then calls
//! `account` exactly once with the time it ran and the [`RunOutcome`]. Host
//! calls that block the thread call [`Scheduler::block`] or
//! [`Scheduler::sleep`] either before or after that `account`; something that
//! makes it runnable again calls [`Scheduler::wake`]. Nothing here panics on
//! an unknown thread id or an inconsistent call sequence: such calls are
//! ignored, because they come from kernel bugs rather than from untrusted
//! input, and a scheduler that panics takes the whole system with it.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Identifies a thread. Allocated by the kernel; the scheduler only compares
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(pub u32);

/// The scheduling class of a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// Weighted round robin. The slice is `weight * BASE_SLICE_NS`; a weight
    /// of `0` is treated as `1`.
    Fair {
        /// Relative share of CPU time among fair threads.
        weight: u32,
    },
    /// Periodic real-time: released every `period_ns` with `budget_ns` of
    /// CPU time and a deadline at the end of the period.
    RealTime {
        /// Period length in nanoseconds. Must be non-zero.
        period_ns: u64,
        /// CPU budget per period in nanoseconds. Must be non-zero and at
        /// most `period_ns`.
        budget_ns: u64,
    },
}

impl Class {
    /// `true` if the class can be scheduled as given: a fair class always;
    /// a real-time class only with `0 < budget_ns <= period_ns`. The kernel
    /// uses this to reject bad `spawn` arguments with `EINVAL`. [`Scheduler::add`]
    /// clamps an invalid real-time class rather than failing, see there.
    pub const fn is_valid(&self) -> bool {
        match *self {
            Class::Fair { .. } => true,
            Class::RealTime { period_ns, budget_ns } => period_ns > 0 && budget_ns > 0 && budget_ns <= period_ns,
        }
    }

    /// `true` for [`Class::RealTime`].
    pub const fn is_real_time(&self) -> bool {
        matches!(self, Class::RealTime { .. })
    }
}

/// The state of a thread as the scheduler sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    /// Runnable and waiting to be picked.
    Ready,
    /// Handed out by `pick_next` and not yet accounted.
    Running,
    /// Waiting for a `wake`.
    Blocked,
    /// Waiting for the clock to reach `until_ns` (or for a `wake`).
    Sleeping {
        /// Wake time in nanoseconds.
        until_ns: u64,
    },
    /// Finished. Stays in the table (so `rt_stats` still answers) until
    /// `remove`.
    Exited,
}

/// What a thread did during the slice it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunOutcome {
    /// The slice ran out (or the kernel preempted it). The thread is still
    /// runnable and keeps its remaining slice, if any.
    Preempted,
    /// The thread gave up the rest of its slice. Still runnable.
    Yielded,
    /// The thread blocked on something; `wake` makes it runnable again.
    Blocked,
    /// The thread went to sleep; `sleep` carries the wake time.
    Slept,
    /// The thread is done. It stays [`State::Exited`] until `remove`.
    Exited,
}

/// Per-period statistics of a real-time thread.
///
/// Every completed period is counted in exactly one of `met`, `missed` and
/// `budget_exhausted`, so `periods == met + missed + budget_exhausted`. The
/// period in progress is not counted yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RtStats {
    /// Completed periods.
    pub periods: u64,
    /// Periods in which the thread finished its work (blocked, yielded or
    /// slept) before the deadline.
    pub met: u64,
    /// Periods in which the thread neither finished nor exhausted its
    /// budget before the deadline: it was still runnable, or blocked or
    /// asleep the whole time without ever finishing.
    pub missed: u64,
    /// Periods in which the thread used its whole budget without finishing
    /// and was demoted to the fair queue. Not a missed deadline in the
    /// accounting sense: the thread asked for more CPU than it was admitted
    /// with, and the scheduler protected the other threads.
    pub budget_exhausted: u64,
}

impl RtStats {
    /// `met` as a fraction of `periods` in parts per million; `1_000_000`
    /// when no period has completed.
    pub const fn met_ppm(&self) -> u64 {
        if self.periods == 0 {
            1_000_000
        } else {
            // periods fits u64, met <= periods; the product fits u128.
            ((self.met as u128 * 1_000_000) / self.periods as u128) as u64
        }
    }
}

/// Base round-robin slice: 1 ms. A fair thread of weight `w` runs
/// `w * BASE_SLICE_NS` per turn.
pub const BASE_SLICE_NS: u64 = 1_000_000;

// ---------------------------------------------------------------------------
// Fuel conversion
// ---------------------------------------------------------------------------

/// A calibrated interpreter speed: `fuel` units of fuel take `ns`
/// nanoseconds. Built from a measurement ("the calibration loop burned
/// 200 000 fuel in 1 000 000 ns") so it needs no floating point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FuelRate {
    fuel: u64,
    ns: u64,
}

impl FuelRate {
    /// `fuel` units per `ns` nanoseconds. A zero in either place is treated
    /// as one so the rate is always usable.
    pub const fn new(fuel: u64, ns: u64) -> FuelRate {
        FuelRate { fuel: if fuel == 0 { 1 } else { fuel }, ns: if ns == 0 { 1 } else { ns } }
    }

    /// Fuel units measured in the calibration.
    pub const fn fuel(&self) -> u64 {
        self.fuel
    }

    /// Nanoseconds measured in the calibration.
    pub const fn ns(&self) -> u64 {
        self.ns
    }

    /// The fuel that runs for about `slice_ns` nanoseconds, rounded down but
    /// never below `1` so a positive slice always makes progress.
    pub const fn fuel_for_ns(&self, slice_ns: u64) -> u64 {
        let fuel = (slice_ns as u128 * self.fuel as u128) / self.ns as u128;
        if fuel == 0 {
            1
        } else if fuel > u64::MAX as u128 {
            u64::MAX
        } else {
            fuel as u64
        }
    }

    /// The time `fuel` units take, rounded up.
    pub const fn ns_for_fuel(&self, fuel: u64) -> u64 {
        let num = fuel as u128 * self.ns as u128;
        let ns = num.div_ceil(self.fuel as u128);
        if ns > u64::MAX as u128 { u64::MAX } else { ns as u64 }
    }
}

/// Converts a slice returned by [`Scheduler::pick_next`] to interpreter
/// fuel at the given calibrated rate. Same as [`FuelRate::fuel_for_ns`].
pub const fn fuel_for_slice(slice_ns: u64, rate: FuelRate) -> u64 {
    rate.fuel_for_ns(slice_ns)
}

// ---------------------------------------------------------------------------
// Internal records
// ---------------------------------------------------------------------------

/// The real-time bookkeeping of one thread.
///
/// Period `k` spans `[phase + k * period, phase + (k + 1) * period)`. The
/// current period is `period_index`; its bounds are cached in `release_ns`
/// and `deadline_ns` (the deadline is also the next release time).
#[derive(Clone, Debug)]
struct RtState {
    /// Release time of period 0: when the thread was added.
    phase_ns: u64,
    period_ns: u64,
    budget_ns: u64,
    /// `k` of the current period.
    period_index: u64,
    /// Start of the current period.
    release_ns: u64,
    /// End of the current period and the next release time.
    deadline_ns: u64,
    /// Budget not yet used in the current period.
    budget_left_ns: u64,
    /// The budget ran out in this period and the thread competes in the fair
    /// queue until the next release.
    demoted: bool,
    /// The thread blocked, yielded or slept at least once in this period
    /// before its deadline.
    finished: bool,
    stats: RtStats,
}

impl RtState {
    fn new(period_ns: u64, budget_ns: u64, now_ns: u64) -> RtState {
        RtState {
            phase_ns: now_ns,
            period_ns,
            budget_ns,
            period_index: 0,
            release_ns: now_ns,
            deadline_ns: now_ns.saturating_add(period_ns),
            budget_left_ns: budget_ns,
            demoted: false,
            finished: false,
            stats: RtStats::default(),
        }
    }

    /// Closes the current period, counts the periods wholly skipped if the
    /// clock jumped, and opens the period that contains `now_ns`.
    /// Precondition: `now_ns >= self.deadline_ns`.
    fn release(&mut self, now_ns: u64) {
        self.stats.periods += 1;
        if self.finished {
            self.stats.met += 1;
        } else if self.demoted {
            self.stats.budget_exhausted += 1;
        } else {
            self.stats.missed += 1;
        }
        let elapsed = now_ns.saturating_sub(self.phase_ns);
        let k = elapsed / self.period_ns;
        // Periods between the one just closed and the one containing `now`
        // passed without the thread ever being scheduled: missed.
        let skipped = k.saturating_sub(self.period_index + 1);
        self.stats.periods += skipped;
        self.stats.missed += skipped;
        self.period_index = k;
        self.release_ns = self.phase_ns.saturating_add(k.saturating_mul(self.period_ns));
        self.deadline_ns = self.release_ns.saturating_add(self.period_ns);
        self.budget_left_ns = self.budget_ns;
        self.demoted = false;
        self.finished = false;
    }

    /// Charges `ran_ns` of CPU time to the current budget.
    fn charge(&mut self, ran_ns: u64) {
        self.budget_left_ns = self.budget_left_ns.saturating_sub(ran_ns);
        if self.budget_left_ns == 0 {
            self.demoted = true;
        }
    }

    /// `true` if the thread competes as RT right now (budget left, not
    /// demoted).
    fn has_budget(&self) -> bool {
        !self.demoted && self.budget_left_ns > 0
    }
}

/// One thread's record.
#[derive(Clone, Debug)]
struct Thread {
    class: Class,
    state: State,
    /// Round-robin slice not yet used, for fair threads and demoted RT
    /// threads. Refilled to the full slice when it reaches zero.
    slice_left_ns: u64,
    /// There is an entry for this thread in `Scheduler::fair_queue`.
    in_fair_queue: bool,
    /// Present for `Class::RealTime` threads.
    rt: Option<RtState>,
}

impl Thread {
    fn full_slice_ns(&self) -> u64 {
        match self.class {
            Class::Fair { weight } => BASE_SLICE_NS.saturating_mul(weight.max(1) as u64),
            Class::RealTime { .. } => BASE_SLICE_NS,
        }
    }

    /// `true` if the thread would be served from the fair queue when ready:
    /// fair threads and demoted RT threads.
    fn is_fair_candidate(&self) -> bool {
        match &self.rt {
            None => true,
            Some(rt) => !rt.has_budget(),
        }
    }

    /// `true` if the thread would be served as RT when ready.
    fn is_rt_candidate(&self) -> bool {
        matches!(&self.rt, Some(rt) if rt.has_budget())
    }
}

/// The thread handed out by the last `pick_next`.
#[derive(Clone, Copy, Debug)]
struct Current {
    id: ThreadId,
    /// When its slice ends.
    slice_end_ns: u64,
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// The scheduler. See the crate documentation for the policy and the data
/// structures.
#[derive(Clone, Debug, Default)]
pub struct Scheduler {
    threads: BTreeMap<ThreadId, Thread>,
    fair_queue: VecDeque<ThreadId>,
    current: Option<Current>,
}

impl Scheduler {
    /// An empty scheduler.
    pub fn new() -> Scheduler {
        Scheduler::default()
    }

    /// Adds a thread in state `Ready`. An RT thread's first period starts at
    /// `now_ns`.
    ///
    /// An invalid RT class (see [`Class::is_valid`]) is clamped rather than
    /// rejected, because the plan's `add` is infallible: a zero period
    /// becomes `1`, a zero budget becomes `1`, a budget above the period
    /// becomes the period. The kernel validates before it gets here.
    /// Re-adding an id that already exists replaces the old thread.
    pub fn add(&mut self, id: ThreadId, class: Class, now_ns: u64) {
        if self.threads.contains_key(&id) {
            self.remove(id);
        }
        let (class, rt) = match class {
            Class::Fair { weight } => (Class::Fair { weight }, None),
            Class::RealTime { period_ns, budget_ns } => {
                let period_ns = period_ns.max(1);
                let budget_ns = budget_ns.clamp(1, period_ns);
                (Class::RealTime { period_ns, budget_ns }, Some(RtState::new(period_ns, budget_ns, now_ns)))
            }
        };
        let mut thread = Thread { class, state: State::Ready, slice_left_ns: 0, in_fair_queue: false, rt };
        thread.slice_left_ns = thread.full_slice_ns();
        self.threads.insert(id, thread);
        self.enqueue_if_fair(id);
    }

    /// Removes a thread in any state. Returns `false` if it was unknown.
    pub fn remove(&mut self, id: ThreadId) -> bool {
        if self.threads.remove(&id).is_none() {
            return false;
        }
        // Purge the fair queue: the kernel reuses the slot, and so the
        // `ThreadId`, of an exited instance, and a stale entry would give
        // the new thread a second turn. The `current` record is cleared so
        // `next_deadline_ns` stops reporting its slice.
        self.fair_queue.retain(|&t| t != id);
        if self.current.map(|c| c.id) == Some(id) {
            self.current = None;
        }
        true
    }

    /// The number of threads known to the scheduler, in any state.
    pub fn len(&self) -> usize {
        self.threads.len()
    }

    /// `true` if no thread is known.
    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    /// `true` if `id` is known.
    pub fn contains(&self, id: ThreadId) -> bool {
        self.threads.contains_key(&id)
    }

    /// The state of a thread.
    pub fn state(&self, id: ThreadId) -> Option<State> {
        self.threads.get(&id).map(|t| t.state)
    }

    /// The class of a thread (after clamping, see [`Scheduler::add`]).
    pub fn class(&self, id: ThreadId) -> Option<Class> {
        self.threads.get(&id).map(|t| t.class)
    }

    /// The thread most recently handed out by `pick_next` and not yet
    /// accounted, if any.
    pub fn current(&self) -> Option<ThreadId> {
        self.current.map(|c| c.id)
    }

    /// Every thread id, ascending.
    pub fn thread_ids(&self) -> Vec<ThreadId> {
        self.threads.keys().copied().collect()
    }

    /// Statistics of an RT thread; `None` for unknown or fair threads.
    pub fn rt_stats(&self, id: ThreadId) -> Option<RtStats> {
        self.threads.get(&id)?.rt.as_ref().map(|rt| rt.stats)
    }

    /// The current period's deadline of an RT thread.
    pub fn rt_deadline_ns(&self, id: ThreadId) -> Option<u64> {
        self.threads.get(&id)?.rt.as_ref().map(|rt| rt.deadline_ns)
    }

    /// The budget an RT thread still has in its current period.
    pub fn rt_budget_left_ns(&self, id: ThreadId) -> Option<u64> {
        self.threads.get(&id)?.rt.as_ref().map(|rt| rt.budget_left_ns)
    }

    /// `true` if an RT thread is currently demoted to the fair queue.
    pub fn is_demoted(&self, id: ThreadId) -> bool {
        matches!(self.threads.get(&id), Some(Thread { rt: Some(rt), .. }) if rt.demoted)
    }

    // -- time ----------------------------------------------------------------

    /// Advances the scheduler's view of time: releases RT periods whose
    /// boundary has passed and wakes sleepers that are due.
    fn advance(&mut self, now_ns: u64) {
        let mut to_enqueue: Vec<ThreadId> = Vec::new();
        for (&id, thread) in self.threads.iter_mut() {
            if thread.state == State::Exited {
                continue;
            }
            if let Some(rt) = thread.rt.as_mut()
                && now_ns >= rt.deadline_ns
            {
                rt.release(now_ns);
            }
            if let State::Sleeping { until_ns } = thread.state
                && now_ns >= until_ns
            {
                thread.state = State::Ready;
                thread.slice_left_ns = thread.full_slice_ns();
                to_enqueue.push(id);
            }
        }
        for id in to_enqueue {
            self.enqueue_if_fair(id);
        }
    }

    /// Timer tick: wakes due sleepers and releases due RT periods. Call
    /// `should_preempt` afterwards to learn whether the running thread must
    /// give way.
    pub fn tick(&mut self, now_ns: u64) {
        self.advance(now_ns);
    }

    /// `true` if the thread handed out by the last `pick_next` should be
    /// preempted now: its slice is over, or an RT thread with an earlier
    /// deadline (or any RT thread, if a fair or demoted one is running)
    /// became runnable.
    pub fn should_preempt(&self, now_ns: u64) -> bool {
        let Some(current) = self.current else {
            return false;
        };
        if now_ns >= current.slice_end_ns {
            return true;
        }
        let Some(running) = self.threads.get(&current.id) else {
            return true;
        };
        if running.state != State::Running {
            // Blocked or exited while running: nothing left to run.
            return true;
        }
        let running_deadline = match &running.rt {
            Some(rt) if rt.has_budget() => rt.deadline_ns,
            _ => u64::MAX,
        };
        self.threads.iter().any(|(&id, t)| {
            id != current.id
                && t.state == State::Ready
                && matches!(&t.rt, Some(rt) if rt.has_budget() && (rt.deadline_ns, id) < (running_deadline, current.id))
        })
    }

    /// The earliest instant the kernel must call back for: the next RT
    /// release, the end of the running slice or the earliest sleeper wake.
    /// `None` when nothing is pending (no RT threads, no sleepers, nothing
    /// running).
    pub fn next_deadline_ns(&self) -> Option<u64> {
        let mut earliest: Option<u64> = self.current.map(|c| c.slice_end_ns);
        let mut consider = |t: u64| {
            earliest = Some(match earliest {
                Some(e) => e.min(t),
                None => t,
            });
        };
        for thread in self.threads.values() {
            if thread.state == State::Exited {
                continue;
            }
            if let Some(rt) = &thread.rt {
                consider(rt.deadline_ns);
            }
            if let State::Sleeping { until_ns } = thread.state {
                consider(until_ns);
            }
        }
        earliest
    }

    // -- picking ---------------------------------------------------------------

    /// Chooses the thread to run now and the slice it may run, in
    /// nanoseconds. `None` if nothing is runnable.
    ///
    /// If the thread from the previous `pick_next` was never accounted it is
    /// treated as preempted with no time used.
    pub fn pick_next(&mut self, now_ns: u64) -> Option<(ThreadId, u64)> {
        self.advance(now_ns);
        if let Some(current) = self.current.take()
            && let Some(t) = self.threads.get_mut(&current.id)
            && t.state == State::Running
        {
            t.state = State::Ready;
            self.requeue_front(current.id);
        }

        let (id, slice) = match self.pick_rt(now_ns) {
            Some(pick) => pick,
            None => self.pick_fair(now_ns)?,
        };
        let slice = slice.max(1);
        if let Some(t) = self.threads.get_mut(&id) {
            t.state = State::Running;
        }
        self.current = Some(Current { id, slice_end_ns: now_ns.saturating_add(slice) });
        Some((id, slice))
    }

    /// The ready RT thread with the earliest deadline and its slice.
    fn pick_rt(&self, now_ns: u64) -> Option<(ThreadId, u64)> {
        let mut best: Option<(u64, ThreadId)> = None;
        for (&id, t) in &self.threads {
            if t.state != State::Ready {
                continue;
            }
            if let Some(rt) = &t.rt
                && rt.has_budget()
            {
                let key = (rt.deadline_ns, id);
                if best.is_none_or(|b| key < b) {
                    best = Some(key);
                }
            }
        }
        let (deadline, id) = best?;
        let rt = self.threads.get(&id)?.rt.as_ref()?;
        // Run until the budget is spent, this thread's own next release, or
        // the release of another RT thread whose new deadline would be
        // earlier than ours (it would preempt us by EDF).
        let mut end = rt.deadline_ns;
        for (&other_id, other) in &self.threads {
            if other_id == id || other.state == State::Exited {
                continue;
            }
            if let Some(o) = &other.rt {
                let next_release = o.deadline_ns;
                let next_deadline = next_release.saturating_add(o.period_ns);
                if (next_deadline, other_id) < (deadline, id) && next_release < end {
                    end = next_release;
                }
            }
        }
        let until_release = end.saturating_sub(now_ns);
        Some((id, rt.budget_left_ns.min(until_release)))
    }

    /// The next eligible thread from the fair queue and its slice.
    fn pick_fair(&mut self, now_ns: u64) -> Option<(ThreadId, u64)> {
        let next_release = self.next_rt_release_ns();
        while let Some(id) = self.fair_queue.pop_front() {
            let Some(t) = self.threads.get_mut(&id) else {
                continue; // removed while queued
            };
            t.in_fair_queue = false;
            if t.state != State::Ready || !t.is_fair_candidate() {
                continue; // stale entry: blocked, exited, or re-released RT
            }
            if t.slice_left_ns == 0 {
                t.slice_left_ns = t.full_slice_ns();
            }
            let mut slice = t.slice_left_ns;
            if let Some(release) = next_release {
                slice = slice.min(release.saturating_sub(now_ns));
            }
            return Some((id, slice));
        }
        None
    }

    /// The earliest next release among all RT threads.
    fn next_rt_release_ns(&self) -> Option<u64> {
        self.threads.values().filter(|t| t.state != State::Exited).filter_map(|t| t.rt.as_ref().map(|rt| rt.deadline_ns)).min()
    }

    // -- queue maintenance ----------------------------------------------------

    /// Appends a ready fair or demoted thread to the fair queue.
    fn enqueue_if_fair(&mut self, id: ThreadId) {
        if let Some(t) = self.threads.get_mut(&id)
            && t.state == State::Ready
            && t.is_fair_candidate()
            && !t.in_fair_queue
        {
            t.in_fair_queue = true;
            self.fair_queue.push_back(id);
        }
    }

    /// Puts a preempted fair or demoted thread back at the front if it has
    /// slice left, else at the back with a fresh slice.
    fn requeue_front(&mut self, id: ThreadId) {
        if let Some(t) = self.threads.get_mut(&id) {
            if t.state != State::Ready || !t.is_fair_candidate() || t.in_fair_queue {
                return;
            }
            t.in_fair_queue = true;
            if t.slice_left_ns > 0 {
                self.fair_queue.push_front(id);
            } else {
                t.slice_left_ns = t.full_slice_ns();
                self.fair_queue.push_back(id);
            }
        }
    }

    // -- accounting -------------------------------------------------------------

    /// Reports that `id` ran for `ran_ns` ending at `now_ns` with `outcome`.
    ///
    /// Charges the time to the thread's RT budget or round-robin slice (a
    /// run that straddles a period boundary is split between the two
    /// periods), records whether the thread finished its work this period,
    /// and moves it to the state the outcome implies unless `block`/`sleep`
    /// already did.
    pub fn account(&mut self, id: ThreadId, ran_ns: u64, now_ns: u64, outcome: RunOutcome) {
        if self.current.map(|c| c.id) == Some(id) {
            self.current = None;
        }
        let start_ns = now_ns.saturating_sub(ran_ns);
        let mut ran_as_rt = false;
        if let Some(t) = self.threads.get_mut(&id) {
            ran_as_rt = t.is_rt_candidate();
            if let Some(rt) = t.rt.as_mut() {
                // Charge the run period by period.
                let mut at = start_ns;
                let mut left = ran_ns;
                while left > 0 {
                    if at >= rt.deadline_ns {
                        rt.release(at);
                        continue;
                    }
                    let part = left.min(rt.deadline_ns - at);
                    rt.charge(part);
                    at += part;
                    left -= part;
                }
                if now_ns >= rt.deadline_ns {
                    rt.release(now_ns);
                }
            }
            if ran_as_rt {
                // RT time comes out of the budget, not the fair slice. A
                // thread demoted by this run joins the fair queue at the
                // back with a full slice.
                if !t.is_rt_candidate() {
                    t.slice_left_ns = t.full_slice_ns();
                }
            } else {
                t.slice_left_ns = t.slice_left_ns.saturating_sub(ran_ns);
            }
        }
        self.advance(now_ns);

        let Some(t) = self.threads.get_mut(&id) else {
            return;
        };
        let finished = matches!(outcome, RunOutcome::Yielded | RunOutcome::Blocked | RunOutcome::Slept);
        if finished && let Some(rt) = t.rt.as_mut() {
            rt.finished = true;
        }
        match outcome {
            RunOutcome::Preempted => {
                if t.state == State::Running {
                    t.state = State::Ready;
                    if ran_as_rt {
                        self.enqueue_if_fair(id);
                    } else {
                        self.requeue_front(id);
                    }
                }
            }
            RunOutcome::Yielded => {
                t.slice_left_ns = 0;
                if t.state == State::Running {
                    t.state = State::Ready;
                    t.slice_left_ns = t.full_slice_ns();
                    self.enqueue_if_fair(id);
                }
            }
            RunOutcome::Blocked | RunOutcome::Slept => {
                // `block`/`sleep` may have run already (state is not Running
                // then) or may follow; either way the thread is not runnable.
                // A `Slept` without a `sleep` call parks the thread until a
                // `wake`.
                if t.state == State::Running {
                    t.state = State::Blocked;
                }
            }
            RunOutcome::Exited => {
                t.state = State::Exited;
            }
        }
    }

    /// Marks a thread as blocked. It is picked again only after `wake`.
    pub fn block(&mut self, id: ThreadId) {
        if let Some(t) = self.threads.get_mut(&id)
            && t.state != State::Exited
        {
            t.state = State::Blocked;
        }
    }

    /// Puts a thread to sleep until `until_ns`. It becomes ready on the first
    /// `tick` (or other time-carrying call) at or after that, or on `wake`.
    pub fn sleep(&mut self, id: ThreadId, until_ns: u64) {
        if let Some(t) = self.threads.get_mut(&id)
            && t.state != State::Exited
        {
            t.state = State::Sleeping { until_ns };
        }
    }

    /// Makes a blocked or sleeping thread ready. A thread that is ready,
    /// running or exited is left alone.
    pub fn wake(&mut self, id: ThreadId, now_ns: u64) {
        self.advance(now_ns);
        if let Some(t) = self.threads.get_mut(&id)
            && matches!(t.state, State::Blocked | State::Sleeping { .. })
        {
            t.state = State::Ready;
            t.slice_left_ns = t.full_slice_ns();
            self.enqueue_if_fair(id);
        }
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
    use std::collections::BTreeMap as Map;

    const US: u64 = 1_000;
    const MS: u64 = 1_000_000;

    fn fair(weight: u32) -> Class {
        Class::Fair { weight }
    }

    fn rt(period_ns: u64, budget_ns: u64) -> Class {
        Class::RealTime { period_ns, budget_ns }
    }

    /// What a simulated thread does when given the CPU.
    #[derive(Clone, Copy)]
    enum Behavior {
        /// Never finishes; uses the whole slice every time.
        Busy,
        /// Does `work_ns` of work per period, then sleeps until its next
        /// release. The simulation tracks the remaining work.
        Periodic { work_ns: u64 },
    }

    struct SimThread {
        behavior: Behavior,
        /// Work left in the current period, for the periodic behaviors.
        work_left: u64,
        ran_total: u64,
    }

    /// A deterministic simulation of the kernel's run loop against the
    /// scheduler: the clock advances only by the time threads run and by
    /// idle jumps to the next deadline.
    struct Sim {
        sched: Scheduler,
        now: u64,
        threads: Map<ThreadId, SimThread>,
        picks: Vec<(u64, ThreadId, u64)>,
    }

    impl Sim {
        fn new() -> Sim {
            Sim { sched: Scheduler::new(), now: 0, threads: Map::new(), picks: Vec::new() }
        }

        fn add(&mut self, id: u32, class: Class, behavior: Behavior) -> ThreadId {
            let id = ThreadId(id);
            self.sched.add(id, class, self.now);
            let work_left = match behavior {
                Behavior::Busy => u64::MAX,
                Behavior::Periodic { work_ns } => work_ns,
            };
            self.threads.insert(id, SimThread { behavior, work_left, ran_total: 0 });
            id
        }

        /// One scheduling decision. Returns `false` if nothing ran and
        /// nothing is pending.
        fn step(&mut self) -> bool {
            let Some((id, slice)) = self.sched.pick_next(self.now) else {
                // Idle: jump to the next deadline and tick.
                match self.sched.next_deadline_ns() {
                    Some(t) => {
                        assert!(t >= self.now, "deadline in the past");
                        self.now = t;
                        self.sched.tick(self.now);
                        return true;
                    }
                    None => return false,
                }
            };
            assert!(slice > 0);
            self.picks.push((self.now, id, slice));
            let st = self.threads.get_mut(&id).expect("picked unknown thread");
            let ran = slice.min(st.work_left);
            st.ran_total += ran;
            st.work_left -= ran;
            self.now += ran;
            let done = st.work_left == 0;
            match st.behavior {
                Behavior::Busy => self.sched.account(id, ran, self.now, RunOutcome::Preempted),
                Behavior::Periodic { work_ns } => {
                    if done {
                        let until = self.sched.rt_deadline_ns(id).expect("periodic is RT");
                        st.work_left = work_ns;
                        self.sched.sleep(id, until);
                        self.sched.account(id, ran, self.now, RunOutcome::Slept);
                    } else {
                        self.sched.account(id, ran, self.now, RunOutcome::Preempted);
                    }
                }
            }
            true
        }

        fn run_until(&mut self, end: u64) {
            while self.now < end {
                if !self.step() {
                    // Nothing runnable and nothing pending: time stands
                    // still. Advance to `end` by ticking.
                    self.now = end;
                    self.sched.tick(self.now);
                }
            }
        }

        fn ran(&self, id: ThreadId) -> u64 {
            self.threads[&id].ran_total
        }
    }

    // -- (1) RT guarantee under fair load ------------------------------------

    #[test]
    fn rt_thread_meets_every_period_under_fair_load() {
        let mut sim = Sim::new();
        let rt_id = sim.add(1, rt(2 * MS, 500 * US), Behavior::Periodic { work_ns: 300 * US });
        let f1 = sim.add(2, fair(1), Behavior::Busy);
        let f2 = sim.add(3, fair(1), Behavior::Busy);
        sim.run_until(1_000 * MS);

        let stats = sim.sched.rt_stats(rt_id).unwrap();
        assert!(stats.periods >= 499, "periods {}", stats.periods);
        assert_eq!(stats.missed, 0, "{stats:?}");
        assert_eq!(stats.budget_exhausted, 0, "{stats:?}");
        assert_eq!(stats.met, stats.periods, "{stats:?}");
        assert_eq!(stats.met_ppm(), 1_000_000);
        // The RT thread ran exactly its work, the fair threads got the rest.
        assert!(sim.ran(rt_id) >= stats.periods * 300 * US);
        assert!(sim.ran(rt_id) <= (stats.periods + 1) * 300 * US);
        assert!(sim.ran(f1) > 300 * MS, "fair 1 ran {}", sim.ran(f1));
        assert!(sim.ran(f2) > 300 * MS, "fair 2 ran {}", sim.ran(f2));
        // Every RT pick happened exactly at a release and was never late.
        for &(at, id, slice) in &sim.picks {
            if id == rt_id {
                assert_eq!(at % (2 * MS), 0, "RT picked late at {at}");
                assert_eq!(slice, 500 * US);
            }
        }
    }

    #[test]
    fn fair_slice_never_crosses_an_rt_release() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), rt(2 * MS, 500 * US), 0);
        s.add(ThreadId(2), fair(4), 0);
        // RT first.
        assert_eq!(s.pick_next(0), Some((ThreadId(1), 500 * US)));
        s.sleep(ThreadId(1), 2 * MS);
        s.account(ThreadId(1), 200 * US, 200 * US, RunOutcome::Slept);
        // The fair thread's 4 ms slice is cut at the release at 2 ms.
        assert_eq!(s.pick_next(200 * US), Some((ThreadId(2), 1_800 * US)));
        assert_eq!(s.next_deadline_ns(), Some(2 * MS));
        s.account(ThreadId(2), 1_800 * US, 2 * MS, RunOutcome::Preempted);
        // At the release the RT thread wakes and wins.
        assert_eq!(s.pick_next(2 * MS), Some((ThreadId(1), 500 * US)));
        s.sleep(ThreadId(1), 4 * MS);
        s.account(ThreadId(1), 100 * US, 2_100 * US, RunOutcome::Slept);
        // The fair thread resumes with the rest of its slice (2.2 ms), cut
        // again at the next release (1.9 ms away).
        assert_eq!(s.pick_next(2_100 * US), Some((ThreadId(2), 1_900 * US)));
        s.account(ThreadId(2), 1_900 * US, 4 * MS, RunOutcome::Preempted);
        assert_eq!(s.pick_next(4 * MS), Some((ThreadId(1), 500 * US)));
        s.sleep(ThreadId(1), 6 * MS);
        s.account(ThreadId(1), 100 * US, 4_100 * US, RunOutcome::Slept);
        // 300 us of slice left, then a fresh 4 ms slice cut at 6 ms.
        assert_eq!(s.pick_next(4_100 * US), Some((ThreadId(2), 300 * US)));
        s.account(ThreadId(2), 300 * US, 4_400 * US, RunOutcome::Preempted);
        assert_eq!(s.pick_next(4_400 * US), Some((ThreadId(2), 1_600 * US)));
    }

    // -- (2) EDF ordering ----------------------------------------------------

    #[test]
    fn rt_threads_are_ordered_earliest_deadline_first() {
        let mut sim = Sim::new();
        // Lower id has the later deadline at t=0, so EDF must beat id order.
        let a = sim.add(1, rt(7 * MS, MS), Behavior::Periodic { work_ns: 500 * US });
        let b = sim.add(2, rt(4 * MS, MS), Behavior::Periodic { work_ns: 500 * US });
        sim.add(3, fair(1), Behavior::Busy);
        sim.run_until(30 * MS);

        let rt_picks: Vec<(u64, ThreadId)> =
            sim.picks.iter().filter(|(_, id, _)| *id == a || *id == b).map(|&(at, id, _)| (at, id)).collect();
        // Releases: b at 0,4,8,12,16,20,24,28; a at 0,7,14,21,28.
        // t=0: b (dl 4) before a (dl 7). t=28: a (dl 35) vs b (dl 32): b.
        let expected = vec![
            (0, b),
            (500 * US, a),
            (4 * MS, b),
            (7 * MS, a),
            (8 * MS, b),
            (12 * MS, b),
            (14 * MS, a),
            (16 * MS, b),
            (20 * MS, b),
            (21 * MS, a),
            (24 * MS, b),
            (28 * MS, b),
            (28_500 * US, a),
        ];
        assert_eq!(rt_picks, expected);
        for id in [a, b] {
            let st = sim.sched.rt_stats(id).unwrap();
            assert_eq!(st.missed, 0, "{id:?} {st:?}");
            assert_eq!(st.budget_exhausted, 0, "{id:?} {st:?}");
        }
    }

    #[test]
    fn edf_pick_prefers_earlier_deadline_regardless_of_id() {
        let mut s = Scheduler::new();
        s.add(ThreadId(10), rt(10 * MS, MS), 0);
        s.add(ThreadId(20), rt(3 * MS, MS), 0);
        s.add(ThreadId(5), fair(1), 0);
        let (id, slice) = s.pick_next(0).unwrap();
        assert_eq!(id, ThreadId(20));
        // Budget 1 ms, own release at 3 ms, no earlier-deadline release.
        assert_eq!(slice, MS);
        s.account(ThreadId(20), MS, MS, RunOutcome::Blocked);
        // Now thread 10 (deadline 10 ms). Its slice is cut at 3 ms, when
        // thread 20 is released with deadline 6 ms < 10 ms.
        assert_eq!(s.pick_next(MS), Some((ThreadId(10), MS)));
        s.account(ThreadId(10), 500 * US, 1_500 * US, RunOutcome::Preempted);
        assert_eq!(s.pick_next(1_500 * US), Some((ThreadId(10), 500 * US)));
    }

    #[test]
    fn rt_slice_is_cut_by_earlier_deadline_release_only() {
        let mut s = Scheduler::new();
        // Thread 1: period 2 ms budget 1.5 ms. Thread 2: period 10 ms,
        // released at 10 ms with deadline 20 ms, which is later than
        // thread 1's deadlines: it must never cut thread 1's slice.
        s.add(ThreadId(1), rt(2 * MS, 1_500 * US), 0);
        s.add(ThreadId(2), rt(10 * MS, MS), 0);
        assert_eq!(s.pick_next(0), Some((ThreadId(1), 1_500 * US)));
        s.account(ThreadId(1), 1_500 * US, 1_500 * US, RunOutcome::Preempted);
        // Thread 1 exhausted its budget: demoted. Thread 2 runs; its slice
        // is cut at 2 ms when thread 1 is re-released with deadline 4 ms.
        assert!(s.is_demoted(ThreadId(1)));
        assert_eq!(s.pick_next(1_500 * US), Some((ThreadId(2), 500 * US)));
    }

    // -- (3) budget exhaustion -----------------------------------------------

    #[test]
    fn overrunning_rt_thread_is_demoted_and_fair_threads_progress() {
        let mut sim = Sim::new();
        let hog = sim.add(1, rt(2 * MS, 500 * US), Behavior::Busy);
        let f1 = sim.add(2, fair(1), Behavior::Busy);
        let f2 = sim.add(3, fair(1), Behavior::Busy);
        sim.run_until(100 * MS);

        let stats = sim.sched.rt_stats(hog).unwrap();
        assert_eq!(stats.periods, 50, "{stats:?}");
        assert_eq!(stats.budget_exhausted, 50, "{stats:?}");
        assert_eq!(stats.met, 0);
        assert_eq!(stats.missed, 0);
        // Per period: 500 us as RT, then a 3-way round robin over 1.5 ms.
        // The hog gets about 1 ms per period, each fair thread about 0.5 ms.
        let fair_total = sim.ran(f1) + sim.ran(f2);
        assert!(fair_total >= 40 * MS, "fair total {fair_total}");
        assert!(sim.ran(f1) >= 15 * MS, "f1 {}", sim.ran(f1));
        assert!(sim.ran(f2) >= 15 * MS, "f2 {}", sim.ran(f2));
        assert!(sim.ran(hog) <= 60 * MS, "hog {}", sim.ran(hog));
        // After its budget, every hog pick in a period came from the fair
        // queue: at most 1 ms (weight 1) per pick.
        let mut rt_picks = 0;
        for &(at, id, slice) in &sim.picks {
            if id == hog {
                if at % (2 * MS) == 0 {
                    assert_eq!(slice, 500 * US);
                    rt_picks += 1;
                } else {
                    assert!(slice <= BASE_SLICE_NS, "demoted slice {slice} at {at}");
                }
            }
        }
        assert_eq!(rt_picks, 50);
    }

    #[test]
    fn demotion_ends_at_the_next_release() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), rt(2 * MS, 500 * US), 0);
        s.add(ThreadId(2), fair(1), 0);
        assert_eq!(s.pick_next(0), Some((ThreadId(1), 500 * US)));
        s.account(ThreadId(1), 500 * US, 500 * US, RunOutcome::Preempted);
        assert!(s.is_demoted(ThreadId(1)));
        assert_eq!(s.rt_budget_left_ns(ThreadId(1)), Some(0));
        // Fair thread 2 was queued first; the demoted thread is behind it.
        assert_eq!(s.pick_next(500 * US), Some((ThreadId(2), MS)));
        s.account(ThreadId(2), MS, 1_500 * US, RunOutcome::Preempted);
        assert_eq!(s.pick_next(1_500 * US), Some((ThreadId(1), 500 * US)));
        s.account(ThreadId(1), 500 * US, 2 * MS, RunOutcome::Preempted);
        // Released at 2 ms: back to RT with a full budget, ahead of fair.
        assert!(!s.is_demoted(ThreadId(1)));
        assert_eq!(s.rt_budget_left_ns(ThreadId(1)), Some(500 * US));
        assert_eq!(s.pick_next(2 * MS), Some((ThreadId(1), 500 * US)));
        let st = s.rt_stats(ThreadId(1)).unwrap();
        assert_eq!(st, RtStats { periods: 1, met: 0, missed: 0, budget_exhausted: 1 });
    }

    #[test]
    fn exhausted_then_finished_period_counts_as_met() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), rt(2 * MS, 500 * US), 0);
        assert_eq!(s.pick_next(0), Some((ThreadId(1), 500 * US)));
        s.account(ThreadId(1), 500 * US, 500 * US, RunOutcome::Preempted);
        assert!(s.is_demoted(ThreadId(1)));
        // Runs from the fair queue and finishes before the deadline.
        assert_eq!(s.pick_next(500 * US), Some((ThreadId(1), MS)));
        s.account(ThreadId(1), 100 * US, 600 * US, RunOutcome::Blocked);
        s.tick(2 * MS);
        let st = s.rt_stats(ThreadId(1)).unwrap();
        assert_eq!((st.periods, st.met, st.missed, st.budget_exhausted), (1, 1, 0, 0));
    }

    // -- (4) fair weights ----------------------------------------------------

    #[test]
    fn fair_weights_share_cpu_proportionally() {
        let mut sim = Sim::new();
        let a = sim.add(1, fair(1), Behavior::Busy);
        let b = sim.add(2, fair(2), Behavior::Busy);
        let c = sim.add(3, fair(3), Behavior::Busy);
        sim.run_until(600 * MS);
        let total = sim.ran(a) + sim.ran(b) + sim.ran(c);
        assert_eq!(total, 600 * MS);
        for (id, weight) in [(a, 1u64), (b, 2), (c, 3)] {
            let expected = total * weight / 6;
            let got = sim.ran(id);
            let diff = got.abs_diff(expected);
            assert!(diff * 10 <= expected, "{id:?}: got {got}, expected {expected} (10%)");
        }
    }

    #[test]
    fn fair_weights_proportional_under_rt_load() {
        let mut sim = Sim::new();
        sim.add(9, rt(2 * MS, 500 * US), Behavior::Periodic { work_ns: 400 * US });
        let a = sim.add(1, fair(1), Behavior::Busy);
        let b = sim.add(2, fair(3), Behavior::Busy);
        sim.run_until(1_000 * MS);
        let total = sim.ran(a) + sim.ran(b);
        assert_eq!(total, 800 * MS);
        let ea = total / 4;
        let eb = total * 3 / 4;
        assert!(sim.ran(a).abs_diff(ea) * 10 <= ea, "a {} vs {ea}", sim.ran(a));
        assert!(sim.ran(b).abs_diff(eb) * 10 <= eb, "b {} vs {eb}", sim.ran(b));
    }

    #[test]
    fn weight_zero_is_treated_as_one_and_round_robin_is_fifo() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), fair(0), 0);
        s.add(ThreadId(2), fair(2), 0);
        assert_eq!(s.pick_next(0), Some((ThreadId(1), MS)));
        s.account(ThreadId(1), MS, MS, RunOutcome::Preempted);
        assert_eq!(s.pick_next(MS), Some((ThreadId(2), 2 * MS)));
        s.account(ThreadId(2), 2 * MS, 3 * MS, RunOutcome::Preempted);
        assert_eq!(s.pick_next(3 * MS), Some((ThreadId(1), MS)));
    }

    #[test]
    fn yield_gives_up_the_rest_of_the_slice() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), fair(3), 0);
        s.add(ThreadId(2), fair(1), 0);
        assert_eq!(s.pick_next(0), Some((ThreadId(1), 3 * MS)));
        s.account(ThreadId(1), 100 * US, 100 * US, RunOutcome::Yielded);
        // Thread 2 next, then thread 1 with a fresh slice.
        assert_eq!(s.pick_next(100 * US), Some((ThreadId(2), MS)));
        s.account(ThreadId(2), MS, 1_100 * US, RunOutcome::Preempted);
        assert_eq!(s.pick_next(1_100 * US), Some((ThreadId(1), 3 * MS)));
    }

    #[test]
    fn preempted_thread_keeps_its_place_and_remaining_slice() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), fair(2), 0);
        s.add(ThreadId(2), fair(1), 0);
        assert_eq!(s.pick_next(0), Some((ThreadId(1), 2 * MS)));
        // The kernel preempts early (e.g. a tick) with slice left.
        s.account(ThreadId(1), 500 * US, 500 * US, RunOutcome::Preempted);
        assert_eq!(s.pick_next(500 * US), Some((ThreadId(1), 1_500 * US)));
        s.account(ThreadId(1), 1_500 * US, 2 * MS, RunOutcome::Preempted);
        assert_eq!(s.pick_next(2 * MS), Some((ThreadId(2), MS)));
    }

    // -- (5) sleep / wake ----------------------------------------------------

    #[test]
    fn sleeping_thread_wakes_on_tick_when_due() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), fair(1), 0);
        assert_eq!(s.pick_next(0), Some((ThreadId(1), MS)));
        s.sleep(ThreadId(1), 5 * MS);
        s.account(ThreadId(1), 100 * US, 100 * US, RunOutcome::Slept);
        assert_eq!(s.state(ThreadId(1)), Some(State::Sleeping { until_ns: 5 * MS }));
        assert_eq!(s.next_deadline_ns(), Some(5 * MS));
        // Not due yet.
        s.tick(4_999 * US);
        assert_eq!(s.pick_next(4_999 * US), None);
        assert_eq!(s.state(ThreadId(1)), Some(State::Sleeping { until_ns: 5 * MS }));
        // Due: woken by the tick and picked.
        s.tick(5 * MS);
        assert_eq!(s.state(ThreadId(1)), Some(State::Ready));
        assert_eq!(s.pick_next(5 * MS), Some((ThreadId(1), MS)));
        assert_eq!(s.state(ThreadId(1)), Some(State::Running));
    }

    #[test]
    fn sleep_before_or_after_account_both_work() {
        // sleep() after account(Slept).
        let mut s = Scheduler::new();
        s.add(ThreadId(1), fair(1), 0);
        s.pick_next(0);
        s.account(ThreadId(1), 10 * US, 10 * US, RunOutcome::Slept);
        assert_eq!(s.state(ThreadId(1)), Some(State::Blocked));
        assert_eq!(s.pick_next(10 * US), None);
        s.sleep(ThreadId(1), MS);
        assert_eq!(s.next_deadline_ns(), Some(MS));
        assert_eq!(s.pick_next(MS), Some((ThreadId(1), MS)));
        // A wake() cuts a sleep short.
        s.sleep(ThreadId(1), 10 * MS);
        s.account(ThreadId(1), 10 * US, 1_010 * US, RunOutcome::Slept);
        assert_eq!(s.pick_next(2 * MS), None);
        s.wake(ThreadId(1), 2 * MS);
        assert_eq!(s.pick_next(2 * MS), Some((ThreadId(1), MS)));
    }

    #[test]
    fn sleepers_wake_in_the_simulation_and_rt_sleeper_resumes_as_rt() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), rt(2 * MS, 500 * US), 0);
        s.add(ThreadId(2), fair(1), 0);
        s.pick_next(0);
        s.sleep(ThreadId(1), 3 * MS); // sleeps across its release at 2 ms
        s.account(ThreadId(1), 100 * US, 100 * US, RunOutcome::Slept);
        assert_eq!(s.pick_next(100 * US), Some((ThreadId(2), MS)));
        s.account(ThreadId(2), MS, 1_100 * US, RunOutcome::Preempted);
        assert_eq!(s.pick_next(1_100 * US), Some((ThreadId(2), 900 * US)));
        s.account(ThreadId(2), 900 * US, 2 * MS, RunOutcome::Preempted);
        // Released at 2 ms but still asleep: the fair thread finishes the
        // 100 us left of its slice, then a fresh slice. The slice is not cut
        // at 3 ms (the next RT release is 4 ms); the sleeper's wake is a
        // separate deadline the kernel programs the timer with.
        assert_eq!(s.pick_next(2 * MS), Some((ThreadId(2), 100 * US)));
        s.account(ThreadId(2), 100 * US, 2_100 * US, RunOutcome::Preempted);
        assert_eq!(s.pick_next(2_100 * US), Some((ThreadId(2), MS)));
        assert_eq!(s.next_deadline_ns(), Some(3 * MS));
        s.account(ThreadId(2), 900 * US, 3 * MS, RunOutcome::Preempted);
        // Woken at 3 ms, in period 1 with a full budget; slice cut at its
        // own release (4 ms) and budget 500 us: 500 us.
        s.tick(3 * MS);
        assert!(s.should_preempt(3 * MS) || s.current().is_none());
        assert_eq!(s.pick_next(3 * MS), Some((ThreadId(1), 500 * US)));
        let st = s.rt_stats(ThreadId(1)).unwrap();
        // Period 0: slept (finished) at 100 us: met.
        assert_eq!((st.periods, st.met, st.missed), (1, 1, 0));
    }

    // -- (6) blocked threads are never picked --------------------------------

    #[test]
    fn blocked_threads_are_never_picked() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), fair(1), 0);
        s.add(ThreadId(2), rt(2 * MS, 500 * US), 0);
        s.block(ThreadId(1));
        s.block(ThreadId(2));
        for t in [0, MS, 2 * MS, 5 * MS, 100 * MS] {
            s.tick(t);
            assert_eq!(s.pick_next(t), None, "at {t}");
        }
        // RT releases keep happening while blocked (periods pass, missed).
        let st = s.rt_stats(ThreadId(2)).unwrap();
        assert_eq!(st.periods, 50);
        assert_eq!(st.missed, 50);
        // Waking makes them eligible again; RT wins.
        s.wake(ThreadId(1), 100 * MS);
        s.wake(ThreadId(2), 100 * MS);
        assert_eq!(s.pick_next(100 * MS), Some((ThreadId(2), 500 * US)));
        s.account(ThreadId(2), 500 * US, 100_500 * US, RunOutcome::Blocked);
        assert_eq!(s.state(ThreadId(2)), Some(State::Blocked));
        assert_eq!(s.pick_next(100_500 * US), Some((ThreadId(1), MS)));
        // Blocking a thread that is queued (not running) drops it from the
        // queue when it reaches the front.
        s.account(ThreadId(1), 100 * US, 100_600 * US, RunOutcome::Yielded);
        s.block(ThreadId(1));
        assert_eq!(s.pick_next(100_600 * US), None);
        // Blocking the running thread before account.
        s.wake(ThreadId(1), 100_600 * US);
        assert_eq!(s.pick_next(100_600 * US).map(|p| p.0), Some(ThreadId(1)));
        s.block(ThreadId(1));
        s.account(ThreadId(1), 10 * US, 100_610 * US, RunOutcome::Blocked);
        assert_eq!(s.state(ThreadId(1)), Some(State::Blocked));
        assert_eq!(s.pick_next(100_610 * US), None);
        // wake() on a ready/running thread is a no-op.
        s.wake(ThreadId(1), 100_610 * US);
        assert_eq!(s.pick_next(100_610 * US).map(|p| p.0), Some(ThreadId(1)));
        s.wake(ThreadId(1), 100_610 * US);
        assert_eq!(s.state(ThreadId(1)), Some(State::Running));
    }

    #[test]
    fn blocked_rt_thread_does_not_cut_a_fair_slice_short_of_its_release() {
        // A blocked RT thread still has releases, and the fair slice is cut
        // there (conservative: the kernel re-evaluates and goes on).
        let mut s = Scheduler::new();
        s.add(ThreadId(1), rt(2 * MS, 500 * US), 0);
        s.add(ThreadId(2), fair(5), 0);
        s.block(ThreadId(1));
        assert_eq!(s.pick_next(0), Some((ThreadId(2), 2 * MS)));
        s.account(ThreadId(2), 2 * MS, 2 * MS, RunOutcome::Preempted);
        assert_eq!(s.pick_next(2 * MS), Some((ThreadId(2), 2 * MS)));
        s.account(ThreadId(2), 2 * MS, 4 * MS, RunOutcome::Preempted);
        assert_eq!(s.pick_next(4 * MS), Some((ThreadId(2), MS)));
        s.account(ThreadId(2), MS, 5 * MS, RunOutcome::Preempted);
        // Fresh 5 ms slice, cut at the 6 ms release.
        assert_eq!(s.pick_next(5 * MS), Some((ThreadId(2), MS)));
    }

    // -- (7) remove ----------------------------------------------------------

    #[test]
    fn remove_while_blocked_or_sleeping_or_queued() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), fair(1), 0);
        s.add(ThreadId(2), rt(2 * MS, 500 * US), 0);
        s.add(ThreadId(3), fair(1), 0);
        s.add(ThreadId(4), fair(1), 0);
        assert_eq!(s.len(), 4);
        s.block(ThreadId(1));
        s.sleep(ThreadId(2), MS);
        assert!(s.remove(ThreadId(1)));
        assert!(s.remove(ThreadId(2)));
        assert!(!s.remove(ThreadId(1)));
        assert!(!s.remove(ThreadId(99)));
        assert_eq!(s.len(), 2);
        assert!(!s.contains(ThreadId(1)));
        assert_eq!(s.state(ThreadId(2)), None);
        assert_eq!(s.rt_stats(ThreadId(2)), None);
        // No RT thread, no sleeper: nothing pending but the queue.
        assert_eq!(s.next_deadline_ns(), None);
        // Thread 3 is queued; removing it purges its entry.
        assert!(s.remove(ThreadId(3)));
        assert!(!s.fair_queue.contains(&ThreadId(3)));
        assert_eq!(s.pick_next(MS), Some((ThreadId(4), MS)));
        // The sleeper's wake time no longer resurrects anything.
        s.tick(5 * MS);
        assert!(!s.contains(ThreadId(2)));
        // Wake/block/sleep/account on a removed id are ignored.
        s.wake(ThreadId(2), 5 * MS);
        s.block(ThreadId(1));
        s.sleep(ThreadId(1), 9 * MS);
        s.account(ThreadId(1), 1, 5 * MS, RunOutcome::Preempted);
        assert_eq!(s.len(), 1);
        // Removing the running thread clears `current`.
        assert_eq!(s.current(), Some(ThreadId(4)));
        assert!(s.remove(ThreadId(4)));
        assert_eq!(s.current(), None);
        assert_eq!(s.next_deadline_ns(), None);
        assert_eq!(s.pick_next(6 * MS), None);
        assert!(s.is_empty());
    }

    #[test]
    fn exited_thread_keeps_stats_until_removed() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), rt(2 * MS, 500 * US), 0);
        s.pick_next(0);
        s.account(ThreadId(1), 100 * US, 100 * US, RunOutcome::Yielded);
        s.tick(2 * MS);
        s.pick_next(2 * MS);
        s.account(ThreadId(1), 100 * US, 2_100 * US, RunOutcome::Exited);
        assert_eq!(s.state(ThreadId(1)), Some(State::Exited));
        assert_eq!(s.pick_next(2_100 * US), None);
        assert_eq!(s.rt_stats(ThreadId(1)).map(|st| st.met), Some(1));
        // An exited thread has no deadlines and is not released any more.
        assert_eq!(s.next_deadline_ns(), None);
        s.tick(10 * MS);
        assert_eq!(s.rt_stats(ThreadId(1)).map(|st| st.periods), Some(1));
        s.wake(ThreadId(1), 10 * MS);
        assert_eq!(s.state(ThreadId(1)), Some(State::Exited));
        assert!(s.remove(ThreadId(1)));
        assert_eq!(s.rt_stats(ThreadId(1)), None);
    }

    // -- accounting details --------------------------------------------------

    #[test]
    fn run_straddling_a_period_boundary_is_split() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), rt(2 * MS, 500 * US), 0);
        s.pick_next(0);
        // The kernel overshoots: 300 us before the boundary and 100 us
        // after it, then the thread sleeps. The old period was not finished
        // in time: missed. The new period has 400 us of budget left.
        s.sleep(ThreadId(1), 4 * MS);
        s.account(ThreadId(1), 400 * US, 2_100 * US, RunOutcome::Slept);
        let st = s.rt_stats(ThreadId(1)).unwrap();
        assert_eq!((st.periods, st.met, st.missed, st.budget_exhausted), (1, 0, 1, 0));
        assert_eq!(s.rt_budget_left_ns(ThreadId(1)), Some(400 * US));
        assert_eq!(s.rt_deadline_ns(ThreadId(1)), Some(4 * MS));
    }

    #[test]
    fn clock_jump_counts_skipped_periods_as_missed() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), rt(2 * MS, 500 * US), MS);
        s.block(ThreadId(1));
        s.tick(11 * MS); // periods [1,3) [3,5) [5,7) [7,9) [9,11) all over
        let st = s.rt_stats(ThreadId(1)).unwrap();
        assert_eq!((st.periods, st.missed), (5, 5));
        assert_eq!(s.rt_deadline_ns(ThreadId(1)), Some(13 * MS));
        s.tick(12 * MS);
        assert_eq!(s.rt_stats(ThreadId(1)).unwrap().periods, 5);
    }

    #[test]
    fn rt_release_without_fair_queue_entry_is_picked_as_rt_again() {
        // A demoted thread sits in the fair queue; after its release the
        // stale entry must not make it run as fair.
        let mut s = Scheduler::new();
        s.add(ThreadId(1), rt(2 * MS, 500 * US), 0);
        s.add(ThreadId(2), fair(1), 0);
        s.pick_next(0);
        s.account(ThreadId(1), 500 * US, 500 * US, RunOutcome::Preempted);
        assert!(s.is_demoted(ThreadId(1)));
        s.tick(2 * MS);
        assert!(!s.is_demoted(ThreadId(1)));
        assert_eq!(s.pick_next(2 * MS), Some((ThreadId(1), 500 * US)));
        s.account(ThreadId(1), 100 * US, 2_100 * US, RunOutcome::Blocked);
        assert_eq!(s.pick_next(2_100 * US), Some((ThreadId(2), MS)));
        s.account(ThreadId(2), MS, 3_100 * US, RunOutcome::Preempted);
        // Only thread 2 is runnable; the stale entry for 1 is skipped.
        assert_eq!(s.pick_next(3_100 * US), Some((ThreadId(2), 900 * US)));
    }

    #[test]
    fn unaccounted_pick_is_treated_as_preempted() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), fair(1), 0);
        s.add(ThreadId(2), fair(1), 0);
        assert_eq!(s.pick_next(0), Some((ThreadId(1), MS)));
        // No account: the thread goes back to the front with its slice.
        assert_eq!(s.pick_next(0), Some((ThreadId(1), MS)));
        s.account(ThreadId(1), MS, MS, RunOutcome::Preempted);
        assert_eq!(s.pick_next(MS), Some((ThreadId(2), MS)));
    }

    #[test]
    fn should_preempt_reports_slice_end_and_rt_arrival() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), fair(1), 0);
        s.add(ThreadId(2), rt(2 * MS, 500 * US), 0);
        assert!(!s.should_preempt(0));
        s.pick_next(0); // RT
        assert!(!s.should_preempt(100 * US));
        s.block(ThreadId(2));
        assert!(s.should_preempt(100 * US)); // blocked while running
        s.account(ThreadId(2), 100 * US, 100 * US, RunOutcome::Blocked);
        assert!(!s.should_preempt(100 * US));
        assert_eq!(s.pick_next(100 * US), Some((ThreadId(1), MS)));
        assert!(!s.should_preempt(500 * US));
        // An RT wake-up preempts the fair thread immediately.
        s.wake(ThreadId(2), 500 * US);
        assert!(s.should_preempt(500 * US));
        s.account(ThreadId(1), 400 * US, 500 * US, RunOutcome::Preempted);
        // 400 us of budget left in this period.
        assert_eq!(s.pick_next(500 * US), Some((ThreadId(2), 400 * US)));
        // Slice end.
        assert!(!s.should_preempt(899 * US));
        assert!(s.should_preempt(900 * US));
        s.account(ThreadId(2), 400 * US, 900 * US, RunOutcome::Preempted);
        // Later-deadline RT does not preempt an earlier-deadline one.
        s.add(ThreadId(3), rt(10 * MS, MS), MS);
        s.tick(2 * MS);
        assert_eq!(s.pick_next(2 * MS).map(|p| p.0), Some(ThreadId(2)));
        assert!(!s.should_preempt(2_100 * US));
    }

    #[test]
    fn next_deadline_is_earliest_of_release_slice_end_and_wake() {
        let mut s = Scheduler::new();
        assert_eq!(s.next_deadline_ns(), None);
        s.add(ThreadId(1), fair(3), 0);
        assert_eq!(s.next_deadline_ns(), None);
        assert_eq!(s.pick_next(0), Some((ThreadId(1), 3 * MS)));
        assert_eq!(s.next_deadline_ns(), Some(3 * MS));
        s.add(ThreadId(2), rt(2 * MS, 500 * US), 0);
        assert_eq!(s.next_deadline_ns(), Some(2 * MS));
        s.add(ThreadId(3), fair(1), 0);
        s.block(ThreadId(3));
        s.sleep(ThreadId(3), MS);
        assert_eq!(s.next_deadline_ns(), Some(MS));
        s.account(ThreadId(1), 500 * US, 500 * US, RunOutcome::Blocked);
        assert_eq!(s.next_deadline_ns(), Some(MS));
        s.tick(MS);
        assert_eq!(s.next_deadline_ns(), Some(2 * MS));
    }

    #[test]
    fn reused_thread_id_has_one_queue_entry() {
        // The kernel reuses the slot, and so the `ThreadId`, of an exited
        // instance. A stale fair-queue entry of the old thread must not give
        // the new one a second turn in the round robin.
        let mut s = Scheduler::new();
        s.add(ThreadId(1), fair(1), 0);
        s.add(ThreadId(2), fair(1), 0);
        // 1 runs its whole slice and is preempted: queued behind 2.
        assert_eq!(s.pick_next(0).map(|p| p.0), Some(ThreadId(1)));
        s.account(ThreadId(1), MS, MS, RunOutcome::Preempted);
        assert!(s.fair_queue.contains(&ThreadId(1)));
        // 1 exits while queued and its id is reused by a new thread.
        assert!(s.remove(ThreadId(1)));
        assert!(!s.fair_queue.contains(&ThreadId(1)));
        s.add(ThreadId(1), fair(1), MS);
        assert_eq!(s.fair_queue.iter().filter(|&&t| t == ThreadId(1)).count(), 1);
        // Round robin: 2, the new 1, 2 again, 1 again.
        for (i, expect) in [2u32, 1, 2, 1].into_iter().enumerate() {
            let now = (i as u64 + 1) * MS;
            assert_eq!(s.pick_next(now).map(|p| p.0), Some(ThreadId(expect)));
            s.account(ThreadId(expect), MS, now + MS, RunOutcome::Preempted);
        }
    }

    #[test]
    fn add_clamps_invalid_rt_class_and_replaces_duplicates() {
        assert!(rt(2 * MS, 500 * US).is_valid());
        assert!(!rt(0, 1).is_valid());
        assert!(!rt(5, 0).is_valid());
        assert!(!rt(5, 6).is_valid());
        assert!(fair(0).is_valid());
        assert!(rt(1, 1).is_real_time());
        assert!(!fair(1).is_real_time());

        let mut s = Scheduler::new();
        s.add(ThreadId(1), rt(0, 0), 0);
        assert_eq!(s.class(ThreadId(1)), Some(Class::RealTime { period_ns: 1, budget_ns: 1 }));
        s.add(ThreadId(1), rt(10, 50), 0);
        assert_eq!(s.class(ThreadId(1)), Some(Class::RealTime { period_ns: 10, budget_ns: 10 }));
        assert_eq!(s.len(), 1);
        s.add(ThreadId(1), fair(2), 0);
        assert_eq!(s.class(ThreadId(1)), Some(fair(2)));
        assert_eq!(s.rt_stats(ThreadId(1)), None);
        assert_eq!(s.thread_ids(), vec![ThreadId(1)]);
        // Huge values do not overflow.
        s.add(ThreadId(2), rt(u64::MAX, u64::MAX), u64::MAX - 5);
        s.add(ThreadId(3), fair(u32::MAX), u64::MAX);
        s.tick(u64::MAX);
        assert!(s.pick_next(u64::MAX).is_some());
        s.account(ThreadId(2), u64::MAX, u64::MAX, RunOutcome::Preempted);
        assert!(s.pick_next(u64::MAX).is_some());
    }

    #[test]
    fn rt_stats_classification() {
        let mut s = Scheduler::new();
        s.add(ThreadId(1), rt(2 * MS, 500 * US), 0);
        // Period 0: yield before the deadline: met.
        s.pick_next(0);
        s.account(ThreadId(1), 100 * US, 100 * US, RunOutcome::Yielded);
        // Period 1: run 200 us, preempted, never finishes: missed.
        s.tick(2 * MS);
        s.pick_next(2 * MS);
        s.account(ThreadId(1), 200 * US, 2_200 * US, RunOutcome::Preempted);
        // Period 2: exhaust: budget_exhausted.
        s.tick(4 * MS);
        s.pick_next(4 * MS);
        s.account(ThreadId(1), 500 * US, 4_500 * US, RunOutcome::Preempted);
        // Period 3: block before the deadline: met.
        s.tick(6 * MS);
        s.pick_next(6 * MS);
        s.account(ThreadId(1), 50 * US, 6_050 * US, RunOutcome::Blocked);
        s.tick(8 * MS);
        let st = s.rt_stats(ThreadId(1)).unwrap();
        assert_eq!(st, RtStats { periods: 4, met: 2, missed: 1, budget_exhausted: 1 });
        assert_eq!(st.met_ppm(), 500_000);
        assert_eq!(RtStats::default().met_ppm(), 1_000_000);
    }

    // -- fuel ------------------------------------------------------------------

    #[test]
    fn fuel_conversion() {
        // 100 fuel per microsecond.
        let rate = FuelRate::new(100_000, MS);
        assert_eq!(rate.fuel(), 100_000);
        assert_eq!(rate.ns(), MS);
        assert_eq!(fuel_for_slice(MS, rate), 100_000);
        assert_eq!(fuel_for_slice(500 * US, rate), 50_000);
        assert_eq!(fuel_for_slice(1, rate), 1, "never zero fuel");
        assert_eq!(fuel_for_slice(0, rate), 1);
        assert_eq!(rate.ns_for_fuel(100_000), MS);
        assert_eq!(rate.ns_for_fuel(1), 10);
        assert_eq!(rate.ns_for_fuel(0), 0);
        // Fractional fuel per ns rounds down to the whole unit.
        let slow = FuelRate::new(3, 10);
        assert_eq!(slow.fuel_for_ns(10), 3);
        assert_eq!(slow.fuel_for_ns(13), 3);
        assert_eq!(slow.ns_for_fuel(1), 4);
        // Zero calibration values are made usable.
        let z = FuelRate::new(0, 0);
        assert_eq!(z.fuel_for_ns(7), 7);
        // Saturates instead of overflowing.
        let fast = FuelRate::new(u64::MAX, 1);
        assert_eq!(fast.fuel_for_ns(u64::MAX), u64::MAX);
        assert_eq!(FuelRate::new(1, u64::MAX).ns_for_fuel(u64::MAX), u64::MAX);
    }
}
