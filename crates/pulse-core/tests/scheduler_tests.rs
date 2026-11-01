//! Property tests: thousands of simulated clicker runs with random timing
//! faults, for mouse and keyboard targets.
//!
//! Invariants checked on every run:
//! - press and release strictly alternate, and nothing is left held;
//! - deadlines never move backwards;
//! - presses stay on the absolute grid (`t0 + n*T`, late by less than the
//!   hold) and never burst to catch up;
//! - click-count and duration limits are respected.

use proptest::prelude::*;
use pulse_core::clicker::{ClickerSpec, compile};
use pulse_core::input::{Key, KeyState, LogicalInput, MouseButton, Pressable};
use pulse_core::scheduler::{Executor, Next};
use pulse_core::time::MS;
use pulse_core::{HeldInputs, Limits, Nanos};

/// Simulate a run. `lateness[i]` is added to the i-th wakeup; `cancel_after`
/// cancels after that many wakeups; runs are capped at `max_wall`.
fn simulate(
    spec: &ClickerSpec,
    t0: Nanos,
    lateness: &[Nanos],
    cancel_after: Option<usize>,
    max_wall: Nanos,
) -> (Vec<(Nanos, LogicalInput)>, Executor) {
    let program = compile(spec, &Limits::STANDARD).expect("valid by construction").program;
    let mut ex = Executor::new(program, t0);
    let mut log = Vec::new();
    let mut now = t0;
    let mut last_deadline = t0;
    for i in 0..2_000_000usize {
        let step = if cancel_after == Some(i) || now > t0 + max_wall {
            ex.cancel()
        } else {
            ex.poll(now)
        };
        log.extend(step.emit.map(|e| (now, e)));
        match step.next {
            Next::At(t) => {
                assert!(t >= last_deadline, "deadline moved backwards: {t} < {last_deadline}");
                last_deadline = t;
                let late = lateness.get(i % lateness.len().max(1)).copied().unwrap_or(0);
                now = now.max(t) + late;
            }
            Next::Done(_) => break,
        }
    }
    (log, ex)
}

fn assert_valid_stream(events: &[(Nanos, LogicalInput)]) {
    let mut held = HeldInputs::new();
    for (t, e) in events {
        held.apply(e)
            .unwrap_or_else(|err| panic!("invalid transition at {t}: {e} ({err})"));
    }
    assert!(held.is_empty(), "stream leaves inputs held: {held:?}");
}

fn target_strategy() -> impl Strategy<Value = Pressable> {
    prop_oneof![
        prop::sample::select(vec![MouseButton::Left, MouseButton::Right, MouseButton::Middle])
            .prop_map(Pressable::Button),
        prop::sample::select(vec!["E", "Space", "F", "1", "LeftShift"])
            .prop_map(|k| Pressable::Key(Key::parse(k).expect("key"))),
    ]
}

fn spec_strategy() -> impl Strategy<Value = ClickerSpec> {
    (
        target_strategy(),
        10u64..400,
        prop_oneof![Just(0u64), 1u64..20],
        prop_oneof![Just(0u64), 1u64..3_000],
    )
        .prop_flat_map(|(target, interval, count, duration)| {
            (1u64..interval).prop_map(move |hold| ClickerSpec {
                target,
                interval_ms: interval,
                hold_ms: hold,
                count,
                duration_ms: duration,
            })
        })
}

fn lateness_strategy() -> impl Strategy<Value = Vec<Nanos>> {
    prop::collection::vec(
        prop_oneof![
            8 => 0u64..200_000,     // typical: < 200 µs
            2 => 0u64..5 * MS,      // moderate load
            1 => 0u64..800 * MS,    // stalls
        ],
        1..64,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 5000, ..ProptestConfig::default() })]

    #[test]
    fn clicker_runs_never_stick_drift_or_burst(
        spec in spec_strategy(),
        lateness in lateness_strategy(),
        cancel_after in prop::option::of(0usize..400),
        t0 in 0u64..1_000_000_000_000,
    ) {
        let (log, ex) = simulate(&spec, t0, &lateness, cancel_after, 60_000 * MS);
        assert_valid_stream(&log);
        prop_assert!(ex.is_done());
        prop_assert!(!ex.is_held());
        prop_assert!(log.iter().all(|(_, e)| e.pressable().0 == spec.target), "only the target is pressed");

        let period = spec.interval_ms * MS;
        let hold = spec.hold_ms * MS;
        let presses: Vec<Nanos> = log.iter().filter(|(_, e)| e.pressable().1 == KeyState::Pressed).map(|(t, _)| *t).collect();
        for t in &presses {
            let offset = (t - t0) % period;
            prop_assert!(offset < hold, "press at {} is {} ns off-grid", t - t0, offset);
        }
        for w in presses.windows(2) {
            prop_assert!(w[1] - w[0] >= period / 2, "presses too close: burst");
        }
        if spec.count > 0 {
            prop_assert!(presses.len() as u64 <= spec.count);
        }
        if spec.duration_ms > 0 {
            for t in &presses {
                prop_assert!(t - t0 < spec.duration_ms * MS + hold);
            }
        }
    }
}

#[test]
fn long_run_has_no_drift() {
    // 1 hour at 10 CPS with 0–150 µs jitter on every wakeup.
    let spec = ClickerSpec {
        target: Pressable::Button(MouseButton::Left),
        interval_ms: 100,
        hold_ms: 20,
        count: 36_000,
        duration_ms: 0,
    };
    let jitter: Vec<Nanos> = (0..97).map(|i| (i * 7_919) % 150_000).collect();
    let (log, ex) = simulate(&spec, 5 * MS, &jitter, None, 7_200_000 * MS);
    assert_valid_stream(&log);
    assert_eq!(ex.stats().clicks, 36_000);
    assert_eq!(ex.stats().missed_deadlines, 0);
    let last_press = log
        .iter()
        .rev()
        .find(|(_, e)| e.is_press())
        .map(|(t, _)| *t)
        .expect("presses");
    let ideal = 5 * MS + 35_999 * 100 * MS;
    assert!(
        last_press >= ideal && last_press - ideal < 150_000,
        "drift: {} ns",
        last_press - ideal
    );
}
