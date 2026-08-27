// Tests for the pricing library.
//
// The interesting ones are not "does it return a number". They are the
// identities that must hold for any correct pricer regardless of implementation:
// put-call parity, which follows from arbitrage alone; gamma and vega being
// equal for a call and a put; and the Monte Carlo confidence interval actually
// containing the analytic price rather than merely being near it.
#include <cmath>
#include <cstdio>
#include <vector>

#include "quant/black_scholes.hpp"
#include "quant/monte_carlo.hpp"

namespace {

int g_failures = 0;
int g_checks = 0;

void check(bool cond, const char* what, int line) {
    ++g_checks;
    if (!cond) {
        ++g_failures;
        std::printf("  FAIL (line %d): %s\n", line, what);
    }
}
#define CHECK(c) check((c), #c, __LINE__)

bool close(double a, double b, double tol) { return std::abs(a - b) <= tol; }

using namespace quant;

Inputs atm() { return Inputs{100.0, 100.0, 0.05, 0.20, 1.0}; }

/// A known-good reference. S=100, K=100, r=5%, vol=20%, T=1 is the textbook
/// case and the call is 10.4506 to four decimals.
void test_against_known_value() {
    const double c = price(atm(), OptionType::Call);
    CHECK(close(c, 10.450583572185565, 1e-9));
    const double p = price(atm(), OptionType::Put);
    CHECK(close(p, 5.573526022256971, 1e-9));
}

/// Put-call parity: C - P = S - K e^{-rT}.
///
/// Model-free. It follows from arbitrage, not from Black-Scholes, so it holds
/// for any consistent pricer and catches a sign error anywhere in either branch.
void test_put_call_parity() {
    for (double s : {60.0, 90.0, 100.0, 110.0, 150.0})
        for (double v : {0.05, 0.20, 0.80})
            for (double t : {0.05, 1.0, 5.0}) {
                Inputs in{s, 100.0, 0.05, v, t};
                CHECK(std::abs(parity_residual(in)) < 1e-10);
            }
}

/// Gamma and vega are second-order in the payoff and identical for a call and a
/// put at the same strike -- the density does not care about the option's sign.
void test_greeks_shared_between_call_and_put() {
    const Greeks c = price_with_greeks(atm(), OptionType::Call);
    const Greeks p = price_with_greeks(atm(), OptionType::Put);
    CHECK(close(c.gamma, p.gamma, 1e-14));
    CHECK(close(c.vega, p.vega, 1e-14));
    // Delta parity: delta_call - delta_put = 1.
    CHECK(close(c.delta - p.delta, 1.0, 1e-12));
}

/// Every analytic Greek must match a central finite difference of the price.
///
/// This is the check that the derivatives were differentiated correctly rather
/// than transcribed from memory. The tolerance is loose because the finite
/// difference is the approximation here, not the closed form.
void test_greeks_match_finite_differences() {
    const Inputs in = atm();
    for (auto type : {OptionType::Call, OptionType::Put}) {
        const Greeks g = price_with_greeks(in, type);

        auto bump = [&](double ds, double dv, double dt, double dr) {
            Inputs b = in;
            b.spot += ds; b.vol += dv; b.time += dt; b.rate += dr;
            return price(b, type);
        };

        const double h = 1e-4;
        const double d_delta = (bump(h, 0, 0, 0) - bump(-h, 0, 0, 0)) / (2 * h);
        CHECK(close(g.delta, d_delta, 1e-6));

        const double d_gamma =
            (bump(h, 0, 0, 0) - 2 * price(in, type) + bump(-h, 0, 0, 0)) / (h * h);
        CHECK(close(g.gamma, d_gamma, 1e-4));

        const double d_vega = (bump(0, h, 0, 0) - bump(0, -h, 0, 0)) / (2 * h);
        CHECK(close(g.vega, d_vega, 1e-5));

        const double d_rho = (bump(0, 0, 0, h) - bump(0, 0, 0, -h)) / (2 * h);
        CHECK(close(g.rho, d_rho, 1e-5));

        // Theta is the derivative with respect to time to expiry, negated:
        // the option decays as expiry approaches.
        const double d_theta = -(bump(0, 0, h, 0) - bump(0, 0, -h, 0)) / (2 * h);
        CHECK(close(g.theta, d_theta, 1e-4));
    }
}

/// Round-tripping a price through implied vol recovers the input vol -- WHERE
/// THE PROBLEM IS WELL CONDITIONED.
///
/// Where it is not, the solver must say so rather than return a number. At
/// S=125, K=100, vol=5% the option is almost pure intrinsic value and vega is
/// 4e-6, so a price matched to 1e-8 leaves the volatility uncertain in the third
/// decimal. That is a property of the input, not a bug: many volatilities
/// produce that price.
void test_implied_vol_round_trips() {
    int conditioned = 0, unconditioned = 0;
    for (double v : {0.05, 0.15, 0.35, 0.90}) {
        for (double s : {80.0, 100.0, 125.0}) {
            Inputs in{s, 100.0, 0.03, v, 0.75};
            const double c = price(in, OptionType::Call);
            const ImpliedVol iv = implied_vol(c, in, OptionType::Call);
            if (iv.well_conditioned) {
                ++conditioned;
                CHECK(close(iv.vol, v, 1e-6));
            } else {
                ++unconditioned;
                // Still solves the PRICE, just not the volatility.
                Inputs at = in;
                at.vol = iv.vol;
                CHECK(close(price(at, OptionType::Call), c, 1e-7));
            }
        }
    }
    CHECK(conditioned >= 9);
    // The deep-ITM low-vol corner must be detected, not silently returned.
    CHECK(unconditioned >= 1);

    Inputs deep{125.0, 100.0, 0.03, 0.05, 0.75};
    const double c = price(deep, OptionType::Call);
    CHECK(!implied_vol(c, deep, OptionType::Call).well_conditioned);

    // A price outside the no-arbitrage bounds has no implied vol at all.
    Inputs in = atm();
    CHECK(std::isnan(implied_vol(in.spot * 2.0, in, OptionType::Call).vol));
}

/// The central claim: the Monte Carlo confidence interval contains the analytic
/// price. Not "is close to" -- contains.
void test_monte_carlo_agrees_with_analytic() {
    const Inputs in = atm();
    for (auto type : {OptionType::Call, OptionType::Put}) {
        const double exact = price(in, type);
        McConfig cfg;
        cfg.paths = 400'000;
        const McResult mc = price_mc(in, type, cfg);
        CHECK(mc.paths == cfg.paths);
        CHECK(mc.contains(exact));
        // The interval must also be tight enough for the containment to mean
        // something; a huge interval contains everything.
        CHECK(mc.ci95() < 0.05 * exact);
    }
}

/// Variance reduction must actually reduce variance, not merely be present.
void test_variance_reduction_reduces_variance() {
    const Inputs in = atm();
    McConfig plain{200'000, 12345, false, false};
    McConfig anti{200'000, 12345, true, false};
    McConfig both{200'000, 12345, true, true};

    const McResult p = price_mc(in, OptionType::Call, plain);
    const McResult a = price_mc(in, OptionType::Call, anti);
    const McResult b = price_mc(in, OptionType::Call, both);

    CHECK(a.std_error < p.std_error);
    CHECK(b.std_error < a.std_error);
    // All three must still be unbiased.
    const double exact = price(in, OptionType::Call);
    CHECK(p.contains(exact));
    CHECK(a.contains(exact));
    CHECK(b.contains(exact));
}

/// Standard error must fall as 1/sqrt(N). Quadrupling the paths should halve it.
///
/// This is the property that makes Monte Carlo predictable: it tells you how
/// many paths buy how much precision, before you run them.
void test_error_falls_as_inverse_sqrt_n() {
    const Inputs in = atm();
    McConfig small{50'000, 999, false, false};
    McConfig large{200'000, 999, false, false};
    const double se_small = price_mc(in, OptionType::Call, small).std_error;
    const double se_large = price_mc(in, OptionType::Call, large).std_error;
    const double ratio = se_small / se_large;
    // 4x the paths => 2x the precision, within sampling noise on the estimate
    // of the standard error itself.
    CHECK(ratio > 1.7 && ratio < 2.3);
}

/// An Asian option has no closed form, which is the reason the engine exists.
/// Averaging reduces the variance of the payoff, so it must price below the
/// European call -- a bound that holds without any analytic formula.
void test_asian_is_cheaper_than_european() {
    const Inputs in = atm();
    McConfig cfg;
    cfg.paths = 200'000;
    const McResult asian = price_asian_call(in, 52, cfg);
    const double european = price(in, OptionType::Call);
    CHECK(asian.price < european);
    CHECK(asian.price > 0.0);
    CHECK(asian.std_error > 0.0);
}

/// Degenerate inputs must return intrinsic value, not divide by zero.
void test_expiry_and_zero_vol() {
    Inputs in{110.0, 100.0, 0.05, 0.20, 0.0};
    CHECK(close(price(in, OptionType::Call), 10.0, 1e-12));
    CHECK(close(price(in, OptionType::Put), 0.0, 1e-12));
    in.time = 1.0;
    in.vol = 0.0;
    CHECK(close(price(in, OptionType::Call), 10.0, 1e-12));
}

}  // namespace

int main() {
    std::printf("running quant tests\n");
    test_against_known_value();
    test_put_call_parity();
    test_greeks_shared_between_call_and_put();
    test_greeks_match_finite_differences();
    test_implied_vol_round_trips();
    test_monte_carlo_agrees_with_analytic();
    test_variance_reduction_reduces_variance();
    test_error_falls_as_inverse_sqrt_n();
    test_asian_is_cheaper_than_european();
    test_expiry_and_zero_vol();
    std::printf("%d checks, %d failures\n", g_checks, g_failures);
    return g_failures == 0 ? 0 : 1;
}
