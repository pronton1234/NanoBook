//! Fixed-point prices. No floating point touches a price, ever.
//!
//! DEEP quotes prices as a signed 64-bit integer in units of 1/10000 of a
//! dollar, so `$99.99` is `999_900`. That representation is exact, and the only
//! way to lose that is to convert it to `f64` "just for display" and then let
//! the converted value leak back into arithmetic. `0.1 + 0.2 != 0.3` is a joke
//! until it is a book that disagrees with the exchange by a tick.
//!
//! So [`Price`] deliberately offers **no** conversion to `f64` and formats
//! itself by integer division. Comparison and ordering come from the integer,
//! which means price levels sort exactly and a `BTreeMap<Price, _>` or an array
//! indexed by tick both behave.

use std::fmt;

/// Price units per whole dollar. DEEP uses four implied decimal places.
pub const UNITS_PER_DOLLAR: i64 = 10_000;

/// A price in units of 1/10000 dollar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Price(pub i64);

impl Price {
    /// The price a resting order is removed at: DEEP signals deletion with a
    /// size of zero, and the price field is then not meaningful.
    pub const ZERO: Price = Price(0);

    #[inline(always)]
    #[must_use]
    pub const fn from_raw(units: i64) -> Self {
        Price(units)
    }

    /// Raw units of 1/10000 dollar.
    #[inline(always)]
    #[must_use]
    pub const fn raw(self) -> i64 {
        self.0
    }

    /// Construct from whole dollars and ten-thousandths, for tests and literals.
    #[inline]
    #[must_use]
    pub const fn from_parts(dollars: i64, ten_thousandths: i64) -> Self {
        Price(dollars * UNITS_PER_DOLLAR + ten_thousandths)
    }

    /// Whole dollars, truncated toward zero.
    #[inline]
    #[must_use]
    pub const fn dollars(self) -> i64 {
        self.0 / UNITS_PER_DOLLAR
    }

    /// Index into a flat array of price levels, given a tick size.
    ///
    /// This is what makes the R1 book fast: US equity prices above $1.00 move on
    /// a one-cent grid, so a price is a small integer offset rather than a key
    /// to be searched for. Returns `None` when the price is not on the grid,
    /// which sub-dollar and sub-penny prices are not.
    #[inline]
    #[must_use]
    pub const fn tick_index(self, tick_units: i64) -> Option<usize> {
        if tick_units <= 0 || self.0 < 0 || self.0 % tick_units != 0 {
            return None;
        }
        Some((self.0 / tick_units) as usize)
    }

    #[inline]
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Price {
    /// Render exactly, by integer division. Trailing zeros beyond two decimals
    /// are trimmed, because `$99.99` reads better than `$99.9900` and both are
    /// the same integer.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let neg = self.0 < 0;
        let abs = self.0.unsigned_abs();
        let whole = abs / UNITS_PER_DOLLAR as u64;
        let frac = abs % UNITS_PER_DOLLAR as u64;
        if neg {
            f.write_str("-")?;
        }
        if frac % 100 == 0 {
            write!(f, "{whole}.{:02}", frac / 100)
        } else {
            write!(f, "{whole}.{frac:04}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_exactly() {
        assert_eq!(Price::from_parts(99, 9900).to_string(), "99.99");
        assert_eq!(Price::from_parts(0, 1).to_string(), "0.0001");
        assert_eq!(Price::from_raw(1_000_000).to_string(), "100.00");
        assert_eq!(Price::from_raw(-25_000).to_string(), "-2.50");
        assert_eq!(Price::ZERO.to_string(), "0.00");
    }

    /// The whole point of the type: the classic binary-float error must not be
    /// representable here.
    #[test]
    fn tenths_add_exactly() {
        let a = Price::from_parts(0, 1_000); // 0.10
        let b = Price::from_parts(0, 2_000); // 0.20
        assert_eq!(
            Price::from_raw(a.raw() + b.raw()),
            Price::from_parts(0, 3_000)
        );
    }

    #[test]
    fn orders_by_integer() {
        let mut v = [
            Price::from_parts(10, 1),
            Price::from_parts(9, 9999),
            Price::from_parts(10, 0),
        ];
        v.sort();
        assert_eq!(v[0], Price::from_parts(9, 9999));
        assert_eq!(v[2], Price::from_parts(10, 1));
    }

    /// A penny grid maps prices to array slots; anything off-grid must say so
    /// rather than silently rounding into the wrong bucket.
    #[test]
    fn tick_index_only_accepts_on_grid_prices() {
        let penny = UNITS_PER_DOLLAR / 100; // 100 units
        assert_eq!(Price::from_parts(1, 0).tick_index(penny), Some(100));
        assert_eq!(Price::from_parts(99, 9900).tick_index(penny), Some(9999));
        assert_eq!(Price::from_parts(0, 1).tick_index(penny), None, "sub-penny");
        assert_eq!(Price::from_raw(-100).tick_index(penny), None, "negative");
        assert_eq!(Price::from_parts(1, 0).tick_index(0), None, "zero tick");
    }
}
