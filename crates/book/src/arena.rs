//! R1c: level storage in one contiguous arena. **This loses. Kept as evidence.**
//!
//! The hypothesis: with `SortedLevels` every symbol's two sides are separately
//! allocated `Vec`s, so ~12,000 tiny allocations scatter across the heap and
//! reading a price is a dependent load into unrelated memory. An arena would
//! keep the per-symbol record tiny — so the hash table stays dense, unlike
//! [`InlineLevels`](crate::InlineLevels), which lost by doubling it — while every
//! level lives in one flat allocation with a symbol's two sides adjacent.
//!
//! Measured over 5M real updates, it is **twice as slow** as the `Vec`s it was
//! meant to replace, and slower than the `BTreeMap` baseline:
//!
//! ```text
//!   sorted SoA (R1)    49.6 ns/upd   1.20x vs BTreeMap
//!   arena, SLOT = 8   103.9 ns/upd   0.57x
//! ```
//!
//! Sweeping the slot size shows part of the reason and rules out the rest:
//!
//! ```text
//!   SLOT = 2   79.5 ns/upd     SLOT = 4   70.2 ns/upd     SLOT = 8  103.9 ns/upd
//! ```
//!
//! Footprint is real: eight slots per side to hold a median of two levels is
//! ~75% padding, inflating the live ~400KB of levels into a ~2.3MB working set
//! and evicting exactly the hot symbols that were staying resident. Halving the
//! slot recovers a third of the loss. Halving it again does not, because at
//! `p50 = 2` a two-slot arena spills constantly and the spill is a second
//! indirection on top of the first.
//!
//! But even at its best slot size the arena is 42% slower than a plain sorted
//! `Vec`, so footprint is not the whole story. The rest is that **the arena
//! de-localised what the `HashMap` had already co-located**. In the `SortedLevels`
//! book, one hash entry holds both sides' `Vec` headers *and* the event-complete
//! flag in a single 64-byte line, so an update touches that one line plus each
//! side's data. Here a symbol's state is spread across four independent arrays —
//! `meta`, `prices`, `sizes`, `complete` — each indexed separately and each in a
//! different line, so the update touches more distinct lines than the design it
//! was replacing.
//!
//! The lesson worth keeping: contiguity is not locality. Laying data out in one
//! flat array makes it contiguous *in the abstract*, and what a cache actually
//! rewards is the fields used together being in the same line. A `HashMap` entry
//! is already an arena of one record, and it had that property for free.

use deep::{Price, PriceLevelUpdate, Side, Symbol};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;

use crate::{Stats, SymbolHasher};

/// Levels held per side before spilling. Eight prices are one cache line.
pub const SLOT: usize = 8;

/// Marks a side whose levels are still in the arena rather than spilled.
const NO_SPILL: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
struct SideMeta {
    len: u16,
    /// Index into `spills`, or [`NO_SPILL`].
    spill: u32,
}

impl Default for SideMeta {
    fn default() -> Self {
        Self {
            len: 0,
            spill: NO_SPILL,
        }
    }
}

/// A side too deep for its arena slot.
#[derive(Debug, Default, Clone)]
struct Spill {
    prices: Vec<Price>,
    sizes: Vec<u32>,
}

/// Aggregated-depth book with arena-allocated levels.
pub struct ArenaBook {
    /// Symbol to symbol index. Sides are `2i` (bids) and `2i + 1` (asks).
    index: HashMap<Symbol, u32, BuildHasherDefault<SymbolHasher>>,
    meta: Vec<SideMeta>,
    /// Stride `SLOT` per side, so side `s` owns `[s * SLOT, s * SLOT + SLOT)`.
    prices: Vec<Price>,
    sizes: Vec<u32>,
    spills: Vec<Spill>,
    /// Per symbol: the feed is not mid-event.
    complete: Vec<bool>,
    pub stats: Stats,
}

impl Default for ArenaBook {
    fn default() -> Self {
        Self::with_capacity(1024)
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

impl ArenaBook {
    #[must_use]
    pub fn with_capacity(symbols: usize) -> Self {
        Self {
            index: HashMap::with_capacity_and_hasher(symbols, Default::default()),
            meta: Vec::with_capacity(symbols * 2),
            prices: Vec::with_capacity(symbols * 2 * SLOT),
            sizes: Vec::with_capacity(symbols * 2 * SLOT),
            spills: Vec::new(),
            complete: Vec::with_capacity(symbols),
            stats: Stats::default(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Symbol index, creating the symbol's two side slots on first sight.
    #[inline]
    fn symbol_index(&mut self, symbol: Symbol) -> u32 {
        let next = self.complete.len() as u32;
        match self.index.entry(symbol) {
            std::collections::hash_map::Entry::Occupied(e) => *e.get(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(next);
                self.meta.push(SideMeta::default());
                self.meta.push(SideMeta::default());
                self.prices
                    .resize(self.prices.len() + 2 * SLOT, Price::ZERO);
                self.sizes.resize(self.sizes.len() + 2 * SLOT, 0);
                self.complete.push(false);
                next
            }
        }
    }

    #[inline(always)]
    fn slot(&self, side: usize) -> (&[Price], &[u32]) {
        let m = self.meta[side];
        if m.spill == NO_SPILL {
            let base = side * SLOT;
            let n = m.len as usize;
            (&self.prices[base..base + n], &self.sizes[base..base + n])
        } else {
            let s = &self.spills[m.spill as usize];
            (&s.prices, &s.sizes)
        }
    }

    #[inline]
    fn best(&self, side: usize, want_max: bool) -> Option<(Price, u32)> {
        let (p, s) = self.slot(side);
        if p.is_empty() {
            return None;
        }
        let i = if want_max { p.len() - 1 } else { 0 };
        Some((p[i], s[i]))
    }

    /// Highest bid for a symbol.
    #[must_use]
    pub fn best_bid(&self, symbol: Symbol) -> Option<(Price, u32)> {
        self.best(*self.index.get(&symbol)? as usize * 2, true)
    }

    /// Lowest ask for a symbol.
    #[must_use]
    pub fn best_ask(&self, symbol: Symbol) -> Option<(Price, u32)> {
        self.best(*self.index.get(&symbol)? as usize * 2 + 1, false)
    }

    /// Move a full arena slot to the spill table.
    #[cold]
    #[inline(never)]
    fn spill_side(&mut self, side: usize) {
        let base = side * SLOT;
        let n = self.meta[side].len as usize;
        let mut s = Spill {
            prices: Vec::with_capacity(SLOT * 4),
            sizes: Vec::with_capacity(SLOT * 4),
        };
        s.prices.extend_from_slice(&self.prices[base..base + n]);
        s.sizes.extend_from_slice(&self.sizes[base..base + n]);
        self.spills.push(s);
        self.meta[side].spill = (self.spills.len() - 1) as u32;
    }

    /// Set a level on one side. Returns true if the level count changed.
    #[inline]
    fn set(&mut self, side: usize, price: Price, size: u32) -> bool {
        let m = self.meta[side];
        if m.spill == NO_SPILL {
            let base = side * SLOT;
            let n = m.len as usize;
            match find(&self.prices[base..base + n], price) {
                Ok(i) => {
                    if size == 0 {
                        self.prices.copy_within(base + i + 1..base + n, base + i);
                        self.sizes.copy_within(base + i + 1..base + n, base + i);
                        self.meta[side].len -= 1;
                        return true;
                    }
                    self.sizes[base + i] = size;
                    return false;
                }
                Err(i) => {
                    if size == 0 {
                        return false;
                    }
                    if n < SLOT {
                        self.prices.copy_within(base + i..base + n, base + i + 1);
                        self.sizes.copy_within(base + i..base + n, base + i + 1);
                        self.prices[base + i] = price;
                        self.sizes[base + i] = size;
                        self.meta[side].len += 1;
                        return true;
                    }
                }
            }
            self.spill_side(side);
        }

        let s = &mut self.spills[self.meta[side].spill as usize];
        match find(&s.prices, price) {
            Ok(i) => {
                if size == 0 {
                    s.prices.remove(i);
                    s.sizes.remove(i);
                    self.meta[side].len -= 1;
                    true
                } else {
                    s.sizes[i] = size;
                    false
                }
            }
            Err(i) => {
                if size == 0 {
                    false
                } else {
                    s.prices.insert(i, price);
                    s.sizes.insert(i, size);
                    self.meta[side].len += 1;
                    true
                }
            }
        }
    }

    /// The hot path.
    #[inline]
    pub fn apply_price_level(&mut self, u: &PriceLevelUpdate) {
        let sym = self.symbol_index(u.symbol) as usize;
        let side = sym * 2 + usize::from(u.side == Side::Sell);

        self.stats.updates += 1;
        let changed = self.set(side, u.price, u.size);
        if u.size == 0 {
            self.stats.deletes += 1;
            if !changed {
                self.stats.deletes_of_absent += 1;
            }
        }
        self.complete[sym] = u.event_complete();

        // Read each touch once, as the non-arena book now does.
        let bid = self.best(sym * 2, true);
        let ask = self.best(sym * 2 + 1, false);
        if let (Some((b, _)), Some((a, _))) = (bid, ask) {
            match b.cmp(&a) {
                Ordering::Greater => {
                    if self.complete[sym] {
                        self.stats.crossed_when_stable += 1;
                    } else {
                        self.stats.crossed_mid_event += 1;
                    }
                }
                Ordering::Equal if self.complete[sym] => self.stats.locked_when_stable += 1,
                _ => {}
            }
        }
    }

    #[must_use]
    pub fn total_levels(&self) -> usize {
        self.meta.iter().map(|m| m.len as usize).sum()
    }

    #[must_use]
    pub fn total_size(&self) -> u64 {
        (0..self.meta.len())
            .map(|s| self.slot(s).1.iter().map(|&v| v as u64).sum::<u64>())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BTreeLevels, Book};

    fn upd(side: Side, sym: &str, price: Price, size: u32, complete: bool) -> PriceLevelUpdate {
        PriceLevelUpdate {
            side,
            flags: if complete { 0x01 } else { 0x00 },
            timestamp: 0,
            symbol: Symbol::from_ticker(sym),
            size,
            price,
        }
    }

    #[test]
    fn builds_a_two_sided_book() {
        let mut b = ArenaBook::with_capacity(8);
        b.apply_price_level(&upd(
            Side::Buy,
            "AAPL",
            Price::from_parts(186, 5500),
            100,
            true,
        ));
        b.apply_price_level(&upd(
            Side::Sell,
            "AAPL",
            Price::from_parts(186, 5700),
            200,
            true,
        ));
        let s = Symbol::from_ticker("AAPL");
        assert_eq!(b.best_bid(s), Some((Price::from_parts(186, 5500), 100)));
        assert_eq!(b.best_ask(s), Some((Price::from_parts(186, 5700), 200)));
    }

    /// The arena's own hazard: side slots are adjacent, so an off-by-one in the
    /// stride writes one symbol's level into its neighbour's book.
    #[test]
    fn adjacent_sides_and_symbols_do_not_bleed() {
        let mut b = ArenaBook::with_capacity(8);
        for (i, t) in ["AAA", "BBB", "CCC", "DDD"].iter().enumerate() {
            for j in 0..SLOT {
                b.apply_price_level(&upd(
                    Side::Buy,
                    t,
                    Price::from_parts(100 + i as i64, j as i64 * 100),
                    (i * 10 + j) as u32 + 1,
                    true,
                ));
                b.apply_price_level(&upd(
                    Side::Sell,
                    t,
                    Price::from_parts(200 + i as i64, j as i64 * 100),
                    (i * 10 + j) as u32 + 1,
                    true,
                ));
            }
        }
        for (i, t) in ["AAA", "BBB", "CCC", "DDD"].iter().enumerate() {
            let s = Symbol::from_ticker(t);
            assert_eq!(
                b.best_bid(s).unwrap().0,
                Price::from_parts(100 + i as i64, (SLOT as i64 - 1) * 100),
                "{t} bids"
            );
            assert_eq!(
                b.best_ask(s).unwrap().0,
                Price::from_parts(200 + i as i64, 0),
                "{t} asks"
            );
        }
        assert_eq!(b.total_levels(), 4 * 2 * SLOT);
    }

    /// Differential test against the reference book, driven across the spill
    /// boundary on many symbols at once so stride errors have room to show.
    #[test]
    fn agrees_with_the_reference_book() {
        let mut arena = ArenaBook::with_capacity(16);
        let mut reference: Book<BTreeLevels> = Book::new();
        let tickers = ["AAA", "BBB", "CCC", "DDD", "EEE", "FFF"];
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for step in 0..80_000 {
            let r = next();
            let t = tickers[(r % tickers.len() as u64) as usize];
            let side = if (r >> 8) & 1 == 0 {
                Side::Buy
            } else {
                Side::Sell
            };
            // 20 prices against SLOT = 8 forces repeated spilling and draining.
            let price = Price::from_parts(100, ((r >> 16) % 20) as i64 * 100);
            let size = if r % 10 < 4 {
                0
            } else {
                (r >> 32) as u32 % 5_000 + 1
            };
            let u = upd(side, t, price, size, r % 3 != 0);
            arena.apply_price_level(&u);
            reference.apply_price_level(&u);

            let sym = Symbol::from_ticker(t);
            let rb = reference.get(sym).unwrap();
            assert_eq!(
                arena.best_bid(sym),
                rb.best_bid(),
                "bid diverged at step {step} on {t}"
            );
            assert_eq!(
                arena.best_ask(sym),
                rb.best_ask(),
                "ask diverged at step {step} on {t}"
            );
        }
        assert_eq!(arena.total_levels(), reference.total_levels());
        assert_eq!(arena.total_size(), reference.total_size());
        assert_eq!(arena.stats.updates, reference.stats.updates);
        assert_eq!(arena.stats.deletes, reference.stats.deletes);
        assert_eq!(
            arena.stats.deletes_of_absent,
            reference.stats.deletes_of_absent
        );
        assert_eq!(
            arena.stats.crossed_when_stable,
            reference.stats.crossed_when_stable
        );
        assert_eq!(
            arena.stats.locked_when_stable,
            reference.stats.locked_when_stable
        );
    }
}
