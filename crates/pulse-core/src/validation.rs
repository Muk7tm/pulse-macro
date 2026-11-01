//! Hard limits and click-timing validation.

use crate::errors::{Issue, ValidationFailed};
use crate::time::{MS, Nanos};

/// Sober has a reported chat-focus issue below this interval.
pub const SOBER_CAUTION_INTERVAL_MS: u64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Smallest permitted click interval.
    pub min_interval_ms: u64,
    pub max_interval_ms: u64,
    /// Smallest press→release time.
    pub min_hold_ms: u64,
    /// Largest "stop after N clicks".
    pub max_count: u64,
    /// Largest "stop after N ms".
    pub max_duration_ms: u64,
}

impl Limits {
    pub const STANDARD: Limits = Limits {
        min_interval_ms: 10,
        max_interval_ms: 3_600_000,
        min_hold_ms: 1,
        max_count: 10_000_000,
        max_duration_ms: 24 * 3_600_000,
    };

    /// `allow_advanced_rates = true`: permits intervals down to 1 ms.
    pub const ADVANCED: Limits = Limits {
        min_interval_ms: 1,
        ..Limits::STANDARD
    };

    pub const fn for_advanced(allow_advanced_rates: bool) -> Limits {
        if allow_advanced_rates {
            Limits::ADVANCED
        } else {
            Limits::STANDARD
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits::STANDARD
    }
}

/// Validated click timing in nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClickTiming {
    pub period: Nanos,
    pub hold: Nanos,
}

/// Validate interval/hold for a continuously repeating clicker. Issues are
/// addressed as `<path>.interval_ms` / `<path>.hold_ms`.
pub fn validate_click_timing(
    interval_ms: u64,
    hold_ms: u64,
    limits: &Limits,
    path: &str,
    issues: &mut Vec<Issue>,
) -> Option<ClickTiming> {
    let field = |f: &str| {
        if path.is_empty() {
            f.to_owned()
        } else {
            format!("{path}.{f}")
        }
    };
    let mut ok = true;
    if interval_ms == 0 {
        issues.push(Issue::error(
            field("interval_ms"),
            "interval must be greater than 0 ms (0 ms would flood input)",
        ));
        ok = false;
    } else if interval_ms < limits.min_interval_ms {
        issues.push(Issue::error(
            field("interval_ms"),
            format!(
                "interval {interval_ms} ms is below the minimum of {} ms; allow fast speeds to go lower",
                limits.min_interval_ms
            ),
        ));
        ok = false;
    } else if interval_ms > limits.max_interval_ms {
        issues.push(Issue::error(
            field("interval_ms"),
            format!("interval exceeds the maximum of {} ms", limits.max_interval_ms),
        ));
        ok = false;
    } else if interval_ms < SOBER_CAUTION_INTERVAL_MS {
        issues.push(Issue::warning(
            field("interval_ms"),
            format!(
                "intervals below {SOBER_CAUTION_INTERVAL_MS} ms are experimental; a chat-focus issue has been reported in Sober at high click rates"
            ),
        ));
    }
    if hold_ms < limits.min_hold_ms {
        issues.push(Issue::error(
            field("hold_ms"),
            format!("hold must be at least {} ms", limits.min_hold_ms),
        ));
        ok = false;
    } else if interval_ms != 0 && hold_ms >= interval_ms {
        issues.push(Issue::error(
            field("hold_ms"),
            format!("hold ({hold_ms} ms) must be shorter than the interval ({interval_ms} ms)"),
        ));
        ok = false;
    }
    if !ok {
        return None;
    }
    Some(ClickTiming {
        period: interval_ms.checked_mul(MS)?,
        hold: hold_ms.checked_mul(MS)?,
    })
}

/// Fail if any issue is an error; otherwise return the warnings.
pub fn finish(issues: Vec<Issue>) -> Result<Vec<Issue>, ValidationFailed> {
    if issues.iter().any(Issue::is_error) {
        Err(ValidationFailed { issues })
    } else {
        Ok(issues)
    }
}

/// Clicks per second for display purposes.
pub fn cps(interval_ms: u64) -> f64 {
    if interval_ms == 0 {
        0.0
    } else {
        1000.0 / interval_ms as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(interval: u64, hold: u64, limits: Limits) -> (Option<ClickTiming>, Vec<Issue>) {
        let mut issues = Vec::new();
        let t = validate_click_timing(interval, hold, &limits, "clicker", &mut issues);
        (t, issues)
    }

    #[test]
    fn rejects_zero_interval_and_bad_hold() {
        let (t, issues) = check(0, 20, Limits::STANDARD);
        assert!(t.is_none());
        assert!(issues.iter().any(|i| i.path == "clicker.interval_ms" && i.is_error()));

        let (t, issues) = check(100, 100, Limits::STANDARD);
        assert!(t.is_none());
        assert!(issues.iter().any(|i| i.path == "clicker.hold_ms"));

        let (t, _) = check(100, 0, Limits::STANDARD);
        assert!(t.is_none());
    }

    #[test]
    fn enforces_min_interval_unless_advanced() {
        assert!(check(5, 2, Limits::STANDARD).0.is_none());
        let (t, issues) = check(5, 2, Limits::ADVANCED);
        assert!(t.is_some());
        assert!(issues.iter().all(|i| !i.is_error()));
        assert!(issues.iter().any(|i| !i.is_error()), "sub-100ms warning expected");
    }

    #[test]
    fn default_timing_is_clean() {
        let (t, issues) = check(100, 20, Limits::STANDARD);
        assert_eq!(
            t,
            Some(ClickTiming {
                period: 100 * MS,
                hold: 20 * MS
            })
        );
        assert!(issues.is_empty());
    }

    #[test]
    fn cps_display() {
        assert_eq!(cps(100), 10.0);
        assert_eq!(cps(0), 0.0);
    }
}
