//! Clock helpers. `CLOCK_MONOTONIC` drives all scheduling; `CLOCK_BOOTTIME`
//! is used only to detect suspend (it advances while suspended, monotonic
//! does not).

use pulse_core::Nanos;
use rustix::time::{ClockId, Timespec, clock_gettime};

fn to_nanos(t: Timespec) -> Nanos {
    (t.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(t.tv_nsec as u64)
}

pub fn to_timespec(ns: Nanos) -> Timespec {
    Timespec {
        tv_sec: (ns / 1_000_000_000) as i64,
        tv_nsec: (ns % 1_000_000_000) as i64,
    }
}

#[inline]
pub fn monotonic_now() -> Nanos {
    to_nanos(clock_gettime(ClockId::Monotonic))
}

#[inline]
pub fn boottime_now() -> Nanos {
    to_nanos(clock_gettime(ClockId::Boottime))
}

/// `boottime - monotonic`: grows by the suspended duration across a suspend.
#[inline]
pub fn suspend_offset() -> Nanos {
    boottime_now().saturating_sub(monotonic_now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timespec_roundtrip_and_monotonicity() {
        let a = monotonic_now();
        let b = monotonic_now();
        assert!(b >= a);
        assert_eq!(to_nanos(to_timespec(1_234_567_890_123)), 1_234_567_890_123);
        // Without a suspend in between, the offset is stable to well under 1 ms.
        let o1 = suspend_offset();
        let o2 = suspend_offset();
        assert!(o1.abs_diff(o2) < 1_000_000);
    }
}
