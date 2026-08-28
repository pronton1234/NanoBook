// Aggregated-depth order book.
//
// The container choice was made by measuring the market, not by reaching for the
// textbook answer. Sampled across 6,530 symbols on a full trading day:
//
//     levels per side    p50 2    p90 5    p99 14    max 52
//     price span (ticks) p50 480  p90 1713 p99 4907
//
// Books are shallow AND sparse. The standard advice -- a flat array indexed by
// price tick with a bitmap to find the touch -- is built for a dense ladder
// hundreds of levels deep. Here it would reserve ~2KB to hold 24 bytes and walk
// cache lines of zeros on every touch lookup, losing by two orders of magnitude
// on the axis it was chosen for.
//
// So: keep the levels sorted and contiguous, and scan them. Two decisions follow
// from p50 = 2:
//
//   * Linear scan, not binary search. At five elements a binary search is two or
//     three unpredictable branches; a linear scan is five perfectly predicted
//     ones. Binary search wins asymptotically and loses at the size that occurs.
//   * Structure of arrays. Prices and sizes live in separate vectors, so the
//     scan -- which reads only prices -- touches 8 prices per cache line instead
//     of 5 interleaved pairs.
#pragma once

#include <cstdint>
#include <optional>
#include <utility>
#include <vector>

#include "nb/deep.hpp"
#include "nb/flat_map.hpp"
#include "nb/price.hpp"
#include "nb/symbol.hpp"

namespace nb {

using Level = std::pair<Price, std::uint32_t>;

/// One side of one symbol's book: price to displayed size, sorted ascending.
class Levels {
public:
    /// Set the displayed size at a price. A size of zero removes the level.
    void set(Price price, std::uint32_t size) noexcept;

    /// Highest price holding size. The best bid.
    [[nodiscard]] std::optional<Level> max() const noexcept {
        if (prices_.empty()) return std::nullopt;
        return Level{prices_.back(), sizes_.back()};
    }

    /// Lowest price holding size. The best ask.
    [[nodiscard]] std::optional<Level> min() const noexcept {
        if (prices_.empty()) return std::nullopt;
        return Level{prices_.front(), sizes_.front()};
    }

    [[nodiscard]] std::size_t size() const noexcept { return prices_.size(); }
    [[nodiscard]] bool empty() const noexcept { return prices_.empty(); }
    [[nodiscard]] std::uint64_t total_size() const noexcept;

    void clear() noexcept {
        prices_.clear();
        sizes_.clear();
    }

private:
    /// Locate a price: index if present, insertion point if not.
    ///
    /// Deliberately a forward linear scan rather than std::lower_bound. See the
    /// header comment: at the depths this feed produces, the branch predictor
    /// beats the algorithm.
    [[nodiscard]] std::size_t find(Price price, bool& found) const noexcept;

    std::vector<Price> prices_;
    std::vector<std::uint32_t> sizes_;
};

/// The touch after an update.
///
/// Returned by apply() because the update already had to read both sides to
/// check whether the book crossed. Handing that back saves the caller a second
/// hash lookup and two more reads of level storage -- on 97% of feed traffic.
struct Touch {
    std::optional<Level> bid;
    std::optional<Level> ask;
    bool stable = false;
};

struct BookStats {
    std::uint64_t updates = 0;
    std::uint64_t deletes = 0;
    /// Bid strictly above ask at an event boundary. The real violation.
    std::uint64_t crossed_when_stable = 0;
    /// Bid exactly equal to ask. Unusual, not a violation -- counted apart so a
    /// real crossing cannot hide inside a pile of benign locks.
    std::uint64_t locked_when_stable = 0;
};

/// One symbol's two sides.
struct SymbolBook {
    Levels bids;
    Levels asks;
    /// False while the feed is partway through an atomic book event.
    bool complete = false;
};

/// Every symbol's book.
class Book {
public:
    explicit Book(std::size_t symbols = 1024) : books_(symbols) {}

    /// The hot path.
    Touch apply(const PriceLevelUpdate& u) noexcept;

    [[nodiscard]] const SymbolBook* get(const Symbol& s) const noexcept {
        return books_.find(s);
    }

    [[nodiscard]] std::size_t symbol_count() const noexcept { return books_.size(); }
    [[nodiscard]] std::size_t total_levels() const noexcept;
    [[nodiscard]] std::uint64_t total_size() const noexcept;
    void clear() noexcept { books_.clear(); }

    BookStats stats;

private:
    FlatMap<Symbol, SymbolBook, SymbolHash> books_;
};

}  // namespace nb
