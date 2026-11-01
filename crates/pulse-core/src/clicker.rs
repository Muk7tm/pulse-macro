//! The clicker: one mouse button or one keyboard key, pressed and released on
//! a fixed time grid until stopped (or until a click count / duration limit).
//!
//! Pulse is deliberately not a sequence-macro tool: there are no steps,
//! delays or loops to edit, only the settings below.

use crate::errors::{Issue, ValidationFailed};
use crate::input::Pressable;
use crate::time::{MS, Nanos};
use crate::validation::{self, Limits};

/// User-facing clicker settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClickerSpec {
    /// What to repeat.
    pub target: Pressable,
    /// Time between presses.
    pub interval_ms: u64,
    /// Press → release time.
    pub hold_ms: u64,
    /// Stop after this many clicks; 0 = until stopped.
    pub count: u64,
    /// Stop after this long; 0 = no time limit.
    pub duration_ms: u64,
}

/// A validated, immutable clicker program (cheap to copy into the runner).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Program {
    pub target: Pressable,
    pub period: Nanos,
    pub hold: Nanos,
    pub count: Option<u64>,
    pub max_duration: Option<Nanos>,
}

impl Program {
    pub fn clicks_per_second(&self) -> f64 {
        1e9 / self.period as f64
    }

    /// Length of a finite run (count and/or duration limit), if any.
    pub fn estimated_total(&self) -> Option<Nanos> {
        let by_count = self
            .count
            .map(|n| self.period.saturating_mul(n.saturating_sub(1)) + self.hold);
        match (by_count, self.max_duration) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compiled {
    pub program: Program,
    pub warnings: Vec<Issue>,
}

/// Validate clicker settings into a program. Issues use `clicker.*` paths.
pub fn compile(spec: &ClickerSpec, limits: &Limits) -> Result<Compiled, ValidationFailed> {
    let mut issues = Vec::new();
    let timing = validation::validate_click_timing(spec.interval_ms, spec.hold_ms, limits, "clicker", &mut issues);
    if spec.count > limits.max_count {
        issues.push(Issue::error(
            "clicker.count",
            format!("count exceeds the maximum of {}", limits.max_count),
        ));
    }
    if spec.duration_ms > limits.max_duration_ms {
        issues.push(Issue::error(
            "clicker.duration_ms",
            format!("duration exceeds the maximum of {} ms", limits.max_duration_ms),
        ));
    }
    let warnings = validation::finish(issues)?;
    let timing = timing.ok_or_else(|| ValidationFailed::single("clicker", "invalid timing"))?;
    Ok(Compiled {
        program: Program {
            target: spec.target,
            period: timing.period,
            hold: timing.hold,
            count: (spec.count > 0).then_some(spec.count),
            max_duration: (spec.duration_ms > 0).then(|| spec.duration_ms * MS),
        },
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, MouseButton};

    fn spec(interval_ms: u64, hold_ms: u64) -> ClickerSpec {
        ClickerSpec {
            target: Pressable::Button(MouseButton::Left),
            interval_ms,
            hold_ms,
            count: 0,
            duration_ms: 0,
        }
    }

    #[test]
    fn compiles_defaults() {
        let c = compile(&spec(100, 20), &Limits::STANDARD).expect("valid");
        assert_eq!(c.program.period, 100 * MS);
        assert_eq!(c.program.hold, 20 * MS);
        assert_eq!(c.program.count, None);
        assert_eq!(c.program.max_duration, None);
        assert!((c.program.clicks_per_second() - 10.0).abs() < 1e-9);
        assert!(c.warnings.is_empty());
    }

    #[test]
    fn keyboard_targets_are_supported() {
        let mut s = spec(50, 10);
        s.target = Pressable::Key(Key::parse("E").expect("key"));
        let c = compile(&s, &Limits::STANDARD).expect("valid");
        assert_eq!(c.program.target, s.target);
        assert_eq!(c.warnings.len(), 1, "sub-100 ms warning");
    }

    #[test]
    fn limits_and_finite_runs() {
        let mut s = spec(100, 20);
        s.count = 5;
        s.duration_ms = 10_000;
        let c = compile(&s, &Limits::STANDARD).expect("valid");
        assert_eq!(c.program.count, Some(5));
        assert_eq!(c.program.estimated_total(), Some(420 * MS));
        s.count = Limits::STANDARD.max_count + 1;
        let err = compile(&s, &Limits::STANDARD).expect_err("too many");
        assert!(err.issues.iter().any(|i| i.path == "clicker.count"));
    }

    #[test]
    fn invalid_timing_is_reported_with_paths() {
        let err = compile(&spec(15, 20), &Limits::STANDARD).expect_err("hold >= interval");
        assert!(err.issues.iter().any(|i| i.path == "clicker.hold_ms"));
        let err = compile(&spec(5, 2), &Limits::STANDARD).expect_err("below minimum");
        assert!(err.issues.iter().any(|i| i.path == "clicker.interval_ms"));
        assert!(compile(&spec(5, 2), &Limits::ADVANCED).is_ok());
    }
}
