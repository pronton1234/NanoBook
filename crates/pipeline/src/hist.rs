//! A log-linear latency histogram, in the HdrHistogram style.
//!
//! Every latency claim in this project is made through this type, so it is the
//! one piece of code that must not be quietly wrong. This session has already
//! produced four measurements that ran cleanly while measuring the wrong thing —
//! a book audited after the close so every value was zero, percentiles pinned to
//! a histogram's own bucket count, a probe that never touched the memory it was
//! supposed to be timing, and a benchmark floor that excluded work the things
//! under test were paying for. A hand-rolled instrument is exactly where that
//! failure recurs, so this one is tested against brute-force exact quantiles
//! rather than against my belief about how it works.
//!
//! ## Why not a mean
//!
//! A mean latency answers no question anyone has. Tail latency is the subject:
//! a feed handler that is fast on average and stalls for a millisecond at p99.9
//! has stalled through the only messages that mattered. So this records the
//! whole distribution and reports quantiles.
//!
//! ## Why log-linear
//!
//! Recording every distinct nanosecond from 1ns to 10s would need 10^10 counters.
//! Instead the bucket width grows with the magnitude of the value, holding
//! *relative* precision roughly constant: values are stored to within
//! `1 / 2^PRECISION_BITS` of their true magnitude, so 3 significant figures costs
//! a few thousand counters rather than ten billion.
//!
//! ## Saturation is reported, never silent
//!
//! A value above the recordable range is counted separately and surfaced by
//! [`Histogram::overflows`]. The earlier bug in this project's audit was a
//! histogram that silently clamped, so `p99` reported the bucket count and read
//! as a fact about the market. A quantile that lands in the overflow bucket here
//! returns [`u64::MAX`] rather than a plausible number.

/// Mantissa bits kept per power of two. 11 bits gives ~0.05% relative error,
/// comfortably better than three significant figures.
const PRECISION_BITS: u32 = 11;
/// Values below this are counted exactly, one counter per nanosecond, because
/// below the mantissa width there is nothing to approximate.
const LINEAR_LIMIT: u64 = 1 << PRECISION_BITS;
/// Largest exponent tracked. 2^42 ns is a bit over an hour.
const MAX_EXPONENT: u32 = 42;

const LINEAR_SLOTS: usize = LINEAR_LIMIT as usize;
const LOG_SLOTS: usize = ((MAX_EXPONENT - PRECISION_BITS) as usize) << PRECISION_BITS;

/// Latency counts, bucketed log-linearly.
#[derive(Clone)]
pub struct Histogram {
    counts: Vec<u64>,
    total: u64,
    /// Values too large to record. Never folded into the last bucket.
    overflows: u64,
    min: u64,
    max: u64,
    sum: u128,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    #[must_use]
    pub fn new() -> Self {
        Self {
            counts: vec![0; LINEAR_SLOTS + LOG_SLOTS],
            total: 0,
            overflows: 0,
            min: u64::MAX,
            max: 0,
            sum: 0,
        }
    }

    /// Bucket index for a value, and the inverse of [`Self::bucket_low`].
    #[inline]
    fn index(value: u64) -> Option<usize> {
        if value < LINEAR_LIMIT {
            return Some(value as usize);
        }
        // floor(log2(value)); value >= LINEAR_LIMIT so this is >= PRECISION_BITS.
        let exponent = 63 - value.leading_zeros();
        if exponent >= MAX_EXPONENT {
            return None;
        }
        let shift = exponent - PRECISION_BITS;
        let mantissa = (value >> shift) & (LINEAR_LIMIT - 1);
        let bucket = (exponent - PRECISION_BITS) as usize;
        Some(LINEAR_SLOTS + (bucket << PRECISION_BITS) + mantissa as usize)
    }

    /// Smallest value that lands in `index`.
    ///
    /// Reporting the bucket's lower bound rather than its midpoint keeps every
    /// quantile a value that was actually representable, and makes the reported
    /// figure an under-estimate by at most the bucket width — which is the
    /// conservative direction for a latency number.
    #[inline]
    fn bucket_low(index: usize) -> u64 {
        if index < LINEAR_SLOTS {
            return index as u64;
        }
        let i = index - LINEAR_SLOTS;
        // `index` derives the slot from `exponent` and the mantissa bits below
        // the leading one. Inverting it means restoring that implicit leading
        // bit and shifting back: with `exponent = bucket + PRECISION_BITS`, the
        // shift used on the way in was `exponent - PRECISION_BITS`, i.e. exactly
        // `bucket`.
        let bucket = (i >> PRECISION_BITS) as u32;
        let mantissa = (i & (LINEAR_SLOTS - 1)) as u64;
        (LINEAR_LIMIT | mantissa) << bucket
    }

    #[inline]
    pub fn record(&mut self, value: u64) {
        match Self::index(value) {
            Some(i) => {
                self.counts[i] += 1;
                self.total += 1;
                self.sum += value as u128;
                if value < self.min {
                    self.min = value;
                }
                if value > self.max {
                    self.max = value;
                }
            }
            None => {
                self.overflows += 1;
                self.total += 1;
                if value > self.max {
                    self.max = value;
                }
            }
        }
    }

    #[must_use]
    pub fn count(&self) -> u64 {
        self.total
    }

    /// Values that exceeded the recordable range. Never silently clamped.
    #[must_use]
    pub fn overflows(&self) -> u64 {
        self.overflows
    }

    #[must_use]
    pub fn min(&self) -> u64 {
        if self.total == 0 {
            0
        } else {
            self.min
        }
    }

    #[must_use]
    pub fn max(&self) -> u64 {
        self.max
    }

    /// Reported only for completeness. Do not lead with it — see the module docs.
    #[must_use]
    pub fn mean(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.sum as f64 / self.total as f64
        }
    }

    /// Value at `q` in `[0, 1]`, or [`u64::MAX`] if that quantile lies in the
    /// overflow bucket, where no value can honestly be reported.
    #[must_use]
    pub fn quantile(&self, q: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        // Rank uses ceil so q = 0 gives the minimum and q = 1 gives the maximum,
        // matching the usual "smallest value at or above this fraction".
        let rank = (q.clamp(0.0, 1.0) * self.total as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            if c == 0 {
                continue;
            }
            seen += c;
            if seen >= rank {
                return Self::bucket_low(i);
            }
        }
        u64::MAX
    }

    /// Merge another histogram, for combining per-thread samples.
    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += *b;
        }
        self.total += other.total;
        self.overflows += other.overflows;
        self.sum += other.sum;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    pub fn clear(&mut self) {
        self.counts.fill(0);
        self.total = 0;
        self.overflows = 0;
        self.min = u64::MAX;
        self.max = 0;
        self.sum = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact quantile by sorting, to check the histogram against.
    fn exact(values: &mut [u64], q: f64) -> u64 {
        values.sort_unstable();
        let rank = (q * values.len() as f64).ceil().max(1.0) as usize;
        values[rank - 1]
    }

    #[test]
    fn small_values_are_exact() {
        let mut h = Histogram::new();
        for v in 0..LINEAR_LIMIT {
            h.record(v);
        }
        // Below the linear limit every value has its own counter, so quantiles
        // must be exact, not approximate.
        assert_eq!(h.quantile(0.0), 0);
        assert_eq!(h.quantile(0.5), LINEAR_LIMIT / 2 - 1);
        assert_eq!(h.quantile(1.0), LINEAR_LIMIT - 1);
        assert_eq!(h.count(), LINEAR_LIMIT);
    }

    #[test]
    fn a_single_value_reports_itself_at_every_quantile() {
        for v in [1u64, 999, 4096, 100_000, 1_000_000_000] {
            let mut h = Histogram::new();
            h.record(v);
            for q in [0.0, 0.5, 0.99, 1.0] {
                let got = h.quantile(q);
                let err = (v as f64 - got as f64).abs() / v as f64;
                assert!(err <= 0.001, "v={v} q={q} got={got} rel err {err}");
            }
        }
    }

    /// The property that matters: a reported quantile is within the histogram's
    /// stated relative precision of the true one, across a wide dynamic range.
    #[test]
    fn quantiles_match_brute_force_within_precision() {
        let mut h = Histogram::new();
        let mut raw = Vec::new();
        // Deterministic spread over five orders of magnitude, so the check
        // exercises the linear region and many exponents.
        let mut state = 0x243F_6A88_85A3_08D3u64;
        for _ in 0..200_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let v = 20 + (state % 2_000_000);
            h.record(v);
            raw.push(v);
        }
        for q in [0.0, 0.01, 0.5, 0.9, 0.99, 0.999, 1.0] {
            let want = exact(&mut raw, q);
            let got = h.quantile(q);
            let err = (want as f64 - got as f64).abs() / want as f64;
            assert!(
                err <= 0.001,
                "q={q}: exact {want}, histogram {got}, rel err {err}"
            );
        }
    }

    /// The bug this project already hit once: a saturating histogram reporting
    /// its own bucket count as a fact about the world.
    #[test]
    fn out_of_range_values_overflow_rather_than_clamp() {
        let mut h = Histogram::new();
        h.record(1_000);
        h.record(u64::MAX);
        assert_eq!(h.overflows(), 1, "the huge value must be counted apart");
        assert_eq!(h.quantile(0.0), 1_000);
        assert_eq!(
            h.quantile(1.0),
            u64::MAX,
            "a quantile in the overflow bucket must refuse to invent a value"
        );
        assert_eq!(h.max(), u64::MAX, "max still tracks the real extreme");
    }

    #[test]
    fn empty_histogram_reports_nothing_rather_than_panicking() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.quantile(0.5), 0);
        assert_eq!(h.mean(), 0.0);
        assert_eq!(h.min(), 0);
    }

    #[test]
    fn merge_is_equivalent_to_recording_into_one() {
        let mut a = Histogram::new();
        let mut b = Histogram::new();
        let mut both = Histogram::new();
        for v in 1..5_000u64 {
            if v % 2 == 0 {
                a.record(v);
            } else {
                b.record(v);
            }
            both.record(v);
        }
        a.merge(&b);
        assert_eq!(a.count(), both.count());
        for q in [0.5, 0.9, 0.99, 1.0] {
            assert_eq!(a.quantile(q), both.quantile(q), "q={q}");
        }
    }

    /// `bucket_low` must be the true inverse of `index`, or every reported
    /// quantile is off by a bucket in a way no other test would notice.
    #[test]
    fn bucket_low_round_trips_through_index() {
        for v in [
            0u64,
            1,
            2047,
            2048,
            2049,
            4095,
            4096,
            10_000,
            1_000_000,
            1_000_000_000,
        ] {
            let i = Histogram::index(v).expect("in range");
            let low = Histogram::bucket_low(i);
            assert!(low <= v, "bucket low {low} must not exceed the value {v}");
            assert_eq!(
                Histogram::index(low),
                Some(i),
                "value {v} -> index {i} -> low {low} must map back to {i}"
            );
        }
    }
}
