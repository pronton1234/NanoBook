// Black-Scholes-Merton: analytic prices and Greeks, from scratch.
//
// No library call does the work here -- the normal CDF is built on erfc, and
// every Greek is the closed-form derivative rather than a finite difference of
// the price. That distinction matters: a bumped price is an approximation whose
// error you have to reason about, while the closed form is exact and costs one
// evaluation instead of two.
//
// The model's assumptions are worth stating because they are what a Monte Carlo
// engine has to reproduce to agree with it: the underlying follows geometric
// Brownian motion with constant volatility and constant risk-free rate, there
// are no transaction costs, and the option is European so early exercise is not
// a consideration.
#pragma once

#include <cmath>

namespace quant {

enum class OptionType { Call, Put };

/// Standard normal probability density.
[[nodiscard]] inline double norm_pdf(double x) noexcept {
    static constexpr double kInvSqrt2Pi = 0.398942280401432677939946059934;
    return kInvSqrt2Pi * std::exp(-0.5 * x * x);
}

/// Standard normal cumulative distribution.
///
/// Via erfc rather than erf. For very negative x, `0.5 * (1 + erf(x/sqrt2))`
/// subtracts two nearly equal numbers and loses most of its significant digits;
/// `0.5 * erfc(-x/sqrt2)` computes the same value without the cancellation. Deep
/// out-of-the-money options live exactly in that tail.
[[nodiscard]] inline double norm_cdf(double x) noexcept {
    static constexpr double kInvSqrt2 = 0.707106781186547524400844362105;
    return 0.5 * std::erfc(-x * kInvSqrt2);
}

/// Inputs to a European option valuation.
struct Inputs {
    double spot = 0.0;
    double strike = 0.0;
    /// Annualised, continuously compounded.
    double rate = 0.0;
    /// Annualised volatility.
    double vol = 0.0;
    /// Time to expiry in years.
    double time = 0.0;
};

struct Greeks {
    double price = 0.0;
    double delta = 0.0;
    double gamma = 0.0;
    /// Per 1.00 of volatility (not per volatility point).
    double vega = 0.0;
    /// Per year (not per day).
    double theta = 0.0;
    double rho = 0.0;
};

/// d1 and d2 from the standard parameterisation.
struct Ds {
    double d1;
    double d2;
};

[[nodiscard]] inline Ds compute_ds(const Inputs& in) noexcept {
    const double vsqrt = in.vol * std::sqrt(in.time);
    const double d1 =
        (std::log(in.spot / in.strike) + (in.rate + 0.5 * in.vol * in.vol) * in.time) / vsqrt;
    return Ds{d1, d1 - vsqrt};
}

/// Price and all five first- and second-order Greeks in one pass.
///
/// Computed together because they share d1, d2 and the discount factor. Calling
/// five separate functions would recompute those every time, and on a surface
/// with thousands of strikes that is the whole cost.
[[nodiscard]] Greeks price_with_greeks(const Inputs& in, OptionType type) noexcept;

[[nodiscard]] inline double price(const Inputs& in, OptionType type) noexcept {
    return price_with_greeks(in, type).price;
}

/// The result of an implied-volatility solve, with its conditioning.
struct ImpliedVol {
    double vol = 0.0;
    /// Vega at the solution. This is the conditioning of the problem.
    double vega = 0.0;
    /// False when vega is so small that the price does not identify a
    /// volatility. See below -- this is a property of the input, not a failure
    /// of the solver.
    bool well_conditioned = false;
};

/// Vega below which an implied volatility is not identified by the price.
///
/// At 1e-4 of price sensitivity per unit of volatility, a price known to 1e-8
/// pins the volatility only to about 1e-4 -- and real quotes are known to a
/// cent, not to 1e-8.
inline constexpr double kMinVegaForIdentification = 1e-4;

/// Implied volatility by Newton-Raphson, falling back to bisection.
///
/// Newton converges quadratically and is the right first choice, but vega goes
/// to zero for deep in- or out-of-the-money options and the step then explodes.
/// Bisection cannot diverge, so it is the safety net rather than the default.
///
/// **The conditioning is returned, and it matters more than the root-finder.**
/// A deep in-the-money option at low volatility is almost entirely intrinsic
/// value: at S=125, K=100, vol=5%, vega is 4e-6, so a price matched to 1e-8
/// still leaves the volatility uncertain in the third decimal. Many volatilities
/// produce that price. No solver can do better, because the information is not
/// in the input.
///
/// Reporting a number anyway would be the same error as a histogram reporting
/// its own bucket count as a p99: a plausible figure that is not a measurement.
/// So `well_conditioned` is false there and the caller decides.
///
/// Returns NaN when the target price is outside the no-arbitrage bounds, because
/// no volatility produces it at all.
[[nodiscard]] ImpliedVol implied_vol(double target_price, const Inputs& in, OptionType type,
                                     double tol = 1e-8, int max_iter = 100) noexcept;

/// Put-call parity residual: C - P - (S - K e^{-rT}).
///
/// Must be zero to floating-point precision for any consistent pricer. It is a
/// model-free identity -- it follows from arbitrage alone, not from
/// Black-Scholes -- which makes it the strongest single check available.
[[nodiscard]] double parity_residual(const Inputs& in) noexcept;

}  // namespace quant
