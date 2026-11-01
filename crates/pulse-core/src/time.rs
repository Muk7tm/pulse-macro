//! Monotonic time representation used by the pure core.
//!
//! The core never reads a clock itself; callers pass `now` explicitly. This
//! keeps the scheduler deterministic under test (fake clock) while the engine
//! feeds it `CLOCK_MONOTONIC` readings.

/// Nanoseconds on a monotonic timeline.
pub type Nanos = u64;

pub const US: Nanos = 1_000;
pub const MS: Nanos = 1_000_000;
pub const SEC: Nanos = 1_000_000_000;

pub const fn ms(n: u64) -> Nanos {
    n.saturating_mul(MS)
}
