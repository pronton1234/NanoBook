//! R1b: levels stored **inline**, so reaching them costs no extra pointer chase.
//!
//! [`SortedLevels`](crate::SortedLevels) picked the right *algorithm* — the
//! measured book is two levels deep at the median, so a short sorted scan beats
//! both a tree and a tick-indexed array — and still only bought 1.20x over the
//! `BTreeMap` baseline. Decomposing the update path showed why, and it was not
//! what I expected:
//!
//! ```text
//!   hash + touch levels    ~2-15 ns/upd
//!   full update path       51.2 ns/upd
//!   => level container     48.8 ns/upd
//! ```
//!
//! The scan is not the cost. The *indirection* is. `SortedLevels` holds two
//! `Vec`s, so reading a price means loading the `SymbolBook`, then dereferencing
//! a heap pointer into memory allocated at some unrelated moment, then doing it
//! again for the sizes. Two levels of data, two cache misses to reach them, and
//! the algorithmic win drowns underneath.
//!
//! So store the levels in the struct. A `SymbolBook` load then brings the prices
//! with it, and the common case does no chasing at all.
//!
//! `INLINE = 8` comes from the measured depth distribution: p50 is 2 and p90 is
//! 5, so eight slots hold the overwhelming majority of sides entirely inline,
//! and eight prices are exactly one 64-byte cache line. Beyond that the level
//! set **spills wholesale** to heap vectors rather than straddling both — a
//! split representation would need every operation to reason about which half a
//! price belongs in, and p99 is 14, so the spill path is rare enough that its
//! simplicity is worth more than its speed.

use deep::Price;

use crate::levels::Levels;

/// Inline capacity. Eight `Price` values are one cache line; p90 depth is 5.
pub const INLINE: usize = 8;

#[derive(Debug, Clone)]
enum Storage {
    /// Entirely inline. `n` slots of `prices`/`sizes` are live, sorted ascending.
    Inline {
        n: u8,
        prices: [Price; INLINE],
        sizes: [u32; INLINE],
    },
    /// Deeper than `INLINE`. All levels live here; none remain inline.
    Spilled { prices: Vec<Price>, sizes: Vec<u32> },
}

/// Sorted levels, inline until they outgrow [`INLINE`].
#[derive(Debug, Clone)]
pub struct InlineLevels {
    storage: Storage,
}

impl Default for InlineLevels {
    fn default() -> Self {
        Self {
            storage: Storage::Inline {
                n: 0,
                prices: [Price::ZERO; INLINE],
                sizes: [0; INLINE],
            },
        }
    }
}

/// Linear search over a sorted slice: `Ok(i)` if present, `Err(i)` to insert at.
#[inline(always)]
fn find(prices: &[Price], price: Price) -> Result<usize, usize> {
    for (i, &p) in prices.iter().enumerate() {
        if p == price {
            return Ok(i);
        }
        if p > price {
            return Err(i);
        }
    }
    Err(prices.len())
}

impl InlineLevels {
    /// True while every level is held inline.
    #[must_use]
    pub fn is_inline(&self) -> bool {
        matches!(self.storage, Storage::Inline { .. })
    }

    /// Move an inline set to the heap to make room for one more level.
    #[cold]
    #[inline(never)]
    fn spill(&mut self) {
        let Storage::Inline { n, prices, sizes } = &self.storage else {
            return;
        };
        let n = *n as usize;
        let mut p = Vec::with_capacity(INLINE * 4);
        let mut s = Vec::with_capacity(INLINE * 4);
        p.extend_from_slice(&prices[..n]);
        s.extend_from_slice(&sizes[..n]);
        self.storage = Storage::Spilled {
            prices: p,
            sizes: s,
        };
    }
}

impl Levels for InlineLevels {
    #[inline]
    fn set(&mut self, price: Price, size: u32) {
        match &mut self.storage {
            Storage::Inline { n, prices, sizes } => {
                let len = *n as usize;
                match find(&prices[..len], price) {
                    Ok(i) => {
                        if size == 0 {
                            prices.copy_within(i + 1..len, i);
                            sizes.copy_within(i + 1..len, i);
                            *n -= 1;
                        } else {
                            sizes[i] = size;
                        }
                        return;
                    }
                    Err(i) => {
                        // A delete for a level we never held: nothing to do.
                        if size == 0 {
                            return;
                        }
                        if len < INLINE {
                            prices.copy_within(i..len, i + 1);
                            sizes.copy_within(i..len, i + 1);
                            prices[i] = price;
                            sizes[i] = size;
                            *n += 1;
                            return;
                        }
                    }
                }
                // Full and this price is new: spill, then retry on the heap.
                self.spill();
                self.set(price, size);
            }
            Storage::Spilled { prices, sizes } => match find(prices, price) {
                Ok(i) => {
                    if size == 0 {
                        prices.remove(i);
                        sizes.remove(i);
                    } else {
                        sizes[i] = size;
                    }
                }
                Err(i) => {
                    if size != 0 {
                        prices.insert(i, price);
                        sizes.insert(i, size);
                    }
                }
            },
        }
    }

    #[inline]
    fn max(&self) -> Option<(Price, u32)> {
        match &self.storage {
            Storage::Inline { n, prices, sizes } => {
                let i = (*n as usize).checked_sub(1)?;
                Some((prices[i], sizes[i]))
            }
            Storage::Spilled { prices, sizes } => {
                let i = prices.len().checked_sub(1)?;
                Some((prices[i], sizes[i]))
            }
        }
    }

    #[inline]
    fn min(&self) -> Option<(Price, u32)> {
        match &self.storage {
            Storage::Inline { n, prices, sizes } => {
                if *n == 0 {
                    return None;
                }
                Some((prices[0], sizes[0]))
            }
            Storage::Spilled { prices, sizes } => Some((*prices.first()?, sizes[0])),
        }
    }

    #[inline]
    fn len(&self) -> usize {
        match &self.storage {
            Storage::Inline { n, .. } => *n as usize,
            Storage::Spilled { prices, .. } => prices.len(),
        }
    }

    fn total_size(&self) -> u64 {
        match &self.storage {
            Storage::Inline { n, sizes, .. } => {
                sizes[..*n as usize].iter().map(|&s| s as u64).sum()
            }
            Storage::Spilled { sizes, .. } => sizes.iter().map(|&s| s as u64).sum(),
        }
    }

    fn for_each(&self, mut f: impl FnMut(Price, u32)) {
        match &self.storage {
            Storage::Inline { n, prices, sizes } => {
                for i in 0..*n as usize {
                    f(prices[i], sizes[i]);
                }
            }
            Storage::Spilled { prices, sizes } => {
                for (&p, &s) in prices.iter().zip(sizes) {
                    f(p, s);
                }
            }
        }
    }

    fn clear(&mut self) {
        // Returning to the inline representation releases the spill allocation,
        // which matters at a session reset: without it a book that was briefly
        // deep would hold its heap vectors for the rest of the run.
        self.storage = Storage::Inline {
            n: 0,
            prices: [Price::ZERO; INLINE],
            sizes: [0; INLINE],
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::BTreeLevels;

    fn px(d: i64, c: i64) -> Price {
        Price::from_parts(d, c * 100)
    }

    fn assert_sorted(l: &InlineLevels) {
        let mut v = Vec::new();
        l.for_each(|p, _| v.push(p));
        assert!(
            v.windows(2).all(|w| w[0] < w[1]),
            "levels must stay strictly ascending: {v:?}"
        );
        assert_eq!(v.len(), l.len());
    }

    #[test]
    fn stays_inline_below_capacity() {
        let mut l = InlineLevels::default();
        for i in 0..INLINE {
            l.set(px(10, i as i64), (i + 1) as u32);
        }
        assert!(l.is_inline());
        assert_eq!(l.len(), INLINE);
        assert_sorted(&l);
    }

    /// The boundary the whole type turns on.
    #[test]
    fn spills_only_when_a_new_price_will_not_fit() {
        let mut l = InlineLevels::default();
        for i in 0..INLINE {
            l.set(px(10, i as i64), 1);
        }
        assert!(l.is_inline());
        // Resizing an existing level must NOT spill; there is room already.
        l.set(px(10, 0), 999);
        assert!(l.is_inline(), "a resize must not force a spill");
        // A genuinely new price does.
        l.set(px(20, 0), 1);
        assert!(!l.is_inline());
        assert_eq!(l.len(), INLINE + 1);
        assert_sorted(&l);
        assert_eq!(l.max(), Some((px(20, 0), 1)));
    }

    /// A delete at capacity has nowhere to go and must not allocate.
    #[test]
    fn delete_at_capacity_does_not_spill() {
        let mut l = InlineLevels::default();
        for i in 0..INLINE {
            l.set(px(10, i as i64), 1);
        }
        l.set(px(99, 0), 0); // absent
        assert!(l.is_inline());
        l.set(px(10, 3), 0); // present
        assert!(l.is_inline());
        assert_eq!(l.len(), INLINE - 1);
        assert_sorted(&l);
    }

    #[test]
    fn inserts_in_the_middle_shift_correctly() {
        let mut l = InlineLevels::default();
        l.set(px(10, 0), 1);
        l.set(px(10, 4), 2);
        l.set(px(10, 2), 3);
        assert_sorted(&l);
        assert_eq!(l.min(), Some((px(10, 0), 1)));
        assert_eq!(l.max(), Some((px(10, 4), 2)));
        let mut seen = Vec::new();
        l.for_each(|p, s| seen.push((p, s)));
        assert_eq!(seen[1], (px(10, 2), 3));
    }

    #[test]
    fn clear_returns_to_inline_and_releases_the_spill() {
        let mut l = InlineLevels::default();
        for i in 0..(INLINE + 20) {
            l.set(px(10, i as i64), 1);
        }
        assert!(!l.is_inline());
        l.clear();
        assert!(l.is_inline(), "clear must release the spill allocation");
        assert!(l.is_empty());
    }

    /// Differential test against the R0 reference, driven hard enough to cross
    /// the inline/spill boundary in both directions many times.
    #[test]
    fn agrees_with_the_btree_reference_across_the_spill_boundary() {
        let mut fast = InlineLevels::default();
        let mut reference = BTreeLevels::default();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for step in 0..50_000 {
            let r = next();
            // 24 prices against an inline capacity of 8 guarantees repeated
            // spilling and repeated draining back below the boundary.
            let price = Price::from_parts(100, (r % 24) as i64 * 100);
            let size = if r % 10 < 4 {
                0
            } else {
                (r >> 8) as u32 % 5_000 + 1
            };
            fast.set(price, size);
            reference.set(price, size);

            assert_sorted(&fast);
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
}
