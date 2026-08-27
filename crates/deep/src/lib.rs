//! IEX DEEP 1.0 message decoding.
//!
//! DEEP is an aggregated-depth feed: it publishes the total displayed size at
//! each price level, rather than every individual order. That makes the book a
//! map from price to size, not an order-id graph — which is a genuinely
//! different data structure from an order-by-order feed like ITCH, and simpler
//! in exactly one dimension while being just as latency-sensitive.
//!
//! Every message is **fixed width**, one length per type. That is not assumed
//! from a specification; it was measured. A histogram of (type, length) over
//! 40,223,554 messages from a full trading day plus the 2017 fixture shows
//! exactly one length per type with no exceptions, and the parser enforces it:
//!
//! | type | bytes | | type | bytes |
//! |---|---|---|---|---|
//! | Price Level Update | 30 | | Trading Status | 22 |
//! | Trade Report / Break | 38 | | Short Sale Price Test | 19 |
//! | Auction Information | 80 | | Security Event / Op Halt | 18 |
//! | Security Directory | 31 | | System Event | 10 |
//! | Official Price | 26 | | | |
//!
//! **97% of traffic is price level updates** (48.8% sell, 48.1% buy), with trade
//! reports at 2.9% and everything else under 0.2%. Every design decision here
//! follows from that: the price-level arm is the first branch, [`Message`] is
//! kept inside a cache line, and the 80-byte auction message is borrowed rather
//! than inlined so the rare case cannot slow the common one.
//!
//! Nothing allocates. Symbols are fixed 8-byte arrays and prices are integers;
//! no floating point touches a price anywhere in this crate.

pub mod messages;
pub mod price;
pub mod symbol;

pub use messages::{
    expected_len, parse, AuctionInformation, Message, OfficialPrice, PriceLevelUpdate,
    SecurityDirectory, Side, SymbolStatus, SystemEvent, Timestamp, Trade, TradingStatus,
};
pub use price::{Price, UNITS_PER_DOLLAR};
pub use symbol::Symbol;

/// A malformed message, as opposed to a merely unrecognised one.
///
/// An unknown message *type* is not an error — it is reported as
/// [`Message::Unknown`], because a capture may contain a newer message than this
/// decoder models and that should still be countable. A known type at the wrong
/// length is an error, because fixed-width messages that are not fixed-width
/// mean the framing above is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("empty message body")]
    Empty,
    #[error("message type 0x{kind:02x} is {got} bytes, expected {expected}")]
    WrongLength {
        kind: u8,
        expected: usize,
        got: usize,
    },
}
