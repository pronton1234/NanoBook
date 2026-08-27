"""Ordinary least squares with heteroskedasticity- and autocorrelation-consistent
standard errors.

Written out rather than imported from statsmodels, because the point of this
file is to show the inference is understood, not that a library exists. It is
about eighty lines and every one of them is a decision.

Why HAC and not plain OLS standard errors
-----------------------------------------
Textbook OLS standard errors assume the residuals are independent and share a
variance. Neither holds for high-frequency market data: volatility clusters, so
the variance moves, and consecutive observations overlap in time, so the
residuals are autocorrelated. Under those conditions the plain standard error is
biased **downward** — often by a large factor — which makes a t-statistic look
significant when it is not.

That is the same failure mode as every measurement bug in this repository: a
number that is plausible, quotable, and wrong. Newey-West corrects it by
estimating the long-run variance with a weighted sum of autocovariances.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np


@dataclass
class Regression:
    """A fitted linear model and everything needed to judge it."""

    names: list[str]
    beta: np.ndarray
    stderr: np.ndarray
    tstat: np.ndarray
    r_squared: float
    n: int
    lags: int

    def summary(self) -> str:
        w = max(len(n) for n in self.names) + 2
        out = [
            f"  {'term':<{w}}{'coef':>14}{'HAC s.e.':>14}{'t':>10}{'':>6}",
            f"  {'-' * (w + 44)}",
        ]
        for i, name in enumerate(self.names):
            t = self.tstat[i]
            # 1.96 is the two-sided 5% critical value for a normal. With n in the
            # tens of thousands the t distribution is indistinguishable from it.
            mark = "significant" if abs(t) > 1.96 else ""
            out.append(
                f"  {name:<{w}}{self.beta[i]:>14.6g}{self.stderr[i]:>14.6g}"
                f"{t:>10.2f}  {mark}"
            )
        out.append("")
        out.append(f"  R² = {self.r_squared:.6f}   n = {self.n:,}   HAC lags = {self.lags}")
        return "\n".join(out)


def newey_west_lags(n: int) -> int:
    """Automatic bandwidth, 4·(n/100)^(2/9).

    The rule of thumb from Newey and West. Too few lags and the autocorrelation
    is not absorbed, leaving the standard error too small; too many and the
    estimate becomes noisy. Choosing by hand invites choosing whatever makes the
    result significant.
    """
    return max(1, int(np.floor(4.0 * (n / 100.0) ** (2.0 / 9.0))))


def ols_hac(y: np.ndarray, X: np.ndarray, names: list[str], lags: int | None = None) -> Regression:
    """Fit y = Xβ + ε with Newey-West standard errors.

    `X` must already contain an intercept column if one is wanted; this does not
    add one silently, because a regression through the origin is sometimes what
    you mean and a hidden intercept would change the answer without saying so.
    """
    y = np.asarray(y, dtype=np.float64).ravel()
    X = np.asarray(X, dtype=np.float64)
    if X.ndim == 1:
        X = X[:, None]
    n, k = X.shape
    if n <= k:
        raise ValueError(f"need more observations than parameters: n={n}, k={k}")

    # lstsq rather than inverting X'X: forming the normal equations squares the
    # condition number, and these regressors are not well scaled.
    beta, *_ = np.linalg.lstsq(X, y, rcond=None)
    resid = y - X @ beta

    XtX_inv = np.linalg.pinv(X.T @ X)
    if lags is None:
        lags = newey_west_lags(n)

    # Newey-West meat: S = Γ₀ + Σ w_j (Γ_j + Γ_j'), with Bartlett weights that
    # decay to zero at the bandwidth. The weights are what guarantee the estimate
    # stays positive semi-definite; an unweighted truncation does not.
    u = X * resid[:, None]
    S = u.T @ u
    for j in range(1, lags + 1):
        w = 1.0 - j / (lags + 1.0)
        G = u[j:].T @ u[:-j]
        S += w * (G + G.T)

    cov = XtX_inv @ S @ XtX_inv
    stderr = np.sqrt(np.maximum(np.diag(cov), 0.0))
    with np.errstate(divide="ignore", invalid="ignore"):
        tstat = np.where(stderr > 0, beta / stderr, 0.0)

    ss_res = float(resid @ resid)
    ss_tot = float(((y - y.mean()) ** 2).sum())
    r2 = 1.0 - ss_res / ss_tot if ss_tot > 0 else 0.0

    return Regression(names, beta, stderr, tstat, r2, n, lags)
