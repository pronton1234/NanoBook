# NanoBook

**Live demo:** https://pronton1234.github.io/NanoBook/
**Source:** https://github.com/pronton1234/NanoBook

This document has two halves. **Part One** explains what this is to anyone,
assuming no knowledge of markets or networking. **Part Two** is the technical
account, for someone who will read the code.

---
---

# Part One — For anyone

## What an exchange actually sends you

A stock exchange keeps a list of everyone who wants to buy and everyone who wants
to sell: at what price, in what quantity. That list is called the **order book**,
and it changes thousands of times per second.

To trade, you need to know what that list looks like *right now*. So the exchange
broadcasts every single change as a stream of network packets, and your job is:

```
packets arrive  →  rebuild the list  →  decide  →  send an order
```

That is the entire loop. This project is the machine that does it.

## Why that is harder than it sounds

**Everyone receives the same packets at the same moment.** The exchange tells you
nothing it is not telling every other participant simultaneously. So the only
advantage available is *who finishes processing first*. If a competitor rebuilds
the book in 100 billionths of a second and you take 200, they are acting on
information you are still reading.

**The network is unreliable, by design.** These packets are sent using a protocol
that makes no delivery guarantees. Packets go missing. They arrive in the wrong
order. Exchanges send everything **twice**, down two physically separate network
paths, precisely because they expect losses. Your software has to notice a gap,
take whichever copy arrives first, throw away the duplicate, and never lose a
message — all in billionths of a second.

**Nothing tells you when you are wrong.** There is no answer key. If your version
of the book is wrong, it still looks like a book. You simply begin trading
against a market that does not exist.

## What was built

A complete path from raw network traffic to an outgoing order, in six stages:

| stage | what it does |
|---|---|
| receive | unwrap the network packet |
| sequence | detect losses, discard the duplicate copy |
| parse | decode the exchange's compressed binary format |
| book | rebuild who wants to buy and sell at what price |
| decide | a deliberately trivial placeholder for a trading strategy |
| encode | write an order back onto the network |

**110 nanoseconds** end to end. For scale: light travels about 33 metres in that
time.

It has been run against **40 million real messages** from a real exchange with
zero decoding errors and zero corrupted books, and against one real trading
session per year from 2017 to 2026 — the demo lets you pick a year and watch it.

## Why this is worth a second look

### The part almost everyone skips

Search for "order book project" and you will find many. Nearly all of them open a
*file* of clean, correctly-ordered messages and parse it. That is the easy
version, with the network's misbehaviour deleted from the problem.

This one handles the real thing. And building it exposed two bugs that were
completely invisible on clean data and would have been fatal in production: under
2% packet loss, the system was **silently discarding 73% of messages**. It looked
perfectly healthy right up until it wasn't.

### Not trusting your own instruments

Anyone can build a set of scales. The hard part is knowing your scales are
accurate — because if they are not, every measurement you take is wrong and *the
numbers still look completely reasonable*.

Eight times during this project, the code ran, produced a number, and the number
was wrong in a way that looked entirely plausible:

| what it reported | what was really happening |
|---|---|
| a stage taking **negative time** | caught *only* because negative time is impossible. An inflated but positive number would have been published as a finding |
| a **38% speed improvement** | the laptop was simply busier during one test than the other |
| **"zero corrupted books"** | the check ran after the market had closed, when the book was empty. It measured nothing, cleanly |
| a **99th-percentile latency of 512** | that was the measuring tool's own internal bucket size, not a measurement |

Every one of those is a number that would have gone on a CV.

That is the actual skill on display. In trading, a simulation showing you would
have made money is worthless if the measurement is broken — and firms lose real
money to exactly this. Two failed experiments remain in the codebase, alongside
the measurements that killed them, rather than being quietly deleted.

## Try it

The demo runs the **real system** on a server and streams back real results.

- **Break the network on purpose.** Throw away packets and watch it detect the
  gaps and recover. Send every message twice and watch it use the first copy.
- **Overload it.** Push past 100% capacity and watch the honest latency
  measurement explode while the flattering one does not move at all. That
  contrast is the single most important thing on the page.
- **Check it against reality.** Pick any year from 2017 to 2026 and watch it
  decode a genuine trading session, showing the real ticker symbols and prices
  from that morning.

## In one sentence

> It is the piece of a trading system that turns raw exchange network traffic
> into a correct picture of the market in about 110 billionths of a second —
> including the messy parts most people skip — and the interesting part is the
> eight times it caught its own measurements lying.

---
---

# Part Two — For engineers

## Layout

Six Rust crates, layered strictly, plus a C++ tier and a Python tier. Each layer
depends only on those beneath it: a decoder must not know about a book, and a
book must not know about a strategy.

```
  server      HTTP/SSE, hand-rolled on TcpListener        runs the pipeline on demand
  pipeline    six stages, latency budget, SPSC, OUCH      composition and measurement
  book │ synth   price → size, and a capture generator    domain
  deep        DEEP 1.0 messages, fixed-point, no alloc    decode
  iextp       pcap/pcapng, IEX-TP, gaps, A/B arbitration  transport
```

**One external dependency across the whole Rust workspace** (`thiserror`), and
zero in C++. The HDR-style histogram, the lock-free ring buffer, the HTTP server
and both capture readers are written here, because each is small enough that a
dependency would cost more in opacity than it saves in lines.

| language | role | why |
|---|---|---|
| Rust | the wire: framing, sequencing, decoding, the book | nanoseconds and memory safety both matter |
| C++20 | a second independent implementation, plus pricing | the language most of this industry runs on |
| Python | statistics and inference | iteration speed beats hand-rolling a regression |

## Transport — `iextp`

The layer most order-book projects omit entirely.

**Two capture formats, sniffed by magic bytes, not by file extension.** IEX ships
both classic pcap and pcapng under the same `.pcap` name. A classic-pcap reader
does not *fail* on a pcapng file — it walks a 24-byte header and 16-byte record
headers over block structure and emits plausible garbage. An independent Python
decoder did exactly that here, reporting price-level updates as 2.8% of traffic
when the true figure is 97%.

**Every IEX frame carries an 802.1Q VLAN tag.** A parser assuming IPv4 begins at
the usual offset 14 lands four bytes early, inside the VLAN header, and misparses
every packet in the file. Not in any specification; found by reading bytes.

**A/B arbitration is not a separate algorithm.** Both feeds carry identical
sequence numbers, so it is deduplication by sequence over the merged stream. Feed
both into one `Sequencer` and the winner is whichever reached the socket first,
rejected by the same code path that rejects a retransmit. No A/B-specific branch,
no clock comparison, so the two paths cannot disagree.

**A gap is not immediately a loss.** UDP reorders, so out-of-order segments are
buffered rather than declared lost; declaring loss on first sight would fire
constantly on a healthy feed.

## Decode — `deep`

DEEP is an aggregated-depth feed: it publishes total displayed size per price
level rather than individual orders, so the book is a price→size map with no
order-id graph.

**Message lengths were measured, not transcribed.** A histogram of (type, length)
over 40,223,554 messages shows exactly one length per type, no exceptions. The
parser enforces it: a *known* type at an unexpected length is an error, because a
fixed-width format that is suddenly not fixed-width means the framing above it is
wrong. An *unknown* type is reported, never dropped.

**No floating point touches a price.** `Price` is an `i64` in units of 1/10000
dollar and offers no `f64` conversion at all, so a price cannot round-trip
through a float by accident — not even to be printed, since a formatting helper
is exactly where such a conversion creeps back into arithmetic later.

**The hot path shapes the types.** 97% of traffic is price-level updates, so that
arm branches first, and the 80-byte auction message is *borrowed* rather than
inlined to keep the message enum inside a cache line. A test pins that size.

## The book — `book`

The container sits behind a trait because choosing it was an experiment, not a
decision. Four implementations were built and measured.

Measuring the book first **killed the textbook answer**. The standard advice is a
flat array indexed by price tick with a bitmap to find the touch. The real book,
sampled across 6,530 symbols on a full session:

```
  levels per side     p50 2     p90 5     p99 14    max 52
  price span (ticks)  p50 480   p90 1713  p99 4907
```

Shallow *and* sparse. That array would reserve ~2 KB to hold 24 bytes and walk
cache lines of zeros on every lookup — losing by two orders of magnitude on the
axis it was chosen for.

The bake-off, 5M real updates, best of five:

| container | ns/update | vs baseline |
|---|---|---|
| *floor: no container at all* | *12.4* | — |
| `BTreeMap` | 59.6 | 1.00× |
| **sorted structure-of-arrays** | **49.6** | **1.20×** |
| levels inlined into the struct | 51.9 | 1.15× |
| arena-allocated | 103.9 | 0.57× |

All four produce byte-identical books. **The simplest structure won, and three
attempts to beat it failed** — each kept in the tree with the measurement that
killed it.

Linear scan rather than binary search, because at five elements a binary search
is two or three unpredictable branches and a linear scan is five perfectly
predicted ones. Binary search wins asymptotically and loses at the size that
occurs.

The arena's failure is the instructive one. It lost by 2×, worse than the
baseline. Sweeping the slot size separated two causes: eight slots to hold a
median of two levels is ~75% padding, inflating 400 KB of live data into a 2.3 MB
working set, and halving the slot recovers a third of the loss. But even at its
best it was 42% slower, because it **de-localised what the `HashMap` had already
co-located** — one hash entry holds both sides' headers and the event flag in a
single cache line, while the arena spread a symbol across four independently
indexed arrays. *Contiguity is not locality.*

**The crossed-book invariant is gated on the feed's event-complete flag.** IEX
splits one logical book event across several messages and is legitimately crossed
in between, so asserting on every message fires constantly on a healthy feed —
and the natural response, relaxing the assert, discards the one invariant that
catches real corruption. Locked (bid == ask) is counted separately from crossed
(bid > ask), so a real crossing cannot hide among benign locks.

## Pipeline and measurement — `pipeline`

**Stages are timed by cumulative prefix, never by clocking each one.**
`Instant::now()` costs ~28 ns here and the whole pipeline is ~110 ns, so six
clock reads per message would report a budget dominated by the act of measuring
it. Each stage is timed by running the pipeline *up to* that stage and
differencing; a test asserts every prefix does strictly less work than the next,
since differencing is meaningless otherwise.

Measured live on the deployed host:

| stage | cost | spread |
|---|---|---|
| receive | 10.2 ns | 2.0 |
| sequence | 8.8 ns | 2.0 |
| parse | 26.7 ns | 4.4 |
| **book update** | **79.4 ns** | 7.1 |
| decide | 5.2 ns | 3.1 |
| encode | 13.0 ns | 20.7 ⚠ unresolved |

Any stage whose cost falls below its own run-to-run spread is **flagged, not
quoted**. A positive number under the noise floor is still not a measurement.

### Coordinated omission

Most benchmarks measure *service time* — how long an item took once work began.
When the system stalls, items queued behind the stall are never measured, so the
stall vanishes from the results.

Driving the pipeline at three loads:

| offered load | service p50 | corrected p50 |
|---|---|---|
| 50% | 84 ns | 125 ns |
| 90% | 84 ns | 5.42 µs |
| 110% | **84 ns** | **14.97 ms** |

Service time is **identical at every load**. A metric that does not move when the
system is driven past capacity is not measuring behaviour under load. The
corrected figure measures from each item's *scheduled* arrival and moves five
orders of magnitude.

### Concurrency

A lock-free SPSC ring buffer: cache-line-padded indices, each side caching its
view of the other so the steady-state path touches only lines it already owns,
Release/Acquire on the publish.

Developed on ARM deliberately. `Relaxed` there would usually still pass on x86,
because TSO forbids the store-store reordering that breaks it; ARM's weak model
permits it, so an ordering bug has somewhere to fail rather than lying dormant.
**Model-checked under `loom`**, which enumerates the interleavings the memory
model permits rather than sampling those that happen to occur — verifying both
that every item crosses exactly once in order, and that a pop never observes a
slot whose value has not been published.

**Threading the pipeline is indistinguishable from single-threaded.** Successive
runs put it 7% ahead and 1% behind, both inside a run-to-run spread larger than
the pipeline itself. What *does* resolve is why: the producer blocks on a full
queue ~4M times while the consumer starves ~9k times, so the split relocates the
bottleneck rather than removing it.

## Cross-language verification — `cpp/`

An independent C++20 implementation of the same decode and book path. Both read
the same file and must agree: 351,903 messages, 33 levels, 35,500 total size,
zero crossed books, from either. **Two decoders in different languages agreeing
is a stronger correctness argument than one passing its own tests.**

The first C++ run measured 233 ns/packet against Rust's 140. The cause was not
the language. `std::unordered_map` is *required by the standard* to be node-based
so references survive rehash, making every lookup a pointer chase, while Rust's
`HashMap` is an open-addressed SwissTable. An open-addressed map took C++ to
146 ns, inside the spread of Rust's 162.

**The gap was a data structure, not a language** — and one workload on one
machine with two compilers cannot settle more than that, which the code says
rather than letting the number imply it.

## Pricing — `cpp/include/quant/`

Black-Scholes with all five Greeks as closed-form derivatives (not bumped
prices), implied volatility by Newton with a bisection fallback, and Monte Carlo
with antithetic and control variates.

The tests are the substance:

- **Put-call parity to 1e-10.** Model-free — it follows from arbitrage, not from
  Black-Scholes — so it catches a sign error anywhere in either branch.
- Every analytic Greek matched against a central finite difference.
- The Monte Carlo **95% confidence interval contains the analytic price**, rather
  than merely being close to it, and the standard error falls as 1/√N. That is
  what licenses trusting the engine on the Asian option, which has no closed
  form.

One finding worth stating: **implied volatility is not identified** for a deep
in-the-money option at low volatility. At S=125, K=100, σ=5%, vega is 4e-6, so a
price matched to 1e-8 still leaves the volatility uncertain in the third decimal —
many volatilities produce that price. The solver returns its conditioning
alongside the answer and flags it. Reporting a number there would be the same
error as a histogram reporting its bucket count as a p99.

## Statistics — `python/`

Order flow imbalance (Cont, Kukanov & Stoikov) regressed on next-bucket returns,
with OLS and **Newey-West standard errors written out rather than imported**.
Textbook OLS errors assume independent, homoskedastic residuals; high-frequency
data has neither, and the bias is *downward* — which makes insignificant results
look significant. The same failure mode as every measurement bug in this project.

Both controls are reported, because a null result alone is uninterpretable:

| control | result |
|---|---|
| **positive** — data built with β = 0.002 | recovered **0.0020002**, 95% CI contains truth |
| **negative** — no signal exists | **t = −1.65**, not significant, R² = 0.006 |

An estimator that finds nothing everywhere finds nothing here too. The positive
control is what makes the null mean anything.

Rust owns the wire and exports book state to CSV; Python owns the inference.
Re-implementing the decoder in Python would mean two decoders that can silently
disagree — the exact hazard this project keeps finding.

## The eight measurement bugs

None crashed. Every one produced a plausible, quotable number.

1. **Negative stage cost (−239 ns).** The first prefix to build a book paid every
   page fault for ~6,000 symbol entries while later prefixes reused warm pages.
   Visible only because negative time is impossible.
2. **A 38% threading speedup that did not exist.** Runs timed in blocks, so load
   drift landed entirely on whichever went first. Taking the minimum does not
   help: the minimum over a busy window is worse than over a quiet one.
3. **"Zero crossed books" on an empty book.** The audit ran after the close, when
   the exchange had deleted every level.
4. **Percentiles pinned to a histogram's bucket count.** "p99 = 512" was a fact
   about the histogram, not the market.
5. **A probe that never touched the memory it timed.** `len()` reads a field
   stored inline; the cache misses it was supposed to measure were charged to
   whatever was under test.
6. **A benchmark floor that excluded work the subjects paid for.** The
   no-container baseline skipped the invariant check's memory traffic.
7. **`bucket_low` was not the inverse of `index`.** Every latency quantile in the
   project would have been off by up to a bucket. Caught by a round-trip test.
8. **A `0x80` bit mask that should have been `0x01`.** Synthetic tests written
   from the same wrong belief passed happily; real data reported that 100% of
   book events were incomplete, which no functioning market produces.

## Verification

- **109 Rust tests** + 2 `loom` model checks, **158 C++ checks**, **11 Python
  tests**, all in CI on every push, on x86 as well as the ARM it is developed on.
- Ten real trading sessions, one per year 2017–2026, decoded live by the demo.
- A synthetic capture generator, so the repository demonstrates itself without a
  multi-gigabyte download — and so the decoder is checked by round trip, not only
  against a second decoder that might share its misunderstandings.

## What is deliberately not done

- **Kernel bypass (AF_XDP).** Needs Linux, a recent kernel, and control of boot
  parameters. The one item hardware cannot be argued out of.
- **`perf` counters.** The arena's cache-miss explanation is a well-supported
  inference, not a measurement, and it is labelled that way.
- **A matching engine and order types.** The book is a consumer of market data,
  not an exchange.
- **FIX.** OUCH-style order encoding exists; FIX does not.
- The OUCH encoder is the **one wire format here not verified against a capture**,
  because IEX publishes market data but not order entry. Its module says so
  rather than letting it sit alongside formats checked against 40M real messages.

---

*Market data provided for free by [IEX](https://iextrading.com). By accessing or
using IEX Historical Data you agree to the IEX Historical Data Terms of Use.*
