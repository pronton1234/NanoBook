# Latency Ladder

**The piece of a trading system that turns raw exchange network traffic into a
correct picture of the market in ~110 nanoseconds.**

[![CI](https://github.com/pronton1234/LatencyLadder/actions/workflows/ci.yml/badge.svg)](https://github.com/pronton1234/LatencyLadder/actions/workflows/ci.yml)

### ▶ [Run it live](https://pronton1234.github.io/LatencyLadder/)

**New here?** [PROJECT.md](PROJECT.md) explains this in two halves — plain
English for anyone, then a full technical account for someone who will read the
code.

---

## What problem this solves

An exchange keeps a list of everyone who wants to buy and sell, at what price, in
what size. That list is the **order book**, and it changes thousands of times a
second.

To trade, you need to know what that book looks like *right now*. So the exchange
broadcasts every change as a stream of network packets, and your job is:

```
packets arrive  →  rebuild the book  →  decide  →  send an order
```

That is the whole loop. It is harder than it sounds for three reasons.

**Everyone gets the same packets at the same time.** The exchange tells you
nothing it is not telling everyone else, simultaneously. The only edge available
is who finishes processing first. If a competitor rebuilds the book in 100
nanoseconds and you take 200, they act on information you are still parsing.

**The network lies to you.** These packets are sent by UDP, which makes no
promises. They get lost, arrive out of order, or arrive twice — exchanges send
everything twice down separate physical paths precisely because they expect it.
You have to notice a gap, take whichever copy arrives first, discard the
duplicate, and never drop a message, in nanoseconds.

**Nothing tells you when you are wrong.** There is no correct answer to check
against. If your book is wrong it still looks like a book. You simply start
trading against a market that does not exist.

## What this is

The machine that turns packets into a correct book, quickly, and **proves both
halves of that claim**.

| | |
|---|---|
| packet in to order encoded | **110.4 ns** |
| real exchange messages decoded, zero errors | **40,223,554** |
| corrupted books, across 38,993,905 updates | **0** |
| measurement bugs caught before publication | **8** |
| languages | Rust · C++20 · Python |
| tests | 109 Rust + 158 C++ + 11 Python |
| external dependencies | **1** |

```
receive    strip Ethernet / VLAN / IPv4 / UDP
sequence   detect gaps, request retransmits, pick the first of two feeds
parse      decode the exchange's binary format
book       rebuild who wants to buy and sell at what price
decide     a deliberately trivial placeholder, so it occupies its real cost
encode     write an order back onto the wire
```

## Three languages, each where it belongs

**Rust** owns the wire — framing, sequencing, decoding, the book — because that
is where nanoseconds and memory safety both matter.

**C++20** is a second, independent implementation of the same decode and book
path, plus the pricing library. Two implementations in different languages that
agree on every count is a far stronger correctness argument than one that passes
its own tests.

**Python** owns the statistics, because that is where iteration speed matters and
a hand-rolled regression would be a liability rather than a demonstration.
Re-implementing the decoder there would mean two decoders that can silently
disagree — exactly the hazard this project keeps finding.

### The cross-language comparison

Both read the same file and must produce identical counts. They do: 351,903
messages, 33 levels, 35,500 total size, zero crossed books, from either.

The first C++ run came in at **233 ns/packet against Rust's 140**. The cause was
not the language: `std::unordered_map` is *required by the standard* to be
node-based, so references stay valid across rehash and every lookup is a pointer
chase, while Rust's `HashMap` is an open-addressed SwissTable. Replacing it with
an open-addressed map took C++ to **146 ns**, inside the run-to-run spread of
Rust's 162 ns.

Which is the honest conclusion: **the gap was a data structure, not a language.**

### Derivatives pricing, from scratch

Black-Scholes with all five Greeks as closed-form derivatives, implied
volatility by Newton with a bisection fallback, and a Monte Carlo engine with
antithetic and control variates.

The tests are the point. Put-call parity holds to 1e-10 — a model-free identity
that follows from arbitrage alone. Every analytic Greek matches a central finite
difference. And the Monte Carlo **95% confidence interval contains the analytic
price**, rather than merely being near it; the standard error falls as 1/√N, so
the engine can be trusted on the Asian option that has no closed form.

One finding worth its own line. Implied volatility is **not identified** for a
deep in-the-money option at low volatility: at S=125, K=100, vol=5%, vega is
4e-6, so a price matched to 1e-8 still leaves the volatility uncertain in the
third decimal. Many volatilities produce that price. The solver returns the
conditioning alongside the answer and flags it, because reporting a number there
would be the same error as a histogram reporting its bucket count as a p99.

### Order flow imbalance, with controls

OFI regressed on next-bucket returns, using OLS with Newey-West standard errors
written out rather than imported — because textbook OLS errors assume
independent residuals, high-frequency data has neither, and the bias is
*downward*, which makes insignificant results look significant.

Run against synthetic data, so the honest expectation is a **null result**, and
both controls are reported:

| control | result |
|---|---|
| **positive** — data built with β = 0.002 | recovered **0.0020002**, 95% CI contains truth |
| **negative** — no signal exists | **t = −1.65**, not significant, R² = 0.006 |

An estimator that reports nothing everywhere reports nothing here too. The
positive control is what makes the null meaningful.

## Status

The transport layer, the DEEP decoder, the order book and the full tick-to-trade
pipeline are implemented, validated against real captures, and benchmarked, with
a C++ second implementation and a Python statistics tier alongside.

Not done, and labelled as such throughout: **kernel bypass** (AF_XDP, which needs
Linux and control of boot parameters), **`perf` counters** (so the arena's
cache-miss explanation remains a supported inference rather than a measurement),
a **matching engine and order types**, and **FIX**.

## What works today

`crates/iextp` — IEX-TP v1 transport, zero-copy throughout:

- **capture reading** in both formats IEX ships — classic pcap (all four magic
  variants: byte order × microsecond/nanosecond timestamps) and pcapng —
  detected by magic and presented as one iterator
- **frame stripping** — Ethernet, 802.1Q VLAN, IPv4, UDP
- **segment parsing** — the 40-byte IEX-TP header and its length-prefixed
  messages
- **sequencing** — gap detection, reorder buffering, duplicate suppression,
  session-reset handling
- **A/B feed arbitration**

`crates/deep` — DEEP 1.0 message decoding, no allocation, no floating point:

- **fixed-point prices** (`i64`, 1/10000 dollar) with no `f64` conversion offered
  at all, so a price cannot round-trip through a float by accident
- **8-byte inline symbols**, compared as a single `u64`
- **typed messages** with the 80-byte auction message borrowed rather than
  inlined, keeping the hot-path enum inside a cache line — pinned by a test
- **length validation** per type, since DEEP is fixed-width

`crates/book` — aggregated-depth book with the level container behind a trait,
so the R0/R1 bake-off is a swap rather than a rewrite:

- **`BTreeLevels`** (R0) — the ordered-map baseline. Correct, and the reference
  the fast implementation gets differentially tested against.
- **per-symbol books** keyed by a custom hasher — `HashMap`'s SipHash default is
  right for untrusted keys and pure overhead for exchange tickers
- **invariant checking** — crossed and locked books counted separately, gated on
  the feed's event-complete flag

### On the crossed-book invariant

IEX splits one logical book event across several messages, so the published book
is *legitimately* crossed in between. Bit 0 of the price level update flags is
the feed saying "I am mid-event, do not look yet".

A validator that asserts on every message therefore fires constantly on a healthy
feed, and the natural response — relaxing the assert — discards the one invariant
that catches real corruption. Gating on the flag keeps it strict exactly where it
means something.

There is a useful coupling in that: this is the same flag whose mask was wrong in
the decoder. Had it stayed wrong, every message would read as mid-event and the
crossed check would have **silently never run**. Two independent things had to be
right for the check to work at all.

### On A/B arbitration

Exchanges publish market data twice down separate paths, and a receiver takes
whichever copy arrives first. Both feeds carry *identical* sequence numbers,
which means:

> A/B arbitration is not a separate algorithm. It is deduplication by sequence
> number over the merged stream.

Feed both into one `Sequencer` and the winner is whichever reached the socket
first; the loser is rejected by the same code path that rejects a retransmit.
No A/B-specific branch, no clock comparison, so the two paths cannot disagree.

## Verification

Two independent implementations — a throwaway Python decoder written first, then
the Rust — agree exactly on the 2017 capture, including every per-type message
count:

```
packets   20,145      messages   48,635      sequence gaps   0
```

A full 2018 trading day, 4.8 GB:

```
packets  30,300,122   messages  40,223,554   gaps 0   duplicates 0   session resets 0
                                             7.1 M msg/s, 0.85 GB/s single-threaded
```

Which settles what the hot path is, and it is not close:

| message | share |
|---|---|
| Price Level Update (sell) | 48.80% |
| Price Level Update (buy) | 48.14% |
| Trade Report | 2.94% |
| everything else | 0.12% |

**97% of traffic is price-level updates.** The book's add/update/delete path is
effectively the whole cost of the system, which is what makes the data-structure
choice in R1 the entire engineering problem rather than a detail.

```bash
cargo test
cargo run --release --example replay -- data/raw/<capture>.pcap
```

### Traps the real data exposed

Each of these was found by running against a capture, not by reading a spec:

- **IEX ships two different formats under one `.pcap` extension.** The August
  2017 fixture is classic pcap; the 2018 capture is pcapng — and which format a
  given day carries is not predictable from its date, so it has to be sniffed. A classic-pcap reader does not fail
  on a pcapng file — it walks a 24-byte header and 16-byte record headers over
  block structure and emits plausible garbage. The Python decoder did exactly
  that, reporting price-level updates as 2.8% of traffic when the real figure is
  97%, and inventing dozens of nonexistent message types. The Rust refused the
  file instead: `not a pcap file: magic 0x0a0d0d0a`. Sniffing by magic rather
  than extension, and failing loudly on an unknown one, is the only reason the
  discrepancy was visible at all.
- **Every IEX frame carries an 802.1Q VLAN tag.** A parser assuming IPv4 begins
  at the usual offset 14 lands four bytes early, inside the VLAN header, and
  misparses every packet in the file.
- **pcapng timestamp resolution is per-interface**, from the `if_tsresol` option,
  defaulting to microseconds. Getting it wrong scales every timestamp by 1000
  with no error raised.
- **pcapng `linktype` is not known until iteration reaches the interface block.**
  Reporting `0` before then is not a harmless default: 0 is BSD loopback, so the
  wrong answer looks like a real one.
- **Byte order flips mid-packet.** Ethernet, IPv4 and UDP are big-endian;
  IEX-TP and everything below it is little-endian.
- **65% of segments in a quiet session are heartbeats** (`message_count == 0`).
  They must not advance the sequence — treating one as authoritative would let a
  quiet channel silently reset the stream.
- **Ethernet pads frames to 60 bytes.** The UDP length field, not the captured
  frame length, decides where the payload ends.
- **A gap is not immediately a loss.** UDP reorders, so out-of-order segments are
  buffered; declaring loss on first sight would fire constantly on a healthy
  feed.
- **A wrong bit mask that no unit test could catch.** The DEEP "event processing
  complete" flag is bit 0; I implemented bit 7. Nothing failed — the synthetic
  tests were built from the same wrong belief and passed happily. Real data
  showed it instantly, because the decoder claimed **100%** of updates were
  mid-event, and a market where no book event ever completes does not exist.
  It reads 36% on the fixture once corrected.

That last one is the argument for validating against captures rather than
fixtures. A test you write from your own reading of a spec proves the code
matches your belief, not that your belief matches the wire.

## Data

Captures are fetched, never committed — they run 0.7 MB to 12 GB per day, and
IEX retains proprietary rights in the data.

IEX publishes HIST DEEP/TOPS as pcap, free, on a T+1 basis. Because the files
contain the **actual UDP packets**, they can be replayed onto a real interface
with `tcpreplay` — which is what makes the R2 kernel-bypass measurement a genuine
network path rather than a simulation.

> Data provided for free by IEX. By accessing or using IEX Historical Data, you
> agree to the IEX Historical Data Terms of Use.

## Layout

```
crates/iextp/          transport: pcap → frame → segment → sequencing
  src/pcap.rs          classic pcap reader, borrowed
  src/frame.rs         Ethernet/VLAN/IPv4/UDP stripping
  src/segment.rs       IEX-TP v1 segment header and messages
  src/sequencer.rs     gaps, duplicates, A/B arbitration
  examples/replay.rs   run a real capture through the stack
```

## Building

Development is on an Apple M2 (arm64). Headline latency numbers will come from
x86 Linux, since AF_XDP, core pinning and huge pages are Linux-only and x86 is
what trading systems actually run on.

The M2 is not purely a compromise: ARM64 has a weak memory model, so lock-free
code that is subtly wrong often works under x86's TSO and breaks on ARM.
