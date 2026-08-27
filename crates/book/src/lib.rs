//! Aggregated-depth order book, with the price-level container left generic.
//!
//! DEEP publishes total displayed size per price level rather than individual
//! orders, so a side of a book is a price-to-size map and there is no order-id
//! graph to maintain. That makes the container the whole engineering question,
//! which is why [`Levels`] is a trait and why every implementation of it is a
//! rung on the ladder rather than a refactor.
//!
//! ## Why the crossed check is gated on `event_complete`
//!
//! IEX may split one logical book event across several messages — lifting an
//! offer and posting a new one, say. Between those messages the published book
//! is legitimately crossed, and bit 0 of the price level update's event flags is
//! how the feed says "I am mid-event, do not look yet".
//!
//! So a validator that asserts *never crossed* on every message fires constantly
//! on a healthy feed, and the natural reaction — relaxing the assert — throws
//! away the one invariant that catches real book corruption. Gating on the flag
//! keeps the invariant strict exactly where it is meaningful.
//!
//! This is also the flag whose mask was wrong in the decoder (bit 7 rather than
//! bit 0), which no synthetic test caught. A crossed-book count is the second
//! line of defence against that class of error: had the mask still been wrong,
//! every message would read as mid-event and this check would silently never
//! run.

pub mod btree;
pub mod inline;
pub mod levels;
pub mod sorted;

pub use btree::BTreeLevels;
pub use inline::InlineLevels;
pub use levels::Levels;
pub use sorted::SortedLevels;

use deep::{Message, Price, PriceLevelUpdate, Side, Symbol};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// One symbol's two sides.
#[derive(Debug, Default, Clone)]
pub struct SymbolBook<L: Levels> {
    pub bids: L,
    pub asks: L,
    /// False while the feed is partway through an atomic book event.
    complete: bool,
}

impl<L: Levels> SymbolBook<L> {
    /// Highest bid.
    #[inline]
    pub fn best_bid(&self) -> Option<(Price, u32)> {
        self.bids.max()
    }

    /// Lowest ask.
    #[inline]
    pub fn best_ask(&self) -> Option<(Price, u32)> {
        self.asks.min()
    }

    /// Best bid strictly **above** best ask.
    ///
    /// This is the violation: it means someone would cross themselves, and on a
    /// correctly maintained book it cannot happen at an event boundary.
    ///
    /// Only meaningful when [`Self::is_stable`] — see the module docs.
    #[inline]
    pub fn is_crossed(&self) -> bool {
        match (self.best_bid(), self.best_ask()) {
            (Some((b, _)), Some((a, _))) => b > a,
            _ => false,
        }
    }

    /// Best bid exactly **equal** to best ask.
    ///
    /// Deliberately not folded into [`Self::is_crossed`]. A locked book is a
    /// weaker signal than a crossed one -- it is unusual but reachable on a
    /// single venue with non-displayed or discretionary order types, whereas a
    /// crossed book at an event boundary is a bug somewhere. Counting them
    /// together would let a real crossing hide inside a pile of benign locks.
    #[inline]
    pub fn is_locked(&self) -> bool {
        match (self.best_bid(), self.best_ask()) {
            (Some((b, _)), Some((a, _))) => b == a,
            _ => false,
        }
    }

    /// The feed is not partway through a multi-message book event.
    #[inline]
    pub fn is_stable(&self) -> bool {
        self.complete
    }

    /// Difference between the touch prices, when both sides are populated.
    #[inline]
    pub fn spread(&self) -> Option<Price> {
        let (b, _) = self.best_bid()?;
        let (a, _) = self.best_ask()?;
        Some(Price::from_raw(a.raw() - b.raw()))
    }

    #[inline]
    fn apply(&mut self, u: &PriceLevelUpdate) {
        match u.side {
            Side::Buy => self.bids.set(u.price, u.size),
            Side::Sell => self.asks.set(u.price, u.size),
        }
        self.complete = u.event_complete();
    }

    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.complete = false;
    }
}

/// Counters the audit reports. All derived, never authoritative.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub updates: u64,
    pub deletes: u64,
    /// Deletes for a price that held no size. Normal when joining mid-session;
    /// a rising count mid-run means levels are being lost somewhere.
    pub deletes_of_absent: u64,
    /// Bid strictly above ask **at an event boundary** — the real violation.
    pub crossed_when_stable: u64,
    /// Crossed while mid-event. Expected, and counted only to show the gating is
    /// doing something.
    pub crossed_mid_event: u64,
    /// Bid exactly equal to ask at an event boundary. Unusual, not a violation.
    pub locked_when_stable: u64,
    pub trades: u64,
    pub traded_shares: u64,
}

/// Hasher for [`Symbol`], which is already eight bytes of entropy-poor ASCII.
///
/// `HashMap`'s default is SipHash, which is a sound default for untrusted keys
/// and pure overhead here: symbols come from an exchange feed, not an attacker,
/// and the lookup happens on every one of the 40 million messages in a session.
/// This is the fibonacci-multiply trick — one multiply and one shift — which
/// spreads the low-entropy ticker bytes across the whole word.
#[derive(Default)]
pub struct SymbolHasher(u64);

impl Hasher for SymbolHasher {
    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        // Symbols are always exactly 8 bytes; the loop is for correctness if
        // some other key type is ever hashed with this.
        for chunk in bytes.chunks(8) {
            let mut buf = [0u8; 8];
            buf[..chunk.len()].copy_from_slice(chunk);
            self.0 = (self.0 ^ u64::from_ne_bytes(buf)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }

    #[inline(always)]
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0 ^ n).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }
}

type SymbolMap<L> = HashMap<Symbol, SymbolBook<L>, BuildHasherDefault<SymbolHasher>>;

/// Every symbol's book.
pub struct Book<L: Levels> {
    symbols: SymbolMap<L>,
    pub stats: Stats,
}

impl<L: Levels> Default for Book<L> {
    fn default() -> Self {
        Self {
            symbols: SymbolMap::default(),
            stats: Stats::default(),
        }
    }
}

impl<L: Levels> Book<L> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-size the symbol table.
    ///
    /// A full IEX session touches ~6,900 symbols. Growing a hash map into that
    /// rehashes every entry several times, and every rehash lands in the middle
    /// of the hot path.
    #[must_use]
    pub fn with_capacity(symbols: usize) -> Self {
        Self {
            symbols: SymbolMap::with_capacity_and_hasher(symbols, Default::default()),
            stats: Stats::default(),
        }
    }

    #[inline]
    #[must_use]
    pub fn get(&self, symbol: Symbol) -> Option<&SymbolBook<L>> {
        self.symbols.get(&symbol)
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Symbol, &SymbolBook<L>)> {
        self.symbols.iter()
    }

    /// Apply one decoded message. Anything that is not book or trade traffic is
    /// ignored rather than rejected.
    #[inline]
    pub fn apply(&mut self, msg: &Message<'_>) {
        match msg {
            Message::PriceLevel(u) => self.apply_price_level(u),
            // A trade break cancels a previously published trade, so it must
            // not add to the traded total.
            Message::Trade(t) if !t.is_break => {
                self.stats.trades += 1;
                self.stats.traded_shares += t.size as u64;
            }
            _ => {}
        }
    }

    /// The hot path: 97% of all feed traffic reaches here.
    #[inline]
    pub fn apply_price_level(&mut self, u: &PriceLevelUpdate) {
        let entry = self.symbols.entry(u.symbol).or_default();
        self.stats.updates += 1;
        if u.size == 0 {
            self.stats.deletes += 1;
            let present = match u.side {
                Side::Buy => entry.bids.len(),
                Side::Sell => entry.asks.len(),
            };
            entry.apply(u);
            let after = match u.side {
                Side::Buy => entry.bids.len(),
                Side::Sell => entry.asks.len(),
            };
            if present == after {
                self.stats.deletes_of_absent += 1;
            }
        } else {
            entry.apply(u);
        }

        if entry.is_crossed() {
            if entry.is_stable() {
                self.stats.crossed_when_stable += 1;
            } else {
                self.stats.crossed_mid_event += 1;
            }
        } else if entry.is_stable() && entry.is_locked() {
            self.stats.locked_when_stable += 1;
        }
    }

    /// Forget everything. The feed's session id changed, so any retained state
    /// belongs to a stream that no longer exists.
    pub fn clear(&mut self) {
        self.symbols.clear();
    }

    /// Total displayed size across every symbol and side.
    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.symbols
            .values()
            .map(|b| b.bids.total_size() + b.asks.total_size())
            .sum()
    }

    /// Populated levels across every symbol and side.
    #[must_use]
    pub fn total_levels(&self) -> usize {
        self.symbols
            .values()
            .map(|b| b.bids.len() + b.asks.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deep::Side;

    fn px(d: i64, c: i64) -> Price {
        Price::from_parts(d, c * 100)
    }

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

    fn book() -> Book<BTreeLevels> {
        Book::new()
    }

    #[test]
    fn builds_a_two_sided_book() {
        let mut b = book();
        b.apply_price_level(&upd(Side::Buy, "AAPL", px(186, 55), 100, true));
        b.apply_price_level(&upd(Side::Sell, "AAPL", px(186, 57), 200, true));
        let s = b.get(Symbol::from_ticker("AAPL")).unwrap();
        assert_eq!(s.best_bid(), Some((px(186, 55), 100)));
        assert_eq!(s.best_ask(), Some((px(186, 57), 200)));
        assert_eq!(s.spread(), Some(Price::from_parts(0, 200)));
        assert!(!s.is_crossed());
    }

    #[test]
    fn best_bid_is_the_highest_and_best_ask_the_lowest() {
        let mut b = book();
        for (p, s) in [(px(10, 0), 1), (px(10, 2), 2), (px(9, 98), 3)] {
            b.apply_price_level(&upd(Side::Buy, "X", p, s, true));
            b.apply_price_level(&upd(Side::Sell, "X", p, s, true));
        }
        let s = b.get(Symbol::from_ticker("X")).unwrap();
        assert_eq!(s.best_bid().unwrap().0, px(10, 2), "bid takes the max");
        assert_eq!(s.best_ask().unwrap().0, px(9, 98), "ask takes the min");
    }

    /// Deleting the touch must expose the next level, not empty the side.
    #[test]
    fn deleting_the_touch_uncovers_the_next_level() {
        let mut b = book();
        b.apply_price_level(&upd(Side::Buy, "X", px(10, 0), 100, true));
        b.apply_price_level(&upd(Side::Buy, "X", px(10, 1), 200, true));
        b.apply_price_level(&upd(Side::Buy, "X", px(10, 1), 0, true));
        let s = b.get(Symbol::from_ticker("X")).unwrap();
        assert_eq!(s.best_bid(), Some((px(10, 0), 100)));
    }

    /// The invariant the module exists to protect, and the reason it is gated.
    #[test]
    fn crossing_counts_only_at_an_event_boundary() {
        let mut b = book();
        // Mid-event: flag clear. A cross here is the feed's business, not a bug.
        b.apply_price_level(&upd(Side::Buy, "X", px(10, 5), 100, false));
        b.apply_price_level(&upd(Side::Sell, "X", px(10, 0), 100, false));
        assert_eq!(b.stats.crossed_mid_event, 1);
        assert_eq!(b.stats.crossed_when_stable, 0);

        // Same cross, but the feed says the event is complete. Now it is real.
        let mut b = book();
        b.apply_price_level(&upd(Side::Buy, "Y", px(10, 5), 100, true));
        b.apply_price_level(&upd(Side::Sell, "Y", px(10, 0), 100, true));
        assert_eq!(b.stats.crossed_when_stable, 1);
    }

    /// A locked book is not a crossed one. Folding them together would let a
    /// real crossing hide inside a pile of benign locks.
    #[test]
    fn locked_is_counted_separately_from_crossed() {
        let mut b = book();
        b.apply_price_level(&upd(Side::Buy, "X", px(10, 0), 100, true));
        b.apply_price_level(&upd(Side::Sell, "X", px(10, 0), 100, true));
        let s = b.get(Symbol::from_ticker("X")).unwrap();
        assert!(s.is_locked());
        assert!(!s.is_crossed(), "equal prices are locked, not crossed");
        assert_eq!(b.stats.locked_when_stable, 1);
        assert_eq!(b.stats.crossed_when_stable, 0);
    }

    /// Joining a live feed mid-session means seeing deletes for levels that were
    /// created before we started listening.
    #[test]
    fn delete_of_an_unknown_level_is_counted_not_fatal() {
        let mut b = book();
        b.apply_price_level(&upd(Side::Buy, "X", px(10, 0), 0, true));
        assert_eq!(b.stats.deletes, 1);
        assert_eq!(b.stats.deletes_of_absent, 1);
        assert!(b.get(Symbol::from_ticker("X")).unwrap().bids.is_empty());
    }

    #[test]
    fn symbols_do_not_leak_into_each_other() {
        let mut b = book();
        b.apply_price_level(&upd(Side::Buy, "AAPL", px(186, 55), 100, true));
        b.apply_price_level(&upd(Side::Buy, "MSFT", px(101, 0), 300, true));
        assert_eq!(b.len(), 2);
        assert_eq!(
            b.get(Symbol::from_ticker("AAPL")).unwrap().best_bid(),
            Some((px(186, 55), 100))
        );
        assert_eq!(
            b.get(Symbol::from_ticker("MSFT")).unwrap().best_bid(),
            Some((px(101, 0), 300))
        );
    }

    #[test]
    fn totals_track_the_levels() {
        let mut b = book();
        b.apply_price_level(&upd(Side::Buy, "X", px(10, 0), 100, true));
        b.apply_price_level(&upd(Side::Sell, "X", px(10, 1), 250, true));
        assert_eq!(b.total_size(), 350);
        assert_eq!(b.total_levels(), 2);
        b.apply_price_level(&upd(Side::Buy, "X", px(10, 0), 0, true));
        assert_eq!(b.total_size(), 250);
        assert_eq!(b.total_levels(), 1);
    }

    /// Low-entropy ASCII keys must still spread. A hasher that collides every
    /// four-letter ticker would turn the symbol map into a linked list.
    #[test]
    fn symbol_hasher_spreads_similar_tickers() {
        use std::collections::HashSet;
        let tickers = [
            "AAPL", "AAPM", "AAPN", "BAPL", "CAPL", "SPY", "SPZ", "QQQ", "IWM", "A", "B", "C",
        ];
        let hashes: HashSet<u64> = tickers
            .iter()
            .map(|t| {
                let mut h = SymbolHasher::default();
                h.write_u64(Symbol::from_ticker(t).as_u64());
                h.finish()
            })
            .collect();
        assert_eq!(
            hashes.len(),
            tickers.len(),
            "no collisions among similar tickers"
        );
    }
}
