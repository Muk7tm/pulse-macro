//! Lock-free, allocation-free runtime metrics.
//!
//! The runner records into fixed atomic histograms with relaxed increments;
//! the control thread snapshots them only when Diagnostics are requested.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use pulse_protocol::HistogramSummary;

/// Exact 1 µs buckets below 16 µs, then 8 log-linear sub-buckets per octave
/// (≤12.5% relative bucket width) up to ~2^36 µs.
const LINEAR: usize = 16;
const SUB: usize = 8;
const OCTAVES: usize = 32;
pub const BUCKETS: usize = LINEAR + OCTAVES * SUB + 1;

fn bucket_of(us: u64) -> usize {
    if us < LINEAR as u64 {
        return us as usize;
    }
    let lz = 63 - us.leading_zeros() as usize; // floor(log2(us)) >= 4
    let octave = lz - 4;
    if octave >= OCTAVES {
        return BUCKETS - 1;
    }
    let sub = ((us >> (lz - 3)) & 0x7) as usize;
    LINEAR + octave * SUB + sub
}

/// Exclusive upper bound of a bucket, in µs.
fn bucket_upper(b: usize) -> u64 {
    if b < LINEAR {
        return b as u64 + 1;
    }
    if b >= BUCKETS - 1 {
        return u64::MAX;
    }
    let octave = (b - LINEAR) / SUB;
    let sub = ((b - LINEAR) % SUB) as u64;
    let lz = octave + 4;
    (9 + sub) << (lz - 3)
}

pub struct Histogram {
    buckets: [AtomicU64; BUCKETS],
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    #[inline]
    pub fn record_ns(&self, ns: u64) {
        self.buckets[bucket_of(ns / 1_000)].fetch_add(1, Relaxed);
        self.count.fetch_add(1, Relaxed);
        self.sum_ns.fetch_add(ns, Relaxed);
        self.max_ns.fetch_max(ns, Relaxed);
    }

    pub fn reset(&self) {
        for b in &self.buckets {
            b.store(0, Relaxed);
        }
        self.count.store(0, Relaxed);
        self.sum_ns.store(0, Relaxed);
        self.max_ns.store(0, Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Relaxed)
    }

    /// Bucket-upper-bound percentile (conservative), in µs.
    fn percentile(&self, counts: &[u64; BUCKETS], total: u64, p: f64) -> f64 {
        if total == 0 {
            return 0.0;
        }
        let rank = ((p * total as f64).ceil() as u64).clamp(1, total);
        let mut acc = 0;
        for (i, c) in counts.iter().enumerate() {
            acc += c;
            if acc >= rank {
                let max_us = self.max_ns.load(Relaxed) as f64 / 1_000.0;
                return (bucket_upper(i) as f64).min(max_us.max(1.0));
            }
        }
        self.max_ns.load(Relaxed) as f64 / 1_000.0
    }

    pub fn summary(&self, name: &str, description: &str) -> HistogramSummary {
        let counts: [u64; BUCKETS] = std::array::from_fn(|i| self.buckets[i].load(Relaxed));
        let total: u64 = counts.iter().sum();
        let sum = self.sum_ns.load(Relaxed);
        HistogramSummary {
            name: name.to_owned(),
            description: description.to_owned(),
            count: total,
            p50_us: self.percentile(&counts, total, 0.50),
            p95_us: self.percentile(&counts, total, 0.95),
            p99_us: self.percentile(&counts, total, 0.99),
            max_us: self.max_ns.load(Relaxed) as f64 / 1_000.0,
            mean_us: if total == 0 {
                0.0
            } else {
                sum as f64 / total as f64 / 1_000.0
            },
            buckets: counts
                .iter()
                .enumerate()
                .filter(|(_, c)| **c > 0)
                .map(|(i, c)| (bucket_upper(i) as f64, *c))
                .collect(),
        }
    }
}

/// Shared between the control thread and the runner.
#[derive(Default)]
pub struct Metrics {
    /// Actual wakeup time minus the absolute deadline.
    pub wake_jitter: Histogram,
    /// Hotkey event received by the engine → first event submitted.
    pub trigger_latency: Histogram,
    /// Time spent inside the backend `emit` call per batch.
    pub emit_duration: Histogram,
    /// Stop requested → all releases submitted.
    pub stop_latency: Histogram,
    pub runs: AtomicU64,
    pub submitted_clicks: AtomicU64,
    pub submitted_events: AtomicU64,
    pub missed_deadlines: AtomicU64,
    pub current_run_clicks: AtomicU64,
    pub backend_errors: AtomicU64,
}

impl Metrics {
    pub fn reset(&self) {
        self.wake_jitter.reset();
        self.trigger_latency.reset();
        self.emit_duration.reset();
        self.stop_latency.reset();
        for c in [
            &self.runs,
            &self.submitted_clicks,
            &self.submitted_events,
            &self.missed_deadlines,
            &self.backend_errors,
        ] {
            c.store(0, Relaxed);
        }
    }

    pub fn summaries(&self) -> Vec<HistogramSummary> {
        vec![
            self.wake_jitter
                .summary("wake_jitter", "timer wakeup minus absolute deadline"),
            self.trigger_latency
                .summary("trigger_latency", "hotkey received by engine to first event submitted"),
            self.emit_duration
                .summary("emit_duration", "time inside the backend submit call per batch"),
            self.stop_latency
                .summary("stop_latency", "stop requested to all releases submitted"),
        ]
    }

    pub fn counters(&self) -> pulse_protocol::Counters {
        pulse_protocol::Counters {
            runs: self.runs.load(Relaxed),
            submitted_clicks: self.submitted_clicks.load(Relaxed),
            submitted_events: self.submitted_events.load(Relaxed),
            missed_deadlines: self.missed_deadlines.load(Relaxed),
            current_run_clicks: self.current_run_clicks.load(Relaxed),
            backend_errors: self.backend_errors.load(Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_monotonic_and_cover_values() {
        let mut last = 0;
        for b in 0..BUCKETS - 1 {
            let u = bucket_upper(b);
            assert!(u > last, "bucket {b}");
            last = u;
        }
        for us in [0u64, 1, 15, 16, 17, 100, 1_000, 12_345, 1 << 30] {
            let b = bucket_of(us);
            assert!(us < bucket_upper(b), "{us} in bucket {b} (< {})", bucket_upper(b));
            if b > 0 {
                assert!(us >= bucket_upper(b - 1), "{us}");
            }
        }
    }

    #[test]
    fn percentiles() {
        let h = Histogram::default();
        for i in 0..1000u64 {
            h.record_ns(i * 1_000); // 0..999 µs
        }
        let s = h.summary("x", "");
        assert_eq!(s.count, 1000);
        assert!((450.0..=600.0).contains(&s.p50_us), "{}", s.p50_us);
        assert!((940.0..=1000.0).contains(&s.p99_us), "{}", s.p99_us);
        assert!((s.max_us - 999.0).abs() < 1e-9);
        h.reset();
        assert_eq!(h.summary("x", "").count, 0);
    }
}
