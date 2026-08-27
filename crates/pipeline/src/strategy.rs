//! The decision stage. **Deliberately trivial, and deliberately not alpha.**
//!
//! This exists to occupy its real place in the latency budget. A tick-to-trade
//! measurement that ends at the book update is measuring a feed handler, not a
//! trading system, and it flatters itself by leaving out the two stages that
//! actually decide whether an order goes out.
//!
//! So the rule here is fixed, cheap, and stupid on purpose: when the book is
//! stable, two-sided, and the spread is at least two ticks, join the near side.
//! It has no edge and is not meant to. Substituting a real signal changes this
//! file and nothing else, which is the point of measuring it as its own stage.
//!
//! One property does matter: it must be **branch-predictable and allocation
//! free**, because an unpredictable decision stage would show up as jitter in
//! the p99.9 and be indistinguishable from jitter in the parts under study.

use book::Touch;
use deep::{Price, Side, Symbol};

/// A decision. `None` far more often than not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    pub side: Side,
    pub symbol: Symbol,
    pub price: Price,
    pub shares: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Minimum spread, in price units, before quoting is considered.
    pub min_spread: i64,
    pub shares: u32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            // Two cents. The measured median spread is 52 ticks, so this fires
            // often enough to be a real branch rather than dead code.
            min_spread: 200,
            shares: 100,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Strategy {
    pub params: Params,
}

impl Strategy {
    #[must_use]
    pub fn new(params: Params) -> Self {
        Self { params }
    }

    /// Decide whether to quote, from the touch the book just produced.
    ///
    /// Takes a [`Touch`] rather than the book, so the decision stage performs no
    /// lookup of its own. The update already read both sides for its crossed
    /// check; asking the book again would be a second hash probe and two more
    /// reads of level storage on every message.
    #[inline]
    pub fn on_touch(&self, symbol: Symbol, t: Touch) -> Option<Quote> {
        if !t.stable {
            return None;
        }
        let (b, _) = t.bid?;
        let (a, _) = t.ask?;
        let spread = a.raw() - b.raw();
        if spread < self.params.min_spread {
            return None;
        }
        Some(Quote {
            side: Side::Buy,
            symbol,
            price: b,
            shares: self.params.shares,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use book::{BTreeLevels, Book};
    use deep::PriceLevelUpdate;

    fn upd(side: Side, price: Price, size: u32, complete: bool) -> PriceLevelUpdate {
        PriceLevelUpdate {
            side,
            flags: if complete { 0x01 } else { 0x00 },
            timestamp: 0,
            symbol: Symbol::from_ticker("AAPL"),
            size,
            price,
        }
    }

    fn book_with(updates: &[PriceLevelUpdate]) -> Book<BTreeLevels> {
        let mut b = Book::new();
        for u in updates {
            b.apply_price_level(u);
        }
        b
    }

    fn decide(b: &Book<BTreeLevels>, s: &Strategy) -> Option<Quote> {
        let sym = Symbol::from_ticker("AAPL");
        let sb = b.get(sym)?;
        s.on_touch(
            sym,
            book::Touch {
                bid: sb.best_bid(),
                ask: sb.best_ask(),
                stable: sb.is_stable(),
            },
        )
    }

    #[test]
    fn quotes_when_the_spread_is_wide_and_the_book_is_stable() {
        let b = book_with(&[
            upd(Side::Buy, Price::from_parts(100, 0), 500, true),
            upd(Side::Sell, Price::from_parts(100, 500), 500, true),
        ]);
        let q = decide(&b, &Strategy::default()).expect("should quote");
        assert_eq!(q.side, Side::Buy);
        assert_eq!(q.price, Price::from_parts(100, 0));
        assert_eq!(q.shares, 100);
    }

    #[test]
    fn does_not_quote_inside_a_tight_spread() {
        let b = book_with(&[
            upd(Side::Buy, Price::from_parts(100, 0), 500, true),
            upd(Side::Sell, Price::from_parts(100, 100), 500, true),
        ]);
        assert_eq!(decide(&b, &Strategy::default()), None);
    }

    /// Quoting off a half-applied book is the failure the event-complete flag
    /// exists to prevent, so the strategy must respect it too.
    #[test]
    fn does_not_quote_while_the_feed_is_mid_event() {
        let b = book_with(&[
            upd(Side::Buy, Price::from_parts(100, 0), 500, true),
            upd(Side::Sell, Price::from_parts(100, 500), 500, false),
        ]);
        assert_eq!(decide(&b, &Strategy::default()), None);
    }

    #[test]
    fn does_not_quote_a_one_sided_book() {
        let b = book_with(&[upd(Side::Buy, Price::from_parts(100, 0), 500, true)]);
        assert_eq!(decide(&b, &Strategy::default()), None);
    }
}
