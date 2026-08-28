// Eight-byte, space-padded ticker symbols.
//
// DEEP carries the symbol inline as a fixed 8-byte field padded with spaces, so
// "AAPL" is "AAPL    ". Keeping the raw array rather than a std::string matters:
// 97% of feed traffic is price level updates, and allocating for each one would
// dominate the parse.
//
// Trivially copyable, 8 bytes, and comparable as a single 64-bit load.
#pragma once

#include <array>
#include <cstdint>
#include <cstring>
#include <string_view>

namespace nb {

class Symbol {
public:
    constexpr Symbol() noexcept : bytes_{} { bytes_.fill(' '); }

    explicit Symbol(const std::uint8_t* src) noexcept { std::memcpy(bytes_.data(), src, 8); }

    /// Build from a ticker. Longer than 8 truncates, matching what the wire
    /// format can represent.
    static Symbol from_ticker(std::string_view t) noexcept {
        Symbol s;
        const std::size_t n = t.size() < 8 ? t.size() : 8;
        std::memcpy(s.bytes_.data(), t.data(), n);
        return s;
    }

    /// The eight bytes as one integer.
    ///
    /// Symbol equality is the most common comparison in the pipeline, and as a
    /// 64-bit value it is a single instruction rather than a memcmp.
    [[nodiscard]] std::uint64_t as_u64() const noexcept {
        std::uint64_t v;
        std::memcpy(&v, bytes_.data(), 8);
        return v;
    }

    [[nodiscard]] std::string_view trimmed() const noexcept {
        std::size_t end = 8;
        while (end > 0 && bytes_[end - 1] == ' ') --end;
        return {reinterpret_cast<const char*>(bytes_.data()), end};
    }

    friend bool operator==(const Symbol& a, const Symbol& b) noexcept {
        return a.as_u64() == b.as_u64();
    }

private:
    std::array<std::uint8_t, 8> bytes_;
};

/// Hash for Symbol, which is eight bytes of entropy-poor ASCII.
///
/// std::hash<std::string> style hashing is overkill and the default identity on
/// an integer would cluster badly, because tickers share leading bytes. This is
/// the fibonacci-multiply trick -- one multiply, one shift -- which spreads the
/// low-entropy bytes across the whole word. Matches the Rust side's hasher so
/// the two implementations are doing the same work.
struct SymbolHash {
    std::size_t operator()(const Symbol& s) const noexcept {
        std::uint64_t h = s.as_u64() * 0x9E37'79B9'7F4A'7C15ull;
        return static_cast<std::size_t>(h ^ (h >> 32));
    }
};

}  // namespace nb
