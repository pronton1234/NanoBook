#include "quant/black_scholes.hpp"
#include "quant/monte_carlo.hpp"

#include <algorithm>
#include <cmath>
#include <limits>
#include <vector>

namespace quant {

// -------------------------------------------------------- Black-Scholes -----

Greeks price_with_greeks(const Inputs& in, OptionType type) noexcept {
    Greeks g;

    // Expiry, or a degenerate input. The limit is the intrinsic value, and the
    // Greeks collapse: gamma and vega vanish, delta becomes a step function.
    // Falling through to the general formula here would divide by zero.
    if (in.time <= 0.0 || in.vol <= 0.0) {
        const double intrinsic = (type == OptionType::Call)
                                     ? std::max(in.spot - in.strike, 0.0)
                                     : std::max(in.strike - in.spot, 0.0);
        g.price = intrinsic;
        g.delta = (type == OptionType::Call) ? (in.spot > in.strike ? 1.0 : 0.0)
                                             : (in.spot < in.strike ? -1.0 : 0.0);
        return g;
    }

    const Ds d = compute_ds(in);
    const double sqrt_t = std::sqrt(in.time);
    const double disc = std::exp(-in.rate * in.time);
    const double nd1 = norm_cdf(d.d1);
    const double nd2 = norm_cdf(d.d2);
    const double pdf1 = norm_pdf(d.d1);

    // Shared by both payoffs: the density does not care about the sign of the
    // option, so gamma and vega are identical for a call and a put at the same
    // strike. That equality is itself a test below.
    g.gamma = pdf1 / (in.spot * in.vol * sqrt_t);
    g.vega = in.spot * pdf1 * sqrt_t;

    if (type == OptionType::Call) {
        g.price = in.spot * nd1 - in.strike * disc * nd2;
        g.delta = nd1;
        g.theta = -(in.spot * pdf1 * in.vol) / (2.0 * sqrt_t) -
                  in.rate * in.strike * disc * nd2;
        g.rho = in.strike * in.time * disc * nd2;
    } else {
        g.price = in.strike * disc * norm_cdf(-d.d2) - in.spot * norm_cdf(-d.d1);
        g.delta = nd1 - 1.0;
        g.theta = -(in.spot * pdf1 * in.vol) / (2.0 * sqrt_t) +
                  in.rate * in.strike * disc * norm_cdf(-d.d2);
        g.rho = -in.strike * in.time * disc * norm_cdf(-d.d2);
    }
    return g;
}

double parity_residual(const Inputs& in) noexcept {
    const double c = price(in, OptionType::Call);
    const double p = price(in, OptionType::Put);
    return c - p - (in.spot - in.strike * std::exp(-in.rate * in.time));
}

ImpliedVol implied_vol(double target, const Inputs& in, OptionType type, double tol,
                       int max_iter) noexcept {
    ImpliedVol out;
    // No-arbitrage bounds. Outside them no volatility reproduces the price, and
    // returning a number anyway would be inventing one.
    const double disc = std::exp(-in.rate * in.time);
    const double lo_bound = (type == OptionType::Call)
                                ? std::max(in.spot - in.strike * disc, 0.0)
                                : std::max(in.strike * disc - in.spot, 0.0);
    const double hi_bound = (type == OptionType::Call) ? in.spot : in.strike * disc;
    if (target < lo_bound - 1e-12 || target > hi_bound + 1e-12) {
        out.vol = std::numeric_limits<double>::quiet_NaN();
        return out;
    }

    Inputs probe = in;
    probe.vol = 0.30;  // a reasonable starting guess for equity vol

    auto finish = [&](double vol) {
        Inputs at = in;
        at.vol = vol;
        out.vol = vol;
        out.vega = price_with_greeks(at, type).vega;
        out.well_conditioned = out.vega >= kMinVegaForIdentification;
        return out;
    };

    for (int i = 0; i < max_iter; ++i) {
        const Greeks g = price_with_greeks(probe, type);
        const double diff = g.price - target;
        if (std::abs(diff) < tol) return finish(probe.vol);
        // Vega collapses for deep in- or out-of-the-money options, and the
        // Newton step then explodes. Hand off to bisection rather than diverge.
        if (g.vega < 1e-10) break;
        const double next = probe.vol - diff / g.vega;
        if (!(next > 0.0) || next > 10.0) break;
        probe.vol = next;
    }

    // Bisection: slower, but it cannot fail to converge on a monotone function,
    // and price is strictly increasing in volatility.
    double lo = 1e-9, hi = 10.0;
    for (int i = 0; i < 200; ++i) {
        probe.vol = 0.5 * (lo + hi);
        const double diff = price(probe, type) - target;
        if (std::abs(diff) < tol) return finish(probe.vol);
        (diff > 0.0 ? hi : lo) = probe.vol;
    }
    return finish(probe.vol);
}

// ---------------------------------------------------------- Monte Carlo -----

namespace {

/// xoshiro256++ -- small, fast, and with far better statistical properties than
/// a linear congruential generator. Seeded explicitly so a price is reproducible;
/// a simulation that gives a different answer each run cannot be debugged.
class Rng {
public:
    explicit Rng(std::uint64_t seed) noexcept {
        // SplitMix64 to expand one seed into the four words of state, so a
        // low-entropy seed like 1 does not leave the generator in a poor region.
        for (auto& w : s_) {
            seed += 0x9E3779B97F4A7C15ull;
            std::uint64_t z = seed;
            z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ull;
            z = (z ^ (z >> 27)) * 0x94D049BB133111EBull;
            w = z ^ (z >> 31);
        }
    }

    [[nodiscard]] std::uint64_t next_u64() noexcept {
        const std::uint64_t r = rotl(s_[0] + s_[3], 23) + s_[0];
        const std::uint64_t t = s_[1] << 17;
        s_[2] ^= s_[0];
        s_[3] ^= s_[1];
        s_[1] ^= s_[2];
        s_[0] ^= s_[3];
        s_[2] ^= t;
        s_[3] = rotl(s_[3], 45);
        return r;
    }

    /// Uniform in (0, 1). The open interval matters: log(0) is -inf, and
    /// Box-Muller takes a log.
    [[nodiscard]] double next_uniform() noexcept {
        return (static_cast<double>(next_u64() >> 11) + 0.5) * 0x1.0p-53;
    }

    /// Standard normal by Box-Muller, returning both values it produces.
    ///
    /// The polar method is often faster but rejects samples, which makes the
    /// number of draws data-dependent and breaks reproducibility across
    /// antithetic pairing. Box-Muller consumes exactly two uniforms every time.
    void next_normals(double& z0, double& z1) noexcept {
        const double u1 = next_uniform();
        const double u2 = next_uniform();
        const double r = std::sqrt(-2.0 * std::log(u1));
        const double theta = 6.283185307179586476925286766559 * u2;
        z0 = r * std::cos(theta);
        z1 = r * std::sin(theta);
    }

private:
    static std::uint64_t rotl(std::uint64_t x, int k) noexcept {
        return (x << k) | (x >> (64 - k));
    }
    std::uint64_t s_[4];
};

double payoff(double spot, double strike, OptionType type) noexcept {
    return (type == OptionType::Call) ? std::max(spot - strike, 0.0)
                                      : std::max(strike - spot, 0.0);
}

}  // namespace

McResult price_mc(const Inputs& in, OptionType type, const McConfig& cfg) noexcept {
    Rng rng(cfg.seed);
    const double drift = (in.rate - 0.5 * in.vol * in.vol) * in.time;
    const double diffusion = in.vol * std::sqrt(in.time);
    const double disc = std::exp(-in.rate * in.time);

    // Two accumulators, because the control variate needs the covariance between
    // the payoff and the control, which needs both means first. Storing every
    // sample would be O(paths) memory for no benefit, so this collects the sums
    // needed for a single-pass covariance instead.
    double sum_y = 0.0, sum_y2 = 0.0;
    double sum_x = 0.0, sum_xy = 0.0, sum_x2 = 0.0;
    std::uint64_t n = 0;

    const std::uint64_t draws = cfg.antithetic ? (cfg.paths + 1) / 2 : cfg.paths;
    for (std::uint64_t i = 0; i < draws; ++i) {
        double z0, z1;
        rng.next_normals(z0, z1);
        // With antithetic sampling the pair is (z, -z); without it, Box-Muller's
        // two independent values are both used so no randomness is wasted.
        const double a = z0;
        const double b = cfg.antithetic ? -z0 : z1;

        for (const double z : {a, b}) {
            const double st = in.spot * std::exp(drift + diffusion * z);
            const double y = disc * payoff(st, in.strike, type);
            const double x = disc * st;  // control: expectation is spot exactly
            sum_y += y;
            sum_y2 += y * y;
            sum_x += x;
            sum_xy += x * y;
            sum_x2 += x * x;
            ++n;
            if (n >= cfg.paths) break;
        }
        if (n >= cfg.paths) break;
    }

    const double nd = static_cast<double>(n);
    double mean_y = sum_y / nd;
    double var_y = std::max((sum_y2 - nd * mean_y * mean_y) / (nd - 1.0), 0.0);

    if (cfg.control_variate && n > 2) {
        // E[disc * S_T] = spot under the risk-neutral measure. The sampling
        // error in the control is therefore known exactly, and the optimal
        // coefficient is the regression slope of payoff on control.
        const double mean_x = sum_x / nd;
        const double cov = (sum_xy - nd * mean_x * mean_y) / (nd - 1.0);
        const double var_x = std::max((sum_x2 - nd * mean_x * mean_x) / (nd - 1.0), 0.0);
        if (var_x > 0.0) {
            const double beta = cov / var_x;
            mean_y -= beta * (mean_x - in.spot);
            // Residual variance after regressing out the control.
            var_y = std::max(var_y - beta * beta * var_x, 0.0);
        }
    }

    McResult r;
    r.price = mean_y;
    r.std_error = std::sqrt(var_y / nd);
    r.paths = n;
    return r;
}

McResult price_asian_call(const Inputs& in, int steps, const McConfig& cfg) noexcept {
    Rng rng(cfg.seed);
    const double dt = in.time / static_cast<double>(steps);
    const double drift = (in.rate - 0.5 * in.vol * in.vol) * dt;
    const double diffusion = in.vol * std::sqrt(dt);
    const double disc = std::exp(-in.rate * in.time);

    double sum = 0.0, sum2 = 0.0;
    std::uint64_t n = 0;
    const std::uint64_t draws = cfg.antithetic ? (cfg.paths + 1) / 2 : cfg.paths;
    std::vector<double> zs(static_cast<std::size_t>(steps));

    for (std::uint64_t i = 0; i < draws && n < cfg.paths; ++i) {
        for (int s = 0; s < steps; s += 2) {
            double z0, z1;
            rng.next_normals(z0, z1);
            zs[static_cast<std::size_t>(s)] = z0;
            if (s + 1 < steps) zs[static_cast<std::size_t>(s + 1)] = z1;
        }
        // The antithetic partner negates the whole path, not each step
        // independently -- otherwise it is simply a different random path and
        // the negative correlation that makes the technique work is lost.
        for (const double sign : {1.0, -1.0}) {
            if (!cfg.antithetic && sign < 0.0) break;
            double s_t = in.spot;
            double avg = 0.0;
            for (int k = 0; k < steps; ++k) {
                s_t *= std::exp(drift + diffusion * sign * zs[static_cast<std::size_t>(k)]);
                avg += s_t;
            }
            avg /= static_cast<double>(steps);
            const double y = disc * std::max(avg - in.strike, 0.0);
            sum += y;
            sum2 += y * y;
            ++n;
            if (n >= cfg.paths) break;
        }
    }

    const double nd = static_cast<double>(n);
    const double mean = sum / nd;
    const double var = std::max((sum2 - nd * mean * mean) / (nd - 1.0), 0.0);
    McResult r;
    r.price = mean;
    r.std_error = std::sqrt(var / nd);
    r.paths = n;
    return r;
}

}  // namespace quant
