// IEX DEEP 1.0 message decoding.
//
// DEEP is an aggregated-depth feed: it publishes total displayed size at each
// price level rather than every order, so the book is price -> size and there is
// no order-id graph.
//
// Every message is fixed width, one length per type. That was measured, not
// transcribed: a histogram of (type, length) over 40,223,554 messages shows
// exactly one length per type with no exceptions. The parser enforces it, and a
// known type at an unexpected length is an error -- a fixed-width format that is
// suddenly not fixed-width means the framing above is wrong.
#pragma once

#include <cstdint>
#include <optional>
#include <variant>

#include "nb/capture.hpp"
#include "nb/price.hpp"
#include "nb/symbol.hpp"

namespace nb {

enum class Side : std::uint8_t { Buy, Sell };

/// A change to one price level. 97% of all feed traffic.
struct PriceLevelUpdate {
    Side side = Side::Buy;
    std::uint8_t flags = 0;
    std::int64_t timestamp = 0;
    Symbol symbol;
    std::uint32_t size = 0;
    Price price;

    /// Bit 0: this update completes an atomic book event.
    ///
    /// The mask is 0x01. The Rust implementation originally used 0x80 -- bit 7 --
    /// and nothing failed, because the synthetic tests were written from the same
    /// wrong belief. Only real data showed it, by reporting that 100% of updates
    /// were mid-event, which no functioning market produces.
    [[nodiscard]] constexpr bool event_complete() const noexcept { return (flags & 0x01) != 0; }

    /// A zero size removes the level. It is not a level holding no shares.
    [[nodiscard]] constexpr bool is_delete() const noexcept { return size == 0; }
};

struct Trade {
    bool is_break = false;
    std::int64_t timestamp = 0;
    Symbol symbol;
    std::uint32_t size = 0;
    Price price;
};

/// Anything this decoder does not model. Kept rather than dropped, so a capture
/// containing a newer message is still countable.
struct Unknown {
    std::uint8_t kind = 0;
};

using Message = std::variant<PriceLevelUpdate, Trade, Unknown>;

// Message type bytes.
inline constexpr std::uint8_t kPriceLevelBuy = 0x38;
inline constexpr std::uint8_t kPriceLevelSell = 0x35;
inline constexpr std::uint8_t kTradeReport = 0x54;
inline constexpr std::uint8_t kTradeBreak = 0x42;

/// Wire length for a known type, or nullopt if unmodelled.
[[nodiscard]] constexpr std::optional<std::size_t> expected_len(std::uint8_t kind) noexcept {
    switch (kind) {
        case 0x53: return 10;   // System Event
        case 0x44: return 31;   // Security Directory
        case 0x48: return 22;   // Trading Status
        case 0x4f: return 18;   // Operational Halt
        case 0x50: return 19;   // Short Sale Price Test
        case 0x45: return 18;   // Security Event
        case kPriceLevelBuy:
        case kPriceLevelSell: return 30;
        case kTradeReport:
        case kTradeBreak: return 38;
        case 0x58: return 26;   // Official Price
        case 0x41: return 80;   // Auction Information
        default: return std::nullopt;
    }
}

/// Decode one message body. Errors only when a KNOWN type arrives at the wrong
/// length; an unknown type is reported, not rejected.
[[nodiscard]] std::optional<Message> parse_message(Bytes body) noexcept;

}  // namespace nb
