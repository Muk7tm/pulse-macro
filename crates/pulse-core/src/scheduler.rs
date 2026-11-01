//! Deterministic, clock-free clicker executor.
//!
//! A pure state machine: the caller supplies `now` (a `CLOCK_MONOTONIC`
//! reading in production, a fake clock in tests) and gets back at most one
//! event to submit plus the next absolute deadline.
//!
//! Invariants:
//! - Presses are aligned to the absolute grid `t0 + n*period`; late wakeups
//!   never accumulate drift.
//! - A press later than its hold time is skipped (never burst-submitted to
//!   catch up); an instant click (hold 0) may be up to half a period late.
//!   After a late release the next press waits for a grid slot that leaves
//!   at least `(period - hold) / 2` of up-time.
//! - A press that is late but acceptable keeps at least half its hold time.
//! - Press and release strictly alternate; releases are never skipped.
//! - Every finish path (count, duration, cancel) releases the input first.

use crate::clicker::Program;
use crate::input::LogicalInput;
use crate::time::Nanos;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishReason {
    /// The click-count limit was reached.
    Completed,
    /// The duration limit was reached.
    DurationElapsed,
    /// External cancellation (stop, emergency stop, focus loss, faults...).
    Cancelled,
}

/// What the caller should do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Call `poll` again at (or after) this absolute deadline.
    At(Nanos),
    /// Run finished; the input is released.
    Done(FinishReason),
}

/// Result of one executor step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    /// Event to submit now, if any.
    pub emit: Option<LogicalInput>,
    pub next: Next,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunStats {
    /// Presses submitted.
    pub clicks: u64,
    /// All events handed to the backend.
    pub events: u64,
    /// Grid slots skipped because the wakeup was too late.
    pub missed_deadlines: u64,
    /// Largest observed lateness at a timed wakeup.
    pub max_lateness: Nanos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Press { slot: u64 },
    Release { slot: u64 },
    Done(FinishReason),
}

pub struct Executor {
    program: Program,
    t0: Nanos,
    deadline: Nanos,
    phase: Phase,
    held: bool,
    stats: RunStats,
}

impl Executor {
    /// Start a run at `now`; the first press is due immediately.
    pub fn new(program: Program, now: Nanos) -> Executor {
        Executor {
            program,
            t0: now,
            deadline: now,
            phase: Phase::Press { slot: 0 },
            held: false,
            stats: RunStats::default(),
        }
    }

    pub fn program(&self) -> &Program {
        &self.program
    }

    pub fn stats(&self) -> &RunStats {
        &self.stats
    }

    pub fn start_time(&self) -> Nanos {
        self.t0
    }

    /// The deadline the caller should currently be waiting for.
    pub fn deadline(&self) -> Nanos {
        self.deadline
    }

    /// Whether the target input is currently held down.
    pub fn is_held(&self) -> bool {
        self.held
    }

    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done(_))
    }

    /// How late a press may still be submitted. A held click is skipped once
    /// its release would already be overdue; an instant click (hold 0) once
    /// it would crowd the next grid slot.
    fn press_tolerance(&self) -> Nanos {
        match self.program.hold {
            0 => self.program.period / 2,
            hold => hold,
        }
    }

    fn past_duration(&self, at: Nanos) -> bool {
        self.program
            .max_duration
            .is_some_and(|d| at >= self.t0.saturating_add(d))
    }

    fn press(&mut self) -> LogicalInput {
        self.held = true;
        self.stats.clicks += 1;
        self.stats.events += 1;
        self.program.target.press()
    }

    fn release(&mut self) -> LogicalInput {
        self.held = false;
        self.stats.events += 1;
        self.program.target.release()
    }

    /// End the run, releasing the input if it is held.
    fn finish(&mut self, reason: FinishReason) -> Step {
        let emit = self.held.then(|| self.release());
        self.phase = Phase::Done(reason);
        Step {
            emit,
            next: Next::Done(reason),
        }
    }

    /// Cancel the run. Idempotent; releases the input if held.
    pub fn cancel(&mut self) -> Step {
        match self.phase {
            Phase::Done(r) => Step {
                emit: None,
                next: Next::Done(r),
            },
            _ => self.finish(FinishReason::Cancelled),
        }
    }

    /// Perform whatever is due at `now`.
    pub fn poll(&mut self, now: Nanos) -> Step {
        let wait = |d| Step {
            emit: None,
            next: Next::At(d),
        };
        match self.phase {
            Phase::Done(r) => Step {
                emit: None,
                next: Next::Done(r),
            },
            Phase::Press { .. } | Phase::Release { .. } if now < self.deadline => wait(self.deadline),
            Phase::Press { slot } => self.on_press(now, slot),
            Phase::Release { slot } => self.on_release(now, slot),
        }
    }

    fn on_press(&mut self, now: Nanos, slot: u64) -> Step {
        let (period, hold) = (self.program.period, self.program.hold);
        let d = self.deadline;
        if self.past_duration(d) {
            return self.finish(FinishReason::DurationElapsed);
        }
        let late = now - d;
        self.stats.max_lateness = self.stats.max_lateness.max(late);
        if late >= self.press_tolerance() {
            // Too late: skip to the next grid slot strictly after `now`;
            // never burst.
            let next = (now - self.t0) / period + 1;
            self.stats.missed_deadlines += next - slot;
            self.deadline = self.t0 + next * period;
            self.phase = Phase::Press { slot: next };
            if self.past_duration(self.deadline) {
                return self.finish(FinishReason::DurationElapsed);
            }
            return Step {
                emit: None,
                next: Next::At(self.deadline),
            };
        }
        let ev = self.press();
        // Keep the grid hold, but never shorten the hold below half.
        self.deadline = (d + hold).max(now + hold / 2);
        self.phase = Phase::Release { slot };
        Step {
            emit: Some(ev),
            next: Next::At(self.deadline),
        }
    }

    fn on_release(&mut self, now: Nanos, slot: u64) -> Step {
        let (period, hold) = (self.program.period, self.program.hold);
        self.stats.max_lateness = self.stats.max_lateness.max(now - self.deadline);
        let ev = self.release();
        if self.program.count.is_some_and(|n| self.stats.clicks >= n) {
            self.phase = Phase::Done(FinishReason::Completed);
            return Step {
                emit: Some(ev),
                next: Next::Done(FinishReason::Completed),
            };
        }
        // Next press: the first grid slot leaving a minimum up-time.
        let earliest = now + (period - hold) / 2;
        let next = (earliest - self.t0).div_ceil(period).max(slot + 1);
        self.stats.missed_deadlines += next - (slot + 1);
        self.deadline = self.t0 + next * period;
        if self.past_duration(self.deadline) {
            self.phase = Phase::Done(FinishReason::DurationElapsed);
            return Step {
                emit: Some(ev),
                next: Next::Done(FinishReason::DurationElapsed),
            };
        }
        self.phase = Phase::Press { slot: next };
        Step {
            emit: Some(ev),
            next: Next::At(self.deadline),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clicker::{ClickerSpec, compile};
    use crate::input::{Key, KeyState, MouseButton, Pressable};
    use crate::time::MS;
    use crate::validation::Limits;

    const LEFT: Pressable = Pressable::Button(MouseButton::Left);

    fn program(interval: u64, hold: u64, count: u64, duration: u64) -> Program {
        compile(
            &ClickerSpec {
                target: LEFT,
                interval_ms: interval,
                hold_ms: hold,
                count,
                duration_ms: duration,
            },
            &Limits::STANDARD,
        )
        .expect("valid")
        .program
    }

    /// Drive with a perfect clock until done or `until`.
    fn run_ideal(ex: &mut Executor, until: Nanos) -> Vec<(Nanos, LogicalInput)> {
        let mut log = Vec::new();
        let mut now = ex.start_time();
        loop {
            let step = ex.poll(now);
            log.extend(step.emit.map(|e| (now, e)));
            match step.next {
                Next::At(t) if t <= until => now = now.max(t),
                _ => break,
            }
        }
        log
    }

    #[test]
    fn follows_absolute_grid() {
        let mut ex = Executor::new(program(100, 20, 0, 0), 1_000 * MS);
        let log = run_ideal(&mut ex, 1_450 * MS);
        let times: Vec<u64> = log.iter().map(|(t, _)| (t - 1_000 * MS) / MS).collect();
        assert_eq!(times, vec![0, 20, 100, 120, 200, 220, 300, 320, 400, 420]);
        assert_eq!(ex.stats().clicks, 5);
        assert_eq!(ex.stats().missed_deadlines, 0);
    }

    #[test]
    fn count_completes_and_releases() {
        let mut ex = Executor::new(program(50, 10, 3, 0), 0);
        let log = run_ideal(&mut ex, 10_000 * MS);
        assert_eq!(log.len(), 6);
        assert!(ex.is_done());
        assert!(!ex.is_held());
        assert_eq!(ex.poll(20_000 * MS).next, Next::Done(FinishReason::Completed));
    }

    #[test]
    fn duration_limit() {
        let mut ex = Executor::new(program(100, 20, 0, 350), 0);
        let _ = run_ideal(&mut ex, 10_000 * MS);
        assert_eq!(ex.stats().clicks, 4); // 0, 100, 200, 300
        assert_eq!(ex.poll(10_000 * MS).next, Next::Done(FinishReason::DurationElapsed));
        assert!(!ex.is_held());
    }

    #[test]
    fn late_press_is_skipped_not_burst() {
        let mut ex = Executor::new(program(100, 20, 0, 0), 0);
        assert_eq!(ex.poll(0).next, Next::At(20 * MS));
        assert_eq!(ex.poll(20 * MS).next, Next::At(100 * MS));
        // A 350 ms stall: slots 100, 200, 300 and 400 are missed.
        let step = ex.poll(450 * MS);
        assert_eq!(step.emit, None, "no press when its release would be overdue");
        assert_eq!(step.next, Next::At(500 * MS));
        assert_eq!(ex.stats().missed_deadlines, 4);
        let step = ex.poll(500 * MS);
        assert!(step.emit.is_some_and(|e| e.is_press()));
        assert_eq!(step.next, Next::At(520 * MS));
    }

    #[test]
    fn late_release_keeps_order_and_min_gap() {
        let mut ex = Executor::new(program(100, 20, 0, 0), 0);
        ex.poll(0);
        // Release wakes 95 ms late: the next press must leave >= 40 ms of gap.
        let step = ex.poll(115 * MS);
        assert_eq!(step.emit, Some(LEFT.release()));
        assert_eq!(step.next, Next::At(200 * MS));
        assert_eq!(ex.stats().missed_deadlines, 1);
    }

    #[test]
    fn slightly_late_press_keeps_half_its_hold() {
        let mut ex = Executor::new(program(100, 20, 0, 0), 0);
        let step = ex.poll(15 * MS);
        assert!(step.emit.is_some_and(|e| e.is_press()));
        assert_eq!(step.next, Next::At(25 * MS));
    }

    #[test]
    fn instant_clicks_reach_1000_per_second() {
        let fastest = |interval| {
            let spec = ClickerSpec {
                target: LEFT,
                interval_ms: interval,
                hold_ms: 0,
                count: 0,
                duration_ms: 0,
            };
            compile(&spec, &Limits::ADVANCED).expect("valid").program
        };
        let mut ex = Executor::new(fastest(1), 0);
        let log = run_ideal(&mut ex, 4_500_000);
        let times: Vec<u64> = log.iter().map(|(t, _)| t / 1_000).collect();
        // Press and release at the same instant, every millisecond.
        assert_eq!(
            times,
            vec![0, 0, 1_000, 1_000, 2_000, 2_000, 3_000, 3_000, 4_000, 4_000]
        );
        assert!(log.iter().step_by(2).all(|(_, e)| e.is_press()));
        assert_eq!((ex.stats().clicks, ex.stats().missed_deadlines), (5, 0));

        // A late instant click is still sent within half a period...
        let mut ex = Executor::new(fastest(10), 0);
        ex.poll(0);
        ex.poll(0);
        assert!(ex.poll(14 * MS).emit.is_some_and(|e| e.is_press()));
        assert_eq!(ex.poll(14 * MS).next, Next::At(20 * MS));
        // ...and skipped after that, without bursting.
        let step = ex.poll(26 * MS);
        assert_eq!(step.emit, None);
        assert_eq!(step.next, Next::At(30 * MS));
        assert_eq!(ex.stats().missed_deadlines, 1);
    }

    #[test]
    fn cancel_releases_and_is_idempotent() {
        let mut ex = Executor::new(program(100, 20, 0, 0), 0);
        ex.poll(0);
        assert!(ex.is_held());
        let step = ex.cancel();
        assert_eq!(step.emit, Some(LEFT.release()));
        assert_eq!(step.next, Next::Done(FinishReason::Cancelled));
        assert!(!ex.is_held());
        assert_eq!(ex.cancel().emit, None);
        assert_eq!(ex.poll(7 * MS).emit, None);
        // Cancelling while released emits nothing.
        let mut ex = Executor::new(program(100, 20, 0, 0), 0);
        ex.poll(0);
        ex.poll(20 * MS);
        assert_eq!(ex.cancel().emit, None);
    }

    #[test]
    fn spurious_early_wake_does_nothing() {
        let mut ex = Executor::new(program(100, 20, 0, 0), 0);
        ex.poll(0);
        let step = ex.poll(10 * MS);
        assert_eq!(step.emit, None);
        assert_eq!(step.next, Next::At(20 * MS));
    }

    #[test]
    fn keyboard_target_presses_the_key() {
        let e = Pressable::Key(Key::parse("E").expect("key"));
        let mut p = program(100, 20, 2, 0);
        p.target = e;
        let mut ex = Executor::new(p, 0);
        let log: Vec<LogicalInput> = run_ideal(&mut ex, 1_000 * MS).into_iter().map(|(_, e)| e).collect();
        let key = Key::parse("E").expect("key");
        assert_eq!(
            log,
            vec![
                LogicalInput::key(key, KeyState::Pressed),
                LogicalInput::key(key, KeyState::Released),
                LogicalInput::key(key, KeyState::Pressed),
                LogicalInput::key(key, KeyState::Released),
            ]
        );
    }
}
