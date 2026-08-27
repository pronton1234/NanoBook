//! R1: a sorted, structure-of-arrays price ladder.
//!
//! This is not the textbook answer, and the measurement is why. A full IEX
//! trading day, sampled every million messages across 6,530 symbols, gives:
//!
//! ```text
//!   levels per side   p50 2   p90 5   p99 14   max 52
//! ```
//!
//! Books are **shallow**. The usual advice for a hot order book — a flat array
//! indexed by price tick, with a bitmap to find the touch — is built for a dense
//! ladder hundreds of levels deep. Applied here it would reserve hundreds of
//! slots to hold a median of *two*, so every touch scan would walk cache lines
//! of zeros to find one number. The clever structure would lose to a linear scan
//! over two elements, and it would lose on the thing it was chosen for.
//!
//! So: keep the levels sorted and contiguous, and scan them.
//!
//! Two decisions follow from `p50 = 2`:
//!
//! * **Linear scan, not binary search.** At five elements a binary search is
//!   two or three unpredictable branches; a linear scan is five perfectly
//!   predicted ones over data already in a register. Binary search wins
//!   asymptotically and loses at the size that actually occurs.
//! * **Structure of arrays.** Prices live in one `Vec` and sizes in another, so
//!   the scan — which reads only prices — touches 8 prices per 64-byte line
//!   instead of 5 interleaved price/size pairs. At `p90 = 5` the entire search
//!   is one cache line.
//!
//! `max()` and `min()` are then the last and first elements: no search at all,
//! which matters because the touch is read far more often than it moves.

use deep::Price;

use crate::levels::Levels;

/// Levels held sorted ascending by price, prices and sizes in parallel arrays.
#[derive(Debug, Default, Clone)]
pub struct SortedLevels {
    /// Ascending, no duplicates. Parallel to `sizes`.
    prices: Vec<Price>,
    sizes: Vec<u32>,
}

impl SortedLevels {
    /// Locate `price`: `Ok(i)` when present, `Err(i)` at the insertion point.
    ///
    /// Deliberately a forward linear scan rather than `binary_search`. See the
    /// module docs: at the depths this feed actually produces, the branch
    /// predictor beats the algorithm.
    #[inline(always)]
    fn find(&self, price: Price) -> Result<usize, usize> {
        for (i, &p) in self.prices.iter().enumerate() {
            if p == price {
                return Ok(i);
            }
            if p > price {
                return Err(i);
            }
        }
        Err(self.prices.len())
    }

    /// Levels from the best bid downward, or the best ask upward.
    pub fn ascending(&self) -> impl Iterator<Item = (Price, u32)> + '_ {
        self.prices.iter().copied().zip(self.sizes.iter().copied())
    }
}

impl Levels for SortedLevels {
    #[inline]
    fn set(&mut self, price: Price, size: u32) {
        match self.find(price) {
            Ok(i) => {
                if size == 0 {
                    self.prices.remove(i);
                    self.sizes.remove(i);
                } else {
                    self.sizes[i] = size;
                }
            }
            Err(i) => {
                // A delete for a level we do not hold is normal when joining a
                // live feed mid-session, and must not insert a zero-size level.
                if size != 0 {
                    self.prices.insert(i, price);
                    self.sizes.insert(i, size);
                }
            }
        }
    }

    #[inline]
    fn max(&self) -> Option<(Price, u32)> {
        let i = self.prices.len().checked_sub(1)?;
        Some((self.prices[i], self.sizes[i]))
    }

    #[inline]
    fn min(&self) -> Option<(Price, u32)> {
        Some((*self.prices.first()?, self.sizes[0]))
    }

    #[inline]
    fn len(&self) -> usize {
        self.prices.len()
    }

    fn total_size(&self) -> u64 {
        self.sizes.iter().map(|&s| s as u64).sum()
    }

    fn for_each(&self, mut f: impl FnMut(Price, u32)) {
        for (p, s) in self.ascending() {
            f(p, s);
        }
    }

    fn clear(&mut self) {
        self.prices.clear();
        self.sizes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::BTreeLevels;

    fn px(d: i64, c: i64) -> Price {
        Price::from_parts(d, c * 100)
    }

    /// The parallel arrays must never disagree in length. If they do, a price
    /// and a size from different levels get paired and the book reports a
    /// plausible, wrong quote.
    fn assert_parallel(l: &SortedLevels) {
        assert_eq!(l.prices.len(), l.sizes.len(), "prices and sizes desynced");
        assert!(
            l.prices.windows(2).all(|w| w[0] < w[1]),
            "prices must stay strictly ascending: {:?}",
            l.prices
        );
    }

    #[test]
    fn inserts_stay_sorted_whatever_the_arrival_order() {
        let mut l = SortedLevels::default();
        for (p, s) in [
            (px(10, 2), 2),
            (px(9, 98), 4),
            (px(10, 0), 3),
            (px(11, 0), 1),
            (px(9, 0), 5),
        ] {
            l.set(p, s);
        }
        assert_parallel(&l);
        assert_eq!(l.min(), Some((px(9, 0), 5)));
        assert_eq!(l.max(), Some((px(11, 0), 1)));
        assert_eq!(l.len(), 5);
    }

    #[test]
    fn resize_does_not_duplicate_a_level() {
        let mut l = SortedLevels::default();
        l.set(px(10, 0), 100);
        l.set(px(10, 0), 700);
        assert_parallel(&l);
        assert_eq!(l.len(), 1);
        assert_eq!(l.max(), Some((px(10, 0), 700)));
    }

    #[test]
    fn delete_removes_from_both_arrays() {
        let mut l = SortedLevels::default();
        l.set(px(10, 0), 100);
        l.set(px(10, 1), 200);
        l.set(px(10, 2), 300);
        l.set(px(10, 1), 0);
        assert_parallel(&l);
        assert_eq!(l.len(), 2);
        assert_eq!(l.min(), Some((px(10, 0), 100)));
        assert_eq!(l.max(), Some((px(10, 2), 300)));
    }

    #[test]
    fn deleting_an_absent_level_inserts_nothing() {
        let mut l = SortedLevels::default();
        l.set(px(10, 0), 100);
        l.set(px(42, 0), 0);
        assert_parallel(&l);
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn empties_cleanly() {
        let mut l = SortedLevels::default();
        l.set(px(10, 0), 100);
        l.set(px(10, 0), 0);
        assert_parallel(&l);
        assert!(l.is_empty());
        assert_eq!(l.max(), None);
        assert_eq!(l.min(), None);
    }

    /// Differential test against the R0 reference.
    ///
    /// The whole point of keeping `BTreeLevels` after writing a faster container
    /// is that any disagreement is a bug in the clever one. A deterministic
    /// pseudo-random operation stream exercises interleaved inserts, resizes and
    /// deletes far past what hand-written cases reach.
    #[test]
    fn agrees_with_the_btree_reference_over_random_operations() {
        let mut fast = SortedLevels::default();
        let mut reference = BTreeLevels::default();
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for step in 0..20_000 {
            let r = next();
            // 40 distinct prices, deliberately narrow so collisions and
            // resizes happen constantly rather than every price being unique.
            let price = Price::from_parts(100, (r % 40) as i64 * 100);
            // Roughly 40% deletes, matching the 38.9% measured on a real day.
            let size = if r % 10 < 4 {
                0
            } else {
                (r >> 8) as u32 % 5_000 + 1
            };
            fast.set(price, size);
            reference.set(price, size);

            assert_parallel(&fast);
            assert_eq!(fast.len(), reference.len(), "len diverged at step {step}");
            assert_eq!(fast.max(), reference.max(), "max diverged at step {step}");
            assert_eq!(fast.min(), reference.min(), "min diverged at step {step}");
            assert_eq!(
                fast.total_size(),
                reference.total_size(),
                "total diverged at step {step}"
            );
        }
    }

    /// Deep books are rare (p99 = 14, max 52) but must not be wrong.
    #[test]
    fn handles_a_book_deeper_than_anything_observed() {
        let mut l = SortedLevels::default();
        for i in 0..200 {
            l.set(Price::from_parts(50, i * 100), (i + 1) as u32);
        }
        assert_parallel(&l);
        assert_eq!(l.len(), 200);
        assert_eq!(l.min().unwrap().0, Price::from_parts(50, 0));
        assert_eq!(l.max().unwrap().0, Price::from_parts(50, 199 * 100));
    }
}
