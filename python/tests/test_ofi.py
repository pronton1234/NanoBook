"""Tests for the order-flow-imbalance construction.

OFI is easy to write in a way that looks right and silently measures something
else. These pin the cases where a naive size-difference would disagree with the
definition.
"""

import numpy as np

from analysis.ofi import Quotes, order_flow_imbalance


def q(bid, bsz, ask, asz):
    n = len(bid)
    return Quotes(
        ts=np.arange(n, dtype=np.int64),
        bid=np.array(bid, dtype=float),
        bid_size=np.array(bsz, dtype=float),
        ask=np.array(ask, dtype=float),
        ask_size=np.array(asz, dtype=float),
    )


def test_a_lifted_bid_is_positive_pressure():
    """The bid improves: the whole new size counts as buying pressure."""
    e = order_flow_imbalance(q([100, 101], [500, 300], [102, 102], [400, 400]))
    assert e[0] > 0


def test_a_pulled_bid_is_negative_pressure():
    """The bid is cancelled downward: the old size counts negatively."""
    e = order_flow_imbalance(q([101, 100], [500, 300], [102, 102], [400, 400]))
    assert e[0] < 0


def test_a_lowered_ask_is_negative_pressure():
    """Someone is willing to sell cheaper -- selling pressure."""
    e = order_flow_imbalance(q([100, 100], [500, 500], [102, 101], [400, 300]))
    assert e[0] < 0


def test_size_only_change_at_the_same_price():
    """At an unchanged bid price, adding size is positive and pulling is
    negative. A naive implementation that only looks at prices sees nothing."""
    add = order_flow_imbalance(q([100, 100], [500, 900], [102, 102], [400, 400]))
    pull = order_flow_imbalance(q([100, 100], [900, 500], [102, 102], [400, 400]))
    assert add[0] > 0
    assert pull[0] < 0
    assert add[0] == -pull[0]


def test_symmetry_between_sides():
    """A bid improvement and an equal-and-opposite ask improvement must produce
    OFI of the same magnitude and opposite sign."""
    up = order_flow_imbalance(q([100, 101], [500, 500], [102, 102], [500, 500]))
    down = order_flow_imbalance(q([100, 100], [500, 500], [102, 101], [500, 500]))
    assert up[0] == -down[0]


def test_length_is_one_less_than_input():
    e = order_flow_imbalance(q([1, 2, 3, 4], [1, 1, 1, 1], [5, 6, 7, 8], [1, 1, 1, 1]))
    assert len(e) == 3
