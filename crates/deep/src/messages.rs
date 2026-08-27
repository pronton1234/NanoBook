//! DEEP 1.0 message bodies.
//!
//! Every field offset below was checked against real captures, and every message
//! length was derived empirically rather than transcribed: a histogram of
//! (type, length) over **40,223,554 messages** from a full trading day plus the
//! 2017 fixture shows exactly one length per type, with no exceptions. That is
//! recorded in [`expected_len`], and the parser rejects any message whose length
//! disagrees — a fixed-width format that suddenly is not fixed-width means the
//! framing above it is wrong, and continuing would produce confident nonsense.
//!
//! All multi-byte fields are little-endian, matching IEX-TP and unlike the
//! Ethernet/IP/UDP headers above them.

use crate::{Price, Symbol};

/// Nanoseconds since the Unix epoch, as the feed stamps them.
pub type Timestamp = i64;

#[inline(always)]
fn u32_le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[inline(always)]
fn i64_le(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes([
        b[o],
        b[o + 1],
        b[o + 2],
        b[o + 3],
        b[o + 4],
        b[o + 5],
        b[o + 6],
        b[o + 7],
    ])
}

#[inline(always)]
fn symbol(b: &[u8], o: usize) -> Symbol {
    let mut s = [0u8; 8];
    s.copy_from_slice(&b[o..o + 8]);
    Symbol(s)
}

/// Which side of the book a price level update applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

/// A change to one price level. **97% of all feed traffic.**
///
/// A `size` of zero is a *deletion* of the level, not a level with no shares.
/// Treating it as an update would leave empty levels in the book forever and
/// slowly corrupt the touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceLevelUpdate {
    pub side: Side,
    pub flags: u8,
    pub timestamp: Timestamp,
    pub symbol: Symbol,
    pub size: u32,
    pub price: Price,
}

impl PriceLevelUpdate {
    /// Bit 0 of the event flags: this update completes an atomic group.
    ///
    /// IEX may split one book event across several messages. A consumer that
    /// publishes a top-of-book after every message will briefly show a crossed
    /// or half-updated book; one that waits for this flag never does.
    ///
    /// The mask is `0x01`, and it was originally written as `0x80` here — bit 7
    /// rather than bit 0. Nothing failed: the synthetic tests were built from the
    /// same wrong belief and passed, and the decoder happily reported that 100%
    /// of updates were mid-event. Only real data showed it, because "no book
    /// event ever completes" is impossible in a functioning market.
    #[inline(always)]
    #[must_use]
    pub const fn event_complete(self) -> bool {
        self.flags & 0x01 != 0
    }

    /// A zero size removes the level.
    #[inline(always)]
    #[must_use]
    pub const fn is_delete(self) -> bool {
        self.size == 0
    }
}

/// An execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trade {
    /// True when this is a Trade Break (`0x42`) rather than a report (`0x54`) —
    /// a previously published trade being cancelled.
    pub is_break: bool,
    pub sale_condition: u8,
    pub timestamp: Timestamp,
    pub symbol: Symbol,
    pub size: u32,
    pub price: Price,
    pub trade_id: i64,
}

/// Start and end of the trading session's phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemEvent {
    pub event: u8,
    pub timestamp: Timestamp,
}

/// Per-symbol reference data, published before the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecurityDirectory {
    pub flags: u8,
    pub timestamp: Timestamp,
    pub symbol: Symbol,
    pub round_lot_size: u32,
    pub adjusted_poc_price: Price,
    pub luld_tier: u8,
}

/// Halted, paused, or trading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradingStatus {
    pub status: u8,
    pub timestamp: Timestamp,
    pub symbol: Symbol,
    pub reason: [u8; 4],
}

/// A single-byte status keyed to a symbol: operational halt, short sale price
/// test, or security event. The three share a shape, so they share a struct and
/// are told apart by [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolStatus {
    pub value: u8,
    pub timestamp: Timestamp,
    pub symbol: Symbol,
    /// Extra byte carried only by Short Sale Price Test.
    pub detail: u8,
}

/// Official opening or closing price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfficialPrice {
    pub price_type: u8,
    pub timestamp: Timestamp,
    pub symbol: Symbol,
    pub price: Price,
}

/// Auction state. Kept **borrowed** rather than inlined: at 80 bytes it is by
/// far the largest message and 0.01% of traffic, so inlining it would more than
/// double [`Message`] and slow the 97% case to make the 0.01% case tidier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuctionInformation<'a> {
    body: &'a [u8],
}

impl<'a> AuctionInformation<'a> {
    #[must_use]
    pub fn auction_type(&self) -> u8 {
        self.body[1]
    }
    #[must_use]
    pub fn timestamp(&self) -> Timestamp {
        i64_le(self.body, 2)
    }
    #[must_use]
    pub fn symbol(&self) -> Symbol {
        symbol(self.body, 10)
    }
    #[must_use]
    pub fn paired_shares(&self) -> u32 {
        u32_le(self.body, 18)
    }
    #[must_use]
    pub fn reference_price(&self) -> Price {
        Price::from_raw(i64_le(self.body, 22))
    }
    #[must_use]
    pub fn indicative_clearing_price(&self) -> Price {
        Price::from_raw(i64_le(self.body, 30))
    }
    #[must_use]
    pub fn imbalance_shares(&self) -> u32 {
        u32_le(self.body, 38)
    }
    #[must_use]
    pub fn imbalance_side(&self) -> u8 {
        self.body[42]
    }
    #[must_use]
    pub fn raw(&self) -> &'a [u8] {
        self.body
    }
}

/// One decoded DEEP message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    PriceLevel(PriceLevelUpdate),
    Trade(Trade),
    SystemEvent(SystemEvent),
    SecurityDirectory(SecurityDirectory),
    TradingStatus(TradingStatus),
    OperationalHalt(SymbolStatus),
    ShortSalePriceTest(SymbolStatus),
    SecurityEvent(SymbolStatus),
    OfficialPrice(OfficialPrice),
    Auction(AuctionInformation<'a>),
    /// A type this decoder does not know. Kept rather than dropped so a capture
    /// containing a newer message is still countable.
    Unknown {
        kind: u8,
        body: &'a [u8],
    },
}

impl Message<'_> {
    /// The symbol this message concerns, if any. System events carry none.
    #[must_use]
    pub fn symbol(&self) -> Option<Symbol> {
        Some(match self {
            Message::PriceLevel(m) => m.symbol,
            Message::Trade(m) => m.symbol,
            Message::SecurityDirectory(m) => m.symbol,
            Message::TradingStatus(m) => m.symbol,
            Message::OperationalHalt(m)
            | Message::ShortSalePriceTest(m)
            | Message::SecurityEvent(m) => m.symbol,
            Message::OfficialPrice(m) => m.symbol,
            Message::Auction(m) => m.symbol(),
            Message::SystemEvent(_) | Message::Unknown { .. } => return None,
        })
    }

    /// Feed timestamp, nanoseconds since the epoch.
    #[must_use]
    pub fn timestamp(&self) -> Option<Timestamp> {
        Some(match self {
            Message::PriceLevel(m) => m.timestamp,
            Message::Trade(m) => m.timestamp,
            Message::SystemEvent(m) => m.timestamp,
            Message::SecurityDirectory(m) => m.timestamp,
            Message::TradingStatus(m) => m.timestamp,
            Message::OperationalHalt(m)
            | Message::ShortSalePriceTest(m)
            | Message::SecurityEvent(m) => m.timestamp,
            Message::OfficialPrice(m) => m.timestamp,
            Message::Auction(m) => m.timestamp(),
            Message::Unknown { .. } => return None,
        })
    }
}

// Message type bytes.
pub const T_SYSTEM_EVENT: u8 = 0x53;
pub const T_SECURITY_DIRECTORY: u8 = 0x44;
pub const T_TRADING_STATUS: u8 = 0x48;
pub const T_OPERATIONAL_HALT: u8 = 0x4f;
pub const T_SHORT_SALE_PRICE_TEST: u8 = 0x50;
pub const T_SECURITY_EVENT: u8 = 0x45;
pub const T_PRICE_LEVEL_BUY: u8 = 0x38;
pub const T_PRICE_LEVEL_SELL: u8 = 0x35;
pub const T_TRADE_REPORT: u8 = 0x54;
pub const T_OFFICIAL_PRICE: u8 = 0x58;
pub const T_TRADE_BREAK: u8 = 0x42;
pub const T_AUCTION_INFORMATION: u8 = 0x41;

/// Wire length for a known message type.
///
/// Empirically confirmed over 40,223,554 messages: one length per type, always.
#[must_use]
pub const fn expected_len(kind: u8) -> Option<usize> {
    Some(match kind {
        T_SYSTEM_EVENT => 10,
        T_SECURITY_DIRECTORY => 31,
        T_TRADING_STATUS => 22,
        T_OPERATIONAL_HALT | T_SECURITY_EVENT => 18,
        T_SHORT_SALE_PRICE_TEST => 19,
        T_PRICE_LEVEL_BUY | T_PRICE_LEVEL_SELL => 30,
        T_TRADE_REPORT | T_TRADE_BREAK => 38,
        T_OFFICIAL_PRICE => 26,
        T_AUCTION_INFORMATION => 80,
        _ => return None,
    })
}

/// Decode one message body.
///
/// Returns [`Message::Unknown`] for a type this decoder does not model, and
/// `Err` only when a *known* type arrives at the wrong length — which means the
/// framing above is wrong, not that the message is exotic.
#[inline]
pub fn parse(body: &[u8]) -> Result<Message<'_>, crate::Error> {
    let Some(&kind) = body.first() else {
        return Err(crate::Error::Empty);
    };
    let Some(want) = expected_len(kind) else {
        return Ok(Message::Unknown { kind, body });
    };
    if body.len() != want {
        return Err(crate::Error::WrongLength {
            kind,
            expected: want,
            got: body.len(),
        });
    }

    Ok(match kind {
        // The hot path, first: 97% of messages land here.
        T_PRICE_LEVEL_BUY | T_PRICE_LEVEL_SELL => Message::PriceLevel(PriceLevelUpdate {
            side: if kind == T_PRICE_LEVEL_BUY {
                Side::Buy
            } else {
                Side::Sell
            },
            flags: body[1],
            timestamp: i64_le(body, 2),
            symbol: symbol(body, 10),
            size: u32_le(body, 18),
            price: Price::from_raw(i64_le(body, 22)),
        }),
        T_TRADE_REPORT | T_TRADE_BREAK => Message::Trade(Trade {
            is_break: kind == T_TRADE_BREAK,
            sale_condition: body[1],
            timestamp: i64_le(body, 2),
            symbol: symbol(body, 10),
            size: u32_le(body, 18),
            price: Price::from_raw(i64_le(body, 22)),
            trade_id: i64_le(body, 30),
        }),
        T_SYSTEM_EVENT => Message::SystemEvent(SystemEvent {
            event: body[1],
            timestamp: i64_le(body, 2),
        }),
        T_SECURITY_DIRECTORY => Message::SecurityDirectory(SecurityDirectory {
            flags: body[1],
            timestamp: i64_le(body, 2),
            symbol: symbol(body, 10),
            round_lot_size: u32_le(body, 18),
            adjusted_poc_price: Price::from_raw(i64_le(body, 22)),
            luld_tier: body[30],
        }),
        T_TRADING_STATUS => Message::TradingStatus(TradingStatus {
            status: body[1],
            timestamp: i64_le(body, 2),
            symbol: symbol(body, 10),
            reason: [body[18], body[19], body[20], body[21]],
        }),
        T_OPERATIONAL_HALT | T_SECURITY_EVENT | T_SHORT_SALE_PRICE_TEST => {
            let s = SymbolStatus {
                value: body[1],
                timestamp: i64_le(body, 2),
                symbol: symbol(body, 10),
                detail: if kind == T_SHORT_SALE_PRICE_TEST {
                    body[18]
                } else {
                    0
                },
            };
            match kind {
                T_OPERATIONAL_HALT => Message::OperationalHalt(s),
                T_SECURITY_EVENT => Message::SecurityEvent(s),
                _ => Message::ShortSalePriceTest(s),
            }
        }
        T_OFFICIAL_PRICE => Message::OfficialPrice(OfficialPrice {
            price_type: body[1],
            timestamp: i64_le(body, 2),
            symbol: symbol(body, 10),
            price: Price::from_raw(i64_le(body, 18)),
        }),
        T_AUCTION_INFORMATION => Message::Auction(AuctionInformation { body }),
        _ => unreachable!("expected_len covers every arm above"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plu(kind: u8, flags: u8, sym: &str, size: u32, price: Price) -> Vec<u8> {
        let mut v = vec![kind, flags];
        v.extend_from_slice(&1_500_000_000_000_000_000i64.to_le_bytes());
        v.extend_from_slice(&Symbol::from_ticker(sym).0);
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&price.raw().to_le_bytes());
        v
    }

    #[test]
    fn parses_a_buy_price_level() {
        let raw = plu(
            T_PRICE_LEVEL_BUY,
            0x01,
            "AAPL",
            100,
            Price::from_parts(186, 5500),
        );
        assert_eq!(raw.len(), 30);
        let Message::PriceLevel(m) = parse(&raw).unwrap() else {
            panic!("wrong variant")
        };
        assert_eq!(m.side, Side::Buy);
        assert_eq!(m.symbol.as_str(), Some("AAPL"));
        assert_eq!(m.size, 100);
        assert_eq!(m.price.to_string(), "186.55");
        assert!(m.event_complete());
        assert!(!m.is_delete());
    }

    #[test]
    fn sell_side_is_distinguished_by_type_byte() {
        let raw = plu(
            T_PRICE_LEVEL_SELL,
            0,
            "MSFT",
            500,
            Price::from_parts(101, 0),
        );
        let Message::PriceLevel(m) = parse(&raw).unwrap() else {
            panic!()
        };
        assert_eq!(m.side, Side::Sell);
        assert!(!m.event_complete(), "flag bit clear");
        // Bit 7 must NOT be mistaken for the completion flag; the original
        // implementation used 0x80 and no synthetic test could see it.
        let raw = plu(T_PRICE_LEVEL_SELL, 0x80, "MSFT", 1, Price::ZERO);
        let Message::PriceLevel(m) = parse(&raw).unwrap() else {
            panic!()
        };
        assert!(!m.event_complete(), "0x80 is not the completion bit");
    }

    /// Zero size deletes the level. Reading it as "a level holding no shares"
    /// leaves dead levels in the book and corrupts the touch.
    #[test]
    fn zero_size_is_a_delete() {
        let raw = plu(
            T_PRICE_LEVEL_BUY,
            0x01,
            "AAPL",
            0,
            Price::from_parts(186, 5500),
        );
        let Message::PriceLevel(m) = parse(&raw).unwrap() else {
            panic!()
        };
        assert!(m.is_delete());
    }

    /// A known type at an unexpected length means the framing above is wrong.
    /// Silently parsing the prefix would yield a confident wrong answer.
    #[test]
    fn known_type_at_wrong_length_is_an_error() {
        let mut raw = plu(T_PRICE_LEVEL_BUY, 0, "AAPL", 1, Price::ZERO);
        raw.push(0);
        assert!(matches!(
            parse(&raw),
            Err(crate::Error::WrongLength {
                kind: T_PRICE_LEVEL_BUY,
                expected: 30,
                got: 31
            })
        ));
    }

    #[test]
    fn unknown_type_is_preserved_not_dropped() {
        let raw = [0xEEu8, 1, 2, 3];
        assert!(matches!(
            parse(&raw),
            Ok(Message::Unknown { kind: 0xEE, .. })
        ));
    }

    #[test]
    fn empty_body_errors() {
        assert!(matches!(parse(&[]), Err(crate::Error::Empty)));
    }

    #[test]
    fn truncated_bodies_never_panic() {
        let raw = plu(T_PRICE_LEVEL_BUY, 0, "AAPL", 1, Price::ZERO);
        for n in 0..raw.len() {
            let _ = parse(&raw[..n]);
        }
    }

    /// The hot path returns `Message` by value, so its size is a latency
    /// concern. Auction Information is 80 bytes on the wire and is borrowed
    /// rather than inlined precisely to keep this small; this test fails if
    /// someone later inlines it.
    #[test]
    fn message_stays_small_enough_for_the_hot_path() {
        let size = std::mem::size_of::<Message<'_>>();
        assert!(
            size <= 64,
            "Message grew to {size} bytes; it must fit a cache line"
        );
    }
}
