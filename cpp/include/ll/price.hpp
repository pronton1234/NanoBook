// Fixed-point prices. No floating point touches a price, ever.
//
// DEEP quotes prices as a signed 64-bit integer in units of 1/10000 dollar, so
// $99.99 is 999'900. That representation is exact, and the only way to lose it
// is to convert to double "just for display" and let the converted value leak
// back into arithmetic.
//
// So Price offers no conversion to double at all and formats itself by integer
// division. It is a distinct type rather than an alias for std::int64_t, so a
// raw count of shares cannot be passed where a price is expected -- the C++
// equivalent of the newtype the Rust side uses.
#pragma once

#include <compare>
#include <cstdint>
#include <optional>
#include <string>

namespace ll {

class Price {
public:
    /// Price units per whole dollar. DEEP uses four implied decimal places.
    static constexpr std::int64_t kUnitsPerDollar = 10'000;

    constexpr Price() = default;
    explicit constexpr Price(std::int64_t units) noexcept : units_(units) {}

    /// For tests and literals: dollars plus ten-thousandths.
    static constexpr Price from_parts(std::int64_t dollars, std::int64_t frac) noexcept {
        return Price{dollars * kUnitsPerDollar + frac};
    }

    [[nodiscard]] constexpr std::int64_t raw() const noexcept { return units_; }
    [[nodiscard]] constexpr bool is_zero() const noexcept { return units_ == 0; }

    /// Index into a flat array of price levels, given a tick size.
    ///
    /// Returns nullopt when the price is not on the grid, which sub-dollar and
    /// sub-penny prices are not. Silently rounding would put a level in the
    /// wrong bucket, which is worse than refusing.
    [[nodiscard]] constexpr std::optional<std::size_t> tick_index(std::int64_t tick) const noexcept {
        if (tick <= 0 || units_ < 0 || units_ % tick != 0) return std::nullopt;
        return static_cast<std::size_t>(units_ / tick);
    }

    /// Exact, by integer division. Never via double.
    [[nodiscard]] std::string to_string() const;

    friend constexpr auto operator<=>(Price, Price) noexcept = default;
    friend constexpr bool operator==(Price, Price) noexcept = default;

private:
    std::int64_t units_ = 0;
};

}  // namespace ll
