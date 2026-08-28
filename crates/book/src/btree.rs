//! R0: the obvious implementation, `BTreeMap<Price, u32>`.
//!
//! This is the baseline every other container is measured against, and it is
//! deliberately the version a competent engineer writes first. It is not a
//! strawman — it is correct, it is ordered, and `max`/`min` are genuinely
//! cheap because a B-tree knows its own extremes.
//!
//! What it pays is locality. Every `set` walks a tree of heap-allocated nodes,
//! and a price level update touches a node the cache has probably evicted. With
//! 97% of feed traffic landing on this one call, that pointer chase *is* the
//! system's cost.
//!
//! Keeping it around after the fast version exists matters for more than the
//! benchmark: it is the reference the fast implementation is differentially
//! tested against, and a disagreement is a bug in the clever one.

use deep::Price;
use std::collections::BTreeMap;

use crate::levels::Levels;

#[derive(Debug, Default, Clone)]
pub struct BTreeLevels {
    map: BTreeMap<Price, u32>,
}

impl Levels for BTreeLevels {
    #[inline]
    fn set(&mut self, price: Price, size: u32) {
        if size == 0 {
            self.map.remove(&price);
        } else {
            self.map.insert(price, size);
        }
    }

    #[inline]
    fn max(&self) -> Option<(Price, u32)> {
        self.map.iter().next_back().map(|(&p, &s)| (p, s))
    }

    #[inline]
    fn min(&self) -> Option<(Price, u32)> {
        self.map.iter().next().map(|(&p, &s)| (p, s))
    }

    #[inline]
    fn len(&self) -> usize {
        self.map.len()
    }

    fn total_size(&self) -> u64 {
        self.map.values().map(|&s| s as u64).sum()
    }

    fn for_each(&self, mut f: impl FnMut(Price, u32)) {
        for (&p, &s) in &self.map {
            f(p, s);
        }
    }

    fn clear(&mut self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(dollars: i64, cents: i64) -> Price {
        Price::from_parts(dollars, cents * 100)
    }

    #[test]
    fn set_and_find_extremes() {
        let mut l = BTreeLevels::default();
        assert_eq!(l.max(), None);
        assert_eq!(l.min(), None);
        l.set(px(10, 0), 100);
        l.set(px(10, 5), 200);
        l.set(px(9, 95), 300);
        assert_eq!(l.max(), Some((px(10, 5), 200)));
        assert_eq!(l.min(), Some((px(9, 95), 300)));
        assert_eq!(l.len(), 3);
        assert_eq!(l.total_size(), 600);
    }

    /// Zero size deletes. Storing a zero-size level would leave the touch
    /// pointing at a level with nothing on it.
    #[test]
    fn zero_size_removes_the_level() {
        let mut l = BTreeLevels::default();
        l.set(px(10, 0), 100);
        l.set(px(10, 5), 200);
        l.set(px(10, 5), 0);
        assert_eq!(l.len(), 1);
        assert_eq!(l.max(), Some((px(10, 0), 100)));
    }

    #[test]
    fn deleting_the_only_level_empties_the_side() {
        let mut l = BTreeLevels::default();
        l.set(px(10, 0), 100);
        l.set(px(10, 0), 0);
        assert!(l.is_empty());
        assert_eq!(l.max(), None);
    }

    /// Deleting a price that was never present must be a no-op, not a panic:
    /// a receiver that joins a live feed mid-session sees deletes for levels it
    /// never saw created.
    #[test]
    fn deleting_an_absent_level_is_harmless() {
        let mut l = BTreeLevels::default();
        l.set(px(10, 0), 100);
        l.set(px(42, 0), 0);
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn resize_in_place_keeps_one_level() {
        let mut l = BTreeLevels::default();
        l.set(px(10, 0), 100);
        l.set(px(10, 0), 700);
        assert_eq!(l.len(), 1);
        assert_eq!(l.max(), Some((px(10, 0), 700)));
    }

    #[test]
    fn clear_empties_everything() {
        let mut l = BTreeLevels::default();
        l.set(px(10, 0), 100);
        l.set(px(11, 0), 100);
        l.clear();
        assert!(l.is_empty());
        assert_eq!(l.total_size(), 0);
    }
}
