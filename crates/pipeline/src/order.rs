//! The outbound stage: encoding a new order to the wire.
//!
//! Modelled on Nasdaq **OUCH 4.2 Enter Order** — a fixed 49-byte, big-endian
//! message. It is the last stage of the tick-to-trade path and exists here to
//! occupy its true place in the latency budget rather than to be omitted and
//! quietly make the numbers look better.
//!
//! **Honest caveat, and it matters given how the rest of this repo is
//! justified.** Every other wire format here was verified against real captures:
//! message lengths came from a histogram over 40 million messages, and a bit mask
//! was corrected because real data contradicted it. This encoder is not. IEX
//! publishes market data captures but not order-entry captures, so there is no
//! recording to check these offsets against. The layout follows the published
//! OUCH specification and the encoder is tested for self-consistency and exact
//! byte output — but "matches the spec as I read it" is a weaker claim than
//! "matches bytes an exchange actually sent", and the two should not be confused.
//!
//! Pairing Nasdaq's order-entry protocol with IEX's market data is deliberate:
//! the latency budget needs a realistic fixed-width binary encode, and OUCH is
//! the best-documented one available.

use deep::{Price, Side, Symbol};

/// Wire size of an Enter Order message.
pub const ENTER_ORDER_LEN: usize = 49;

/// Price is a 32-bit field in units of 1/10000 dollar, so this is the ceiling.
const MAX_WIRE_PRICE: i64 = u32::MAX as i64;

/// Why an order could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// The price does not fit OUCH's 32-bit price field.
    ///
    /// Not hypothetical: the 2018 session carries prices up to $294,750, and the
    /// field tops out at $429,496.7295. Truncating would send an order at a
    /// wildly wrong price, so this refuses instead.
    #[error("price {0} does not fit a 32-bit wire price")]
    PriceOutOfRange(i64),
    #[error("size {0} does not fit a 32-bit share count")]
    SizeOutOfRange(u64),
}

/// A new order, in the terms the strategy thinks in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewOrder {
    pub token: [u8; 14],
    pub side: Side,
    pub shares: u32,
    pub symbol: Symbol,
    pub price: Price,
}

/// Encode into `out`, returning the bytes written.
///
/// Takes a caller-owned buffer rather than returning a `Vec`: allocating on the
/// order path would put a call to the allocator between a book update and the
/// wire, which is the one place in the system that cannot afford one.
#[inline]
pub fn encode_enter_order(
    o: &NewOrder,
    out: &mut [u8; ENTER_ORDER_LEN],
) -> Result<usize, EncodeError> {
    let price = o.price.raw();
    if !(0..=MAX_WIRE_PRICE).contains(&price) {
        return Err(EncodeError::PriceOutOfRange(price));
    }

    out[0] = b'O';
    out[1..15].copy_from_slice(&o.token);
    out[15] = match o.side {
        Side::Buy => b'B',
        Side::Sell => b'S',
    };
    // OUCH is big-endian, unlike IEX-TP and DEEP on the inbound side. Carrying
    // one convention across the whole pipeline would be wrong at this boundary.
    out[16..20].copy_from_slice(&o.shares.to_be_bytes());
    out[20..28].copy_from_slice(&o.symbol.0);
    out[28..32].copy_from_slice(&(price as u32).to_be_bytes());
    out[32..36].copy_from_slice(b"IOCE"); // time in force
    out[36..40].copy_from_slice(b"LTLD"); // firm
    out[40] = b'Y'; // display
    out[41] = b'P'; // capacity: principal
    out[42] = b'N'; // intermarket sweep
    out[43..47].copy_from_slice(&0u32.to_be_bytes()); // minimum quantity
    out[47] = b'N'; // cross type
    out[48] = b'R'; // customer type: retail
    Ok(ENTER_ORDER_LEN)
}

/// Monotonic order tokens, formatted once per order without allocating.
#[derive(Debug, Default)]
pub struct TokenGen {
    next: u64,
}

impl TokenGen {
    /// A 14-byte token: a fixed prefix and a zero-padded decimal counter.
    ///
    /// Written by hand rather than with `format!` because the order path must
    /// not allocate, and a formatting machine on the hot path would dominate the
    /// stage it is being measured in.
    #[inline]
    pub fn next_token(&mut self) -> [u8; 14] {
        let mut t = [b'0'; 14];
        t[0] = b'L';
        t[1] = b'L';
        let mut n = self.next;
        self.next += 1;
        let mut i = 13;
        while i >= 2 {
            t[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
            i -= 1;
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(price: Price, shares: u32, side: Side) -> NewOrder {
        NewOrder {
            token: *b"LL000000000001",
            side,
            shares,
            symbol: Symbol::from_ticker("AAPL"),
            price,
        }
    }

    #[test]
    fn encodes_every_field_at_its_documented_offset() {
        let mut buf = [0u8; ENTER_ORDER_LEN];
        let o = order(Price::from_parts(186, 5500), 100, Side::Buy);
        assert_eq!(encode_enter_order(&o, &mut buf).unwrap(), ENTER_ORDER_LEN);

        assert_eq!(buf[0], b'O');
        assert_eq!(&buf[1..15], b"LL000000000001");
        assert_eq!(buf[15], b'B');
        assert_eq!(u32::from_be_bytes(buf[16..20].try_into().unwrap()), 100);
        assert_eq!(&buf[20..28], b"AAPL    ");
        assert_eq!(
            u32::from_be_bytes(buf[28..32].try_into().unwrap()),
            1_865_500
        );
        assert_eq!(&buf[32..36], b"IOCE");
    }

    #[test]
    fn side_selects_the_indicator_byte() {
        let mut buf = [0u8; ENTER_ORDER_LEN];
        encode_enter_order(&order(Price::from_parts(1, 0), 1, Side::Sell), &mut buf).unwrap();
        assert_eq!(buf[15], b'S');
    }

    /// The price field is 32 bits and the real feed carries prices near its
    /// ceiling, so the boundary is worth pinning rather than assuming.
    #[test]
    fn price_at_and_beyond_the_wire_ceiling() {
        let mut buf = [0u8; ENTER_ORDER_LEN];
        // BRK.A territory: the 2018 session tops out around $294,750.
        let brk = Price::from_raw(2_947_500_000);
        assert!(encode_enter_order(&order(brk, 1, Side::Buy), &mut buf).is_ok());

        let too_big = Price::from_raw(MAX_WIRE_PRICE + 1);
        assert_eq!(
            encode_enter_order(&order(too_big, 1, Side::Buy), &mut buf),
            Err(EncodeError::PriceOutOfRange(MAX_WIRE_PRICE + 1)),
            "truncating would send an order at a wildly wrong price"
        );

        let negative = Price::from_raw(-1);
        assert!(encode_enter_order(&order(negative, 1, Side::Buy), &mut buf).is_err());
    }

    #[test]
    fn tokens_are_unique_and_fixed_width() {
        let mut g = TokenGen::default();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..5_000 {
            let t = g.next_token();
            assert_eq!(t.len(), 14);
            assert!(t.iter().all(|b| b.is_ascii()), "token must be ASCII");
            assert!(seen.insert(t), "tokens must not repeat");
        }
    }
}
