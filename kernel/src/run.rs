//! The run loop: picks threads, runs their Wasm executions for a fuel
//! budget derived from the scheduler's slice, maps the result to a
//! [`RunOutcome`], and programs the timer for the next deadline.
//!
//! # Fuel and time
//!
//! The interpreter runs for a number of instructions (fuel), the scheduler
//! thinks in nanoseconds. A [`FuelMeter`] converts between them from
//! measurement: every chunk that ends in `OutOfFuel` burned exactly its fuel
//! in a known time, and the meter keeps a decaying window of those samples.
//! It starts from a deliberately pessimistic guess, so the first chunks are
//! short and the estimate converges within a few of them.
//!
//! A slice is run in chunks of about [`CHUNK_NS`] so that preemption is
//! prompt: between chunks the loop checks the timer's preempt flag and asks
//! the scheduler whether a woken thread should run instead.
//!
//! # Timer
//!
//! The generic timer is one-shot. Before every chunk sequence it is armed
//! for the earliest of the scheduler's next deadline (the end of the running
//! slice, an RT release or a sleeper's wake time) and one millisecond from
//! now. The interrupt handler only sets [`PREEMPT`]; all scheduling happens
//! in the loop.
//!
//! # Termination
//!
//! When `init` exits the kernel prints `kalium: init exited <code>` and
//! leaves QEMU with code 0 if `init` exited 0 and every other instance has
//! exited, else 1. A deadlock (no runnable thread and nothing the timer
//! could wake) exits with 1.

use core::sync::atomic::{AtomicBool, Ordering};

use kalium_hal::{println, timer};
use kalium_sched::{FuelRate, RunOutcome, ThreadId};
use kalium_wasm::{ExecResult, Execution};

use crate::host::{Effect, HostCall};
use crate::kernel::{Kernel, Phase, ENTRY_EXPORT};

/// Set from the timer interrupt; cleared by the loop.
static PREEMPT: AtomicBool = AtomicBool::new(false);

/// Target length of one interpreter chunk.
const CHUNK_NS: u64 = 100_000;
/// The timer is never armed later than this.
const MAX_TIMER_NS: u64 = 1_000_000;
/// Fuel bounds for a chunk, whatever the meter says.
const MIN_CHUNK_FUEL: u64 = 100;
const MAX_CHUNK_FUEL: u64 = 1_000_000;

fn on_tick() {
    PREEMPT.store(true, Ordering::Release);
}

/// Measured interpreter speed. See the module docs.
pub struct FuelMeter {
    window_fuel: u64,
    window_ns: u64,
    pub total_fuel: u64,
    pub total_ns: u64,
}

impl FuelMeter {
    /// A pessimistic guess for the first chunks: 1 M instructions/s.
    const INITIAL: FuelRate = FuelRate::new(1_000, 1_000_000);
    /// The window is halved past this much measured time.
    const WINDOW_NS: u64 = 200_000_000;
    /// Below this much measured time the guess is used.
    const MIN_SAMPLE_NS: u64 = 500_000;

    pub const fn new() -> FuelMeter {
        FuelMeter {
            window_fuel: 0,
            window_ns: 0,
            total_fuel: 0,
            total_ns: 0,
        }
    }

    pub fn observe(&mut self, fuel: u64, ns: u64) {
        self.window_fuel = self.window_fuel.saturating_add(fuel);
        self.window_ns = self.window_ns.saturating_add(ns);
        self.total_fuel = self.total_fuel.saturating_add(fuel);
        self.total_ns = self.total_ns.saturating_add(ns);
        if self.window_ns > Self::WINDOW_NS {
            self.window_fuel /= 2;
            self.window_ns /= 2;
        }
    }

    pub fn rate(&self) -> FuelRate {
        if self.window_ns < Self::MIN_SAMPLE_NS {
            Self::INITIAL
        } else {
            FuelRate::new(self.window_fuel, self.window_ns)
        }
    }

    /// Instructions per second over everything measured.
    pub fn instructions_per_second(&self) -> u64 {
        if self.total_ns == 0 {
            0
        } else {
            ((self.total_fuel as u128 * 1_000_000_000) / self.total_ns as u128) as u64
        }
    }
}

/// Arms the timer for `deadline`, never later than `MAX_TIMER_NS` from now.
fn arm_timer(deadline: u64, now: u64) {
    let latest = now.saturating_add(MAX_TIMER_NS);
    timer::set_timer_at(deadline.min(latest));
}

/// Runs the kernel until `init` exits. Never returns.
pub fn run(k: &mut Kernel) -> ! {
    let mut meter = FuelMeter::new();
    timer::set_tick_handler(on_tick);
    kalium_hal::enable_irqs();
    loop {
        let now = timer::now_ns();
        k.sched.tick(now);
        if let Some(code) = k.instances.first().and_then(|i| i.exited) {
            shutdown(k, &meter, code);
        }
        match k.sched.pick_next(now) {
            Some((tid, slice_ns)) => run_slice(k, &mut meter, tid, slice_ns, now),
            None => idle(k, now),
        }
    }
}

/// Nothing is runnable: wait for the next timer deadline.
fn idle(k: &mut Kernel, now: u64) {
    match k.sched.next_deadline_ns() {
        Some(deadline) => {
            if deadline <= now {
                return;
            }
            arm_timer(deadline, now);
            kalium_hal::disable_irqs();
            if !PREEMPT.load(Ordering::Acquire) {
                // `wfi` wakes on a pending interrupt even while masked, so
                // the interrupt cannot be lost between the check and here.
                kalium_hal::wait_for_interrupt();
            }
            kalium_hal::enable_irqs();
            PREEMPT.store(false, Ordering::Release);
        }
        None => {
            println!(
                "kalium: deadlock: {} instance(s) blocked and nothing to wake them",
                k.live_instances()
            );
            for (i, inst) in k.instances.iter().enumerate() {
                if inst.exited.is_none() {
                    println!("kalium: instance {} ({}) is blocked", i, inst.name);
                }
            }
            kalium_hal::exit(1);
        }
    }
}

/// Runs `tid` for `slice_ns` (or until it blocks, yields, exits or is
/// preempted) and reports back to the scheduler.
fn run_slice(k: &mut Kernel, meter: &mut FuelMeter, tid: ThreadId, slice_ns: u64, start: u64) {
    let idx = tid.0 as usize;
    let Some(mut run) = k.instances.get_mut(idx).and_then(|i| i.run.take()) else {
        // Exited while queued (killed): nothing to run.
        k.sched.account(tid, 0, start, RunOutcome::Exited);
        k.sched.remove(tid);
        return;
    };
    let rate = meter.rate();
    let slice_fuel = rate.fuel_for_ns(slice_ns).max(MIN_CHUNK_FUEL);
    let chunk_fuel = rate
        .fuel_for_ns(CHUNK_NS)
        .clamp(MIN_CHUNK_FUEL, MAX_CHUNK_FUEL);

    PREEMPT.store(false, Ordering::Release);
    k.reschedule = false;
    let slice_end = start.saturating_add(slice_ns);
    let deadline = k
        .sched
        .next_deadline_ns()
        .map_or(slice_end, |d| d.min(slice_end));
    arm_timer(deadline, start);

    let mut used: u64 = 0;
    let mut exit: Option<i32> = None;
    let outcome;
    loop {
        let chunk = chunk_fuel.min(slice_fuel.saturating_sub(used)).max(1);
        let t0 = timer::now_ns();
        let mut host = HostCall::new(k, idx);
        let result = run.exec.run(&mut run.wasm, &mut host, chunk);
        let effect = host.effect;
        let t1 = timer::now_ns();
        match result {
            ExecResult::OutOfFuel => {
                meter.observe(chunk, t1.saturating_sub(t0));
                used = used.saturating_add(chunk);
                let preempt = PREEMPT.load(Ordering::Acquire);
                let reschedule = k.reschedule && k.sched.should_preempt(t1);
                if used >= slice_fuel || preempt || reschedule || t1 >= slice_end {
                    outcome = RunOutcome::Preempted;
                    break;
                }
            }
            ExecResult::Blocked => {
                outcome = match effect {
                    Effect::Yielded => RunOutcome::Yielded,
                    Effect::Slept => RunOutcome::Slept,
                    Effect::Blocked | Effect::None => RunOutcome::Blocked,
                };
                break;
            }
            ExecResult::Done(_) => {
                if k.instances[idx].phase == Phase::Start {
                    // The start function returned; now `_start`.
                    let entry = run.wasm.export_func(ENTRY_EXPORT);
                    match entry.and_then(|f| Execution::new(&run.wasm, f, &[]).ok()) {
                        Some(e) => {
                            run.exec = e;
                            k.instances[idx].phase = Phase::Main;
                            continue;
                        }
                        None => {
                            exit = Some(kalium_abi::exit::TRAP);
                            outcome = RunOutcome::Exited;
                            break;
                        }
                    }
                }
                exit = Some(kalium_abi::exit::SUCCESS);
                outcome = RunOutcome::Exited;
                break;
            }
            ExecResult::Exited(code) => {
                exit = Some(code);
                outcome = RunOutcome::Exited;
                break;
            }
            ExecResult::Trap(trap) => {
                println!(
                    "kalium: instance {} ({}) trapped: {}",
                    idx, k.instances[idx].name, trap
                );
                exit = Some(kalium_abi::exit::TRAP);
                outcome = RunOutcome::Exited;
                break;
            }
        }
    }
    let end = timer::now_ns();
    k.sched
        .account(tid, end.saturating_sub(start), end, outcome);
    match exit {
        Some(code) => {
            drop(run);
            k.finish_instance(idx, code, end);
            k.sched.remove(tid);
        }
        None => k.instances[idx].run = Some(run),
    }
}

/// `init` exited: report and leave QEMU.
fn shutdown(k: &Kernel, meter: &FuelMeter, code: i32) -> ! {
    kalium_hal::disable_irqs();
    timer::stop_timer();
    let mut running = 0;
    for (i, inst) in k.instances.iter().enumerate().skip(1) {
        if inst.exited.is_none() {
            println!("kalium: instance {} ({}) still running", i, inst.name);
            running += 1;
        }
    }
    println!(
        "kalium: interpreter ran {} instructions in {} ms ({} instr/s)",
        meter.total_fuel,
        meter.total_ns / 1_000_000,
        meter.instructions_per_second()
    );
    println!("kalium: init exited {}", code);
    kalium_hal::exit(if code == 0 && running == 0 { 0 } else { 1 })
}
