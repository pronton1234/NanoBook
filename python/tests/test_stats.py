"""Tests for the regression machinery.

The interesting assertions are not "does it return numbers" but the properties
an OLS/HAC implementation must satisfy: it recovers a known coefficient, its
HAC standard errors exceed the naive ones when residuals are autocorrelated
(that being the entire reason to use them), and it reports no signal when there
is none.
"""

import numpy as np
import pytest

from analysis.stats import newey_west_lags, ols_hac


def test_recovers_a_known_coefficient():
    rng = np.random.default_rng(1)
    n = 5000
    x = rng.normal(size=n)
    y = 3.0 + 2.5 * x + rng.normal(scale=0.1, size=n)
    fit = ols_hac(y, np.column_stack([np.ones(n), x]), ["c", "x"])
    assert fit.beta[0] == pytest.approx(3.0, abs=0.01)
    assert fit.beta[1] == pytest.approx(2.5, abs=0.01)
    assert fit.r_squared > 0.99


def test_finds_nothing_when_there_is_nothing():
    """Pure noise must not produce a significant coefficient.

    Run over many seeds because a single draw is uninformative: at the 5% level
    roughly one in twenty should cross the threshold by chance, and a test that
    demanded zero would be wrong about statistics rather than about the code.
    """
    hits = 0
    trials = 200
    for seed in range(trials):
        rng = np.random.default_rng(seed)
        n = 800
        x = rng.normal(size=n)
        y = rng.normal(size=n)
        fit = ols_hac(y, np.column_stack([np.ones(n), x]), ["c", "x"])
        if abs(fit.tstat[1]) > 1.96:
            hits += 1
    # Expect ~5%. Allow up to 12% before calling the estimator broken.
    assert hits / trials < 0.12, f"false positive rate {hits/trials:.1%} is too high"


def test_hac_errors_exceed_naive_when_residuals_autocorrelate():
    """The whole reason Newey-West exists.

    With autocorrelated residuals the naive OLS standard error is biased
    downward, so a t-statistic computed from it overstates significance. HAC
    must be larger here, or it is not doing its job.
    """
    rng = np.random.default_rng(3)
    n = 4000
    x = rng.normal(size=n)

    # AR(1) residuals: each carries most of the previous one.
    e = np.zeros(n)
    innov = rng.normal(scale=0.3, size=n)
    for i in range(1, n):
        e[i] = 0.9 * e[i - 1] + innov[i]
    y = 1.0 + 0.5 * x + e

    X = np.column_stack([np.ones(n), x])
    fit = ols_hac(y, X, ["c", "x"])

    # Naive OLS standard error, assuming i.i.d. homoskedastic residuals.
    resid = y - X @ fit.beta
    s2 = float(resid @ resid) / (n - 2)
    naive = np.sqrt(np.diag(s2 * np.linalg.pinv(X.T @ X)))

    assert fit.stderr[0] > naive[0], "HAC must widen the intercept error"


def test_lag_rule_grows_slowly():
    assert newey_west_lags(100) >= 1
    assert newey_west_lags(100_000) > newey_west_lags(1_000)
    # Sub-linear: the bandwidth must not grow with n or the estimate degenerates.
    assert newey_west_lags(1_000_000) < 1_000


def test_rejects_underdetermined_systems():
    with pytest.raises(ValueError):
        ols_hac(np.ones(2), np.ones((2, 5)), ["a", "b", "c", "d", "e"])
