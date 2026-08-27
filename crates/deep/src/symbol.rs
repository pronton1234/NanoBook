//! Eight-byte, space-padded ticker symbols.
//!
//! DEEP carries the symbol inline as a fixed 8-byte field padded with spaces:
//! `AAPL` is `b"AAPL    "`. Keeping it as the raw array rather than a `&str` or
//! `String` matters for the hot path — 97% of feed traffic is price-level
//! updates, and allocating or validating UTF-8 for each one would dominate the
//! parse.
//!
//! The array is `Copy`, 8 bytes, and compares as a single `u64` load, which
//! makes it a cheap hash key for the per-symbol book.

use std::fmt;

/// A fixed-width, space-padded ticker.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Symbol(pub [u8; 8]);

impl Symbol {
    #[inline(always)]
    #[must_use]
    pub const fn from_bytes(b: [u8; 8]) -> Self {
        Symbol(b)
    }

    /// Interpret the eight bytes as one integer.
    ///
    /// Symbol equality is the most common comparison in the whole pipeline, and
    /// as a `u64` it is a single instruction rather than a memcmp.
    #[inline(always)]
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        u64::from_ne_bytes(self.0)
    }

    /// The ticker without its padding.
    #[inline]
    #[must_use]
    pub fn trimmed(&self) -> &[u8] {
        let mut end = self.0.len();
        while end > 0 && self.0[end - 1] == b' ' {
            end -= 1;
        }
        &self.0[..end]
    }

    /// The ticker as text, if it is ASCII. Not on the hot path.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.trimmed()).ok()
    }

    /// Build from a ticker for tests and lookups. Longer than 8 bytes truncates,
    /// which matches what the wire format can represent.
    #[must_use]
    pub fn from_ticker(s: &str) -> Self {
        let mut b = [b' '; 8];
        for (dst, src) in b.iter_mut().zip(s.as_bytes()) {
            *dst = *src;
        }
        Symbol(b)
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_str() {
            Some(s) => f.write_str(s),
            None => write!(f, "{:02x?}", self.0),
        }
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Symbol({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_padding() {
        let s = Symbol::from_bytes(*b"AAPL    ");
        assert_eq!(s.trimmed(), b"AAPL");
        assert_eq!(s.as_str(), Some("AAPL"));
        assert_eq!(s.to_string(), "AAPL");
    }

    #[test]
    fn round_trips_through_ticker() {
        for t in ["A", "AAPL", "GOOGL", "ZVZZT"] {
            assert_eq!(Symbol::from_ticker(t).as_str(), Some(t));
        }
    }

    #[test]
    fn full_width_symbol_has_no_padding() {
        let s = Symbol::from_ticker("ABCDEFGH");
        assert_eq!(s.trimmed(), b"ABCDEFGH");
    }

    /// Equality must agree whichever way it is computed, since the hot path uses
    /// the integer form and diagnostics use the bytes.
    #[test]
    fn u64_equality_matches_byte_equality() {
        let a = Symbol::from_ticker("AAPL");
        let b = Symbol::from_ticker("AAPL");
        let c = Symbol::from_ticker("AAPY");
        assert_eq!(a.as_u64(), b.as_u64());
        assert_ne!(a.as_u64(), c.as_u64());
    }

    /// Non-ASCII must not panic; it is a malformed feed, not a crash.
    #[test]
    fn non_utf8_degrades_gracefully() {
        let s = Symbol::from_bytes([0xff, 0xfe, b' ', b' ', b' ', b' ', b' ', b' ']);
        assert_eq!(s.as_str(), None);
        assert!(!s.to_string().is_empty());
    }
}
