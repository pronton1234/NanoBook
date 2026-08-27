// Monte Carlo pricing under geometric Brownian motion, with variance reduction.
//
// The point is not to price a European call -- Black-Scholes does that exactly
// and instantly. The point is that a Monte Carlo engine which agrees with a
// closed form on the problem the closed form solves can then be trusted on the
// problems it does not: path dependence, early exercise, anything without an
// analytic answer.
//
// So the engine reports a **standard error**, and the test asserts the analytic
// price falls inside the confidence interval. "Close enough" is not a test; an
// interval that contains the true value is.
//
// Two variance reduction techniques, both of which reduce the standard error at
// fixed sample count rather than making the estimate merely look tidier:
//
//   * ANTITHETIC VARIATES. For every draw Z, also use -Z. The two payoffs are
//     negatively correlated, so their average has lower variance than two
//     independent draws. Free, since the second path costs no extra randomness.
//
//   * CONTROL VARIATES. The discounted terminal spot has a known expectation
//     (the forward). Subtracting its sampling error, scaled by the regression
//     coefficient between it and the payoff, removes the part of the payoff's
//     error that the control explains.
#pragma once

#include <cstdint>

#include "quant/black_scholes.hpp"

namespace quant {

struct McResult {
    double price = 0.0;
    /// Standard error of the estimate. The estimate is meaningless without it.
    double std_error = 0.0;
    std::uint64_t paths = 0;

    /// Half-width of the 95% confidence interval.
    [[nodiscard]] double ci95() const noexcept { return 1.959963985 * std_error; }
    [[nodiscard]] bool contains(double value) const noexcept {
        return value >= price - ci95() && value <= price + ci95();
    }
};

struct McConfig {
    std::uint64_t paths = 1'000'000;
    std::uint64_t seed = 0x2545'F491'4F6C'DD1D;
    bool antithetic = true;
    bool control_variate = true;
};

/// Price a European option by simulation.
[[nodiscard]] McResult price_mc(const Inputs& in, OptionType type, const McConfig& cfg) noexcept;

/// An Asian (arithmetic average) call, which has no closed form.
///
/// Included to make the point of the exercise concrete: the engine is validated
/// against Black-Scholes on the European case, and then used where no analytic
/// answer exists. The price must sit below the European call, because averaging
/// reduces the variance of the payoff.
[[nodiscard]] McResult price_asian_call(const Inputs& in, int steps,
                                        const McConfig& cfg) noexcept;

}  // namespace quant
