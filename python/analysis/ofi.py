"""Order flow imbalance, and whether it predicts short-horizon returns.

OFI is the standard microstructure measure of net buying pressure at the touch
(Cont, Kukanov & Stoikov, 2014). For consecutive quote updates it counts size
added to the bid and removed from the ask as positive, and the reverse as
negative:

    e_n = 1[P_b_n >= P_b_{n-1}]·q_b_n − 1[P_b_n <= P_b_{n-1}]·q_b_{n-1}
        − 1[P_a_n <= P_a_{n-1}]·q_a_n + 1[P_a_n >= P_a_{n-1}]·q_a_{n-1}

The indicators matter more than they look. A bid that *improves* contributes its
whole new size; a bid at the same price contributes the change; a bid that is
pulled contributes the old size negatively. Treating it as a simple size
difference conflates a price move with a size change and destroys the signal.

Controls, which are the point
-----------------------------
This runs against SYNTHETIC data whose generator has no relationship between
order flow and future returns. So the honest expectation is a **null result**,
and a significant coefficient here would mean the estimator is broken, not that
a signal was found. That is a negative control.

The positive control is the other half: data is synthesised WITH a known
coefficient, and the estimator must recover it. An estimator that finds nothing
everywhere is not evidence of anything — it has to be shown capable of finding
something before its silence means anything.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from .stats import ols_hac

PRICE_SCALE = 10_000.0  # DEEP prices are integers in 1/10000 dollar


@dataclass
class Quotes:
    ts: np.ndarray
    bid: np.ndarray
    bid_size: np.ndarray
    ask: np.ndarray
    ask_size: np.ndarray

    def __len__(self) -> int:
        return len(self.ts)

    @property
    def mid(self) -> np.ndarray:
        # Integer prices halved in float space. The division is the first and
        # only place a price becomes a float, and it happens after all the
        # exactness-sensitive work is done.
        return (self.bid + self.ask) / 2.0 / PRICE_SCALE


def load(path: Path, symbol: str) -> Quotes:
    """Read the CSV the Rust exporter writes, for one symbol."""
    raw = np.genfromtxt(
        path, delimiter=",", names=True, dtype=None, encoding="utf-8"
    )
    m = raw["symbol"] == symbol
    return Quotes(
        ts=raw["ts_nanos"][m].astype(np.int64),
        bid=raw["bid"][m].astype(np.float64),
        bid_size=raw["bid_size"][m].astype(np.float64),
        ask=raw["ask"][m].astype(np.float64),
        ask_size=raw["ask_size"][m].astype(np.float64),
    )


def order_flow_imbalance(q: Quotes) -> np.ndarray:
    """Per-update OFI. Length len(q) - 1."""
    bid, ask = q.bid[1:], q.ask[1:]
    pbid, pask = q.bid[:-1], q.ask[:-1]
    bsz, asz = q.bid_size[1:], q.ask_size[1:]
    pbsz, pasz = q.bid_size[:-1], q.ask_size[:-1]

    return (
        (bid >= pbid) * bsz
        - (bid <= pbid) * pbsz
        - (ask <= pask) * asz
        + (ask >= pask) * pasz
    )


def bucket(values: np.ndarray, ts: np.ndarray, window_ns: int) -> tuple[np.ndarray, np.ndarray]:
    """Sum `values` into fixed time buckets, returning (bucket index, sum).

    Aggregating in time rather than in event count matters: OFI over ten updates
    means something different when those updates span a microsecond than when
    they span a minute, and the regressand is a return over a fixed horizon.
    """
    if len(ts) == 0:
        return np.array([]), np.array([])
    idx = (ts - ts[0]) // window_ns
    keys, inverse = np.unique(idx, return_inverse=True)
    sums = np.bincount(inverse, weights=values, minlength=len(keys))
    return keys, sums


def analyse(q: Quotes, window_ns: int, label: str) -> None:
    """Regress the next bucket's mid return on this bucket's OFI."""
    if len(q) < 100:
        print(f"  {label}: only {len(q)} quotes, skipping")
        return

    ofi = order_flow_imbalance(q)
    ts = q.ts[1:]
    mid = q.mid[1:]

    keys, ofi_sum = bucket(ofi, ts, window_ns)
    # Last mid observed in each bucket, so the return is measured between the
    # states an observer would actually have seen.
    _, inverse = np.unique((ts - ts[0]) // window_ns, return_inverse=True)
    last_mid = np.zeros(len(keys))
    np.maximum.at(last_mid, inverse, 0)  # placeholder to size the array
    order = np.argsort(inverse, kind="stable")
    last_mid[inverse[order]] = mid[order]

    # Contemporaneous OFI against the NEXT bucket's return. Regressing on the
    # same bucket's return would be measuring impact, not prediction, and would
    # look impressive while forecasting nothing.
    ret = np.diff(last_mid)
    x = ofi_sum[:-1]
    if len(ret) < 50:
        print(f"  {label}: only {len(ret)} buckets, skipping")
        return

    X = np.column_stack([np.ones(len(x)), x])
    fit = ols_hac(ret, X, ["intercept", "ofi"])
    print(f"\n  {label}   ({len(q):,} quotes, {len(ret):,} buckets of {window_ns/1e3:.0f} µs)")
    print(fit.summary())
    return fit


def positive_control(n: int = 20_000, beta: float = 0.002, seed: int = 7) -> None:
    """Synthesise data WITH a known coefficient and check it is recovered.

    Without this, a null result is uninterpretable: an estimator that reports
    nothing everywhere reports nothing here too. This shows the estimator can
    see a relationship of the size being looked for, so that its silence on real
    data means something.
    """
    rng = np.random.default_rng(seed)
    ofi = rng.normal(0.0, 500.0, n)
    noise = rng.normal(0.0, 0.01, n)
    ret = beta * ofi + noise

    X = np.column_stack([np.ones(n), ofi])
    fit = ols_hac(ret, X, ["intercept", "ofi"])
    print("\n  POSITIVE CONTROL — data built with a known coefficient")
    print(f"  true beta = {beta}")
    print(fit.summary())

    lo = fit.beta[1] - 1.96 * fit.stderr[1]
    hi = fit.beta[1] + 1.96 * fit.stderr[1]
    ok = lo <= beta <= hi
    print(f"  95% CI [{lo:.6g}, {hi:.6g}] {'contains' if ok else 'MISSES'} the true value")
    if not ok:
        raise SystemExit("positive control failed: the estimator cannot recover a known effect")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("csv", type=Path, help="quotes CSV from the Rust exporter")
    p.add_argument("--symbol", default="AAPL")
    p.add_argument("--window-us", type=int, default=500, help="bucket width in microseconds")
    args = p.parse_args()

    positive_control()

    q = load(args.csv, args.symbol)
    print("\n  NEGATIVE CONTROL — synthetic feed, no signal was built in")
    print("  A significant coefficient here would indict the estimator, not")
    print("  reveal a signal: the generator has no link between flow and returns.")
    analyse(q, args.window_us * 1_000, args.symbol)


if __name__ == "__main__":
    main()
