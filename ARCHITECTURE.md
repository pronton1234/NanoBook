# Architecture

Six crates, layered strictly. Each depends only on the ones below it, and the
dependency direction is the design: a decoder must not know about a book, and a
book must not know about a strategy.

```
                    ┌──────────────────────────────┐
   deployment  ->   │  server    HTTP/SSE, no deps │   runs the pipeline on demand
                    └───────────────┬──────────────┘
                    ┌───────────────▼──────────────┐
   composition ->   │  pipeline  six stages        │   budget · histogram · SPSC · OUCH
                    └───────┬───────────────┬──────┘
                    ┌───────▼──────┐ ┌──────▼───────┐
   domain      ->   │  book        │ │  synth       │   book: price -> size
                    └───────┬──────┘ └──────┬───────┘   synth: writes valid captures
                    ┌───────▼───────────────▼──────┐
   decode      ->   │  deep      DEEP 1.0 messages │   fixed-point, no allocation
                    └───────────────┬──────────────┘
                    ┌───────────────▼──────────────┐
   transport   ->   │  iextp     pcap/pcapng, IEX-TP│  gaps · retransmit · A/B
                    └──────────────────────────────┘
```

One external dependency in the whole workspace: `thiserror`. The HDR-style
histogram, the lock-free SPSC ring, the HTTP server, and both capture readers are
written here, because each is small enough that a dependency would cost more in
opacity than it saves in lines.

---

## How this got here

The repository began as something else, and the pivot is the most important
architectural decision in it.

### It was a trading strategy, and the strategy did not work

The original project tried to beat Kalshi's NBA prediction markets. Twenty-three
hypotheses were tested — taker, maker, arbitrage, cross-venue, factor, prop and
situational surfaces — and every one closed. The venue was internally consistent
to about a cent from three independent directions, including **zero violations in
807,794 cross-book comparisons**. Market price alone beat every model given every
feature.

The last result was the informative one: the book takes **30–45 seconds** to
absorb a scoring play, with real fills at 28% of the eventual move within half a
second. Genuinely slow — and still not tradeable, because the repricing was
smaller than the cost of crossing to reach it. Entry delay cost 0.22¢ per second,
so closing the gap would have required entering six seconds *before* the play
finished. No achievable speed was enough.

That is a real finding about a market. It is not a portfolio piece for a systems
role, and it stops being interesting the moment the answer is "no".

### What survived the pivot

Almost nothing, and saying so is more useful than pretending otherwise. The
research code, the models, and 32 architecture decision records were archived
rather than ported. What carried over was **discipline, not code**:

* **Never route money through a float.** [`deep::Price`](crates/deep/src/price.rs)
  is an `i64` in units of 1/10000 dollar and offers no `f64` conversion at all,
  so a price cannot round-trip through a float by accident.
* **A result that would be extraordinary is a bug report until audited.** A
  strategy showing 24.9% daily returns passed six robustness checks and was a
  double-counted spread term. What caught it was noticing the magnitude, not any
  test.
* **Report what failed.** Two container implementations that lost are in this
  tree with the measurements that killed them.

### The architecture that replaced it

The question changed from *"can I predict this market"* to *"how low can
tick-to-trade go, and what stops you"*. That is a systems question with an
objective answer, and it does not depend on a market cooperating.

The layering above follows from one measurement: **97% of feed traffic is
price-level updates.** Everything else is a rounding error, so the price-level
path is the first branch at every level, the auction message is borrowed rather
than inlined to keep the hot enum inside a cache line, and the book's container
choice is the whole engineering problem rather than a detail.

---

## Why each layer is shaped the way it is

### `iextp` — transport

The layer most order-book projects skip entirely. Market data is UDP multicast,
published twice down separate paths, and packets are lost and reordered.

**A/B arbitration is not a separate algorithm.** Both feeds carry identical
sequence numbers, so it is deduplication by sequence over the merged stream: feed
both into one `Sequencer` and the winner is whichever reached the socket first,
rejected by the same path that rejects a retransmit. No A/B branch, no clock
comparison, so the two paths cannot disagree.

Two capture formats, detected by magic rather than by extension, because IEX
ships both under `.pcap` and a classic-pcap reader does not fail on a pcapng
file — it emits plausible garbage.

### `deep` — decode

Fixed-width messages, one length per type, **measured over 40,223,554 messages**
rather than transcribed from a specification. A known type at an unexpected
length is an error, because a fixed-width format that is not fixed-width means
the framing above is wrong.

### `book` — the order book

DEEP is aggregated depth, so a side is price → size with no order-id graph. The
container is behind a trait ([`Levels`](crates/book/src/levels.rs)) because
choosing it was an experiment, not a decision.

Measuring the book first killed the textbook answer. A flat tick-indexed array
with a bitmap is the standard advice; the real book is **p50 = 2 levels spread
over 480 ticks**, so that array would reserve ~2KB to hold 24 bytes and walk
cache lines of zeros. A sorted structure-of-arrays with a linear scan won,
because at five elements the branch predictor beats the algorithm.

### `pipeline` — composition and measurement

Stages are timed by **cumulative prefix**, never by clocking each one:
`Instant::now()` costs ~28 ns and the whole pipeline is ~110 ns, so per-stage
clocks would report the act of measuring.

Coordinated omission is corrected, not ignored. Service time — how long an item
took once work began — is flat at ~90 ns whether the system is at 50% or 110% of
capacity, which is not a property any system has. Measured from each item's
scheduled arrival, p50 goes 125 ns → 5.42 µs → 14.97 ms.

### `synth` — generating captures

Exists so the repository demonstrates itself without a multi-gigabyte download,
and closes a hole in the correctness argument: everything else rested on two
independent *decoders* agreeing, which is blind to a misunderstanding they share.
Round-tripping through an encoder is a different check, and it fails instantly on
the bit-mask bug that previously took real market data to catch.

### `server` — the demo

Hand-rolled HTTP over `TcpListener`. Four fixed endpoints with bounded parameters
did not justify an async runtime and its dependency tree. It warms before
answering, because the first run on a cold host measures the host waking up, and
serialises benchmarks, because concurrent runs contend and a queue is better than
a wrong number.
