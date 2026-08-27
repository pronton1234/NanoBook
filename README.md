# Latency Ladder

**How low can tick-to-trade go, and what stops you at each rung?**

Most order-book projects open a file of already-de-framed messages and start
parsing. That skips everything the transport actually does: market data arrives
as UDP multicast, published twice down physically separate paths, with packets
reordered and dropped, and a receiver has to reconcile all of that before a
single book update is correct.

This builds the real receive path, then implements the same tick-to-trade
pipeline at three levels of the stack and measures each one.

| rung | what changes | status |
|---|---|---|
| **R0** naive | allocating, `BTreeMap`, blocking sockets | done |
| **R1** optimized | zero-copy, sorted structure-of-arrays levels | pipeline done |
| **R2** kernel bypass | AF_XDP, busy-poll, pinned cores, IRQ affinity | planned |

### The tick-to-trade path

Wire bytes in, order bytes out — receive, sequence, parse, book, decide, encode.
The strategy is deliberately trivial (join a wide, stable spread); it exists to
occupy its real place in the budget, since a measurement that stops at the book
update is timing a feed handler and calling it a trading system.

**110.4 ns per packet, 88.5 ns per message**, over 2M packets of a real session:
2,496,143 messages, 2,398,061 book updates, 2,065,378 orders encoded, 0 errors.

Stages are timed by **cumulative prefix** — run the pipeline up to each stage and
difference — because `Instant::now()` costs ~28 ns here and six clock reads per
message would report a budget dominated by the act of measuring it.

| stage | cumulative | this stage | noise |
|---|---|---|---|
| receive (eth/vlan/ip/udp) | 7.9 ns | 7.9 ns | 8.6 ns |
| sequence (IEX-TP, A/B) | 16.8 ns | 8.9 ns | 10.5 ns |
| parse (DEEP) | 37.4 ns | 20.6 ns | 15.2 ns |
| book update | 89.0 ns | 51.6 ns | 23.3 ns |
| decide (strategy) | 96.8 ns | 7.9 ns | 14.0 ns ⚠ |
| encode (OUCH order) | 110.4 ns | 13.6 ns | 13.4 ns |

The **noise column is not decoration**. It is the run-to-run spread, and any
stage whose cost falls below it is flagged rather than quoted — a positive
number below the noise floor is still not a measurement. macOS offers no core
pinning or CPU isolation, so the per-stage split needs the pinned x86 Linux box
(R2) before it is quotable. The total is a large aggregate and holds.

Getting here took two corrections. Timing the stages in sequence let thermal
drift accumulate along the stage order and produced a **negative** cost for
`decide` (−239 ns) — impossible, and the only reason the underlying bug was
visible: the first prefix to build a book was paying every page fault for ~6,000
symbol entries while later prefixes reused warm pages. An inflated but still
positive number would have been published as a finding. Now the full pipeline is
warmed once before any timing, and repetitions are round-robin across stages.

### Does splitting it across two cores help? No.

The natural split puts the wire stages on one thread and the book stages on
another, over a real lock-free SPSC ring — cache-line padded, with each side
caching its view of the other so the steady-state hot path touches only lines it
already owns. On paper it should win: the rate becomes the slower half (~73 ns)
rather than the sum (~117 ns).

| arrangement | ns/packet |
|---|---|
| single thread | ~117 |
| two threads (SPSC split) | ~117 |

**Indistinguishable, and the sign flips between runs** — successive measurements
put threading 7% ahead and then 1% behind, both inside the run-to-run spread. The
example refuses to name a winner unless the gap clears that spread, because a
benchmark announcing a result it cannot resolve is worse than one that says it
cannot.

What the run *does* resolve is why: the producer blocks on a full queue ~4M times
while the consumer starves ~9k times. The book side is saturated and the parse
thread spends its life waiting, so the split relocates the bottleneck rather than
removing it. Pipelining is arithmetic that works at millisecond granularity and
stops working at nanosecond granularity.

Both arrangements emit **identical book updates and identical orders**
(2,398,061 and 2,065,378). Matching books alone would not be enough — the split
moves the decision and encode stages to the other thread, so a reordering that
reached the same final book while quoting off a different intermediate state
would pass a book-only check.

### Coordinated omission, demonstrated rather than asserted

Most latency benchmarks measure *service time* — how long an item took once work
began — and call it latency. When the system stalls, the items queued behind the
stall are measured late or not at all, and the stall disappears.

Driving the pipeline at three loads makes the difference impossible to miss:

| offered load | behind schedule | service p50 | **corrected p50** |
|---|---|---|---|
| 50% of capacity | 26.1% | 84 ns | 125 ns |
| 90% of capacity | 79.2% | 84 ns | 5.42 µs |
| 110% of capacity | 99.5% | 84 ns | **14.97 ms** |

Service time is **identical at every load**. A benchmark reporting it would show
a pipeline equally fast at 50% and 110% of capacity — which is not a property any
system has. The corrected figure, measured from each item's *scheduled* arrival,
moves five orders of magnitude.

### The measurement changed the design

R1 was *going* to be a flat array indexed by price tick, with a bitmap to find
the touch — the standard answer for a hot order book. Measuring the book's actual
shape first killed it. Sampled every million messages across 6,530 symbols on a
full trading day:

| | p50 | p90 | p99 | max |
|---|---|---|---|---|
| levels per side | **2** | 5 | 14 | 52 |
| price span (¢ ticks) | **480** | 1,713 | 4,907 | 26,361,001 |

Books are shallow **and** sparse: the median side holds two levels spread over
480 ticks. A flat array would reserve ~2KB to store 24 bytes and walk cache lines
of zeros on every touch lookup — losing by two orders of magnitude on the axis it
was chosen for.

So R1 is a sorted structure-of-arrays instead, with a linear scan rather than a
binary search, because at five elements the branch predictor beats the algorithm.

### The bake-off, and two wrong guesses

5M real updates, 6,085 symbols, 40.7% deletes. Best of five runs (minimum, not
mean — the slow runs measure the machine's background noise, not the code):

| container | ns/update | vs R0 | container's share |
|---|---|---|---|
| *floor: no container at all* | *12.4* | — | — |
| `BTreeMap` (R0) | 59.6 | 1.00x | 47.2 ns (79%) |
| **sorted SoA (R1)** | **49.6** | **1.20x** | 37.2 ns (75%) |
| inline SoA (R1b) | 51.9 | 1.15x | 39.5 ns (76%) |
| arena (R1c) | 103.9 | 0.57x | 91.5 ns (88%) |

All four produce byte-identical books, so nothing is fast by being wrong.

**The simplest structure won, and three attempts to beat it failed.** Each is
kept in the tree as evidence rather than deleted:

1. *"The symbol hash lookup dominates."* No — the whole book path with no
   container at all is 12.4 ns, a fifth of the total.
2. *"Inlining the levels removes a cache miss."* It lost. Storing 8 levels in the
   struct grew `SymbolBook` from ~112 to ~208 bytes, doubling the hash table's
   backing array to ~1.27MB. The table's own misses cost more than the
   indirection removed.
3. *"An arena makes level storage contiguous."* It lost by **2x**, worse even than
   the `BTreeMap` baseline. Sweeping the slot size separates the two causes:

   | SLOT | ns/update |
   |---|---|
   | 2 | 79.5 |
   | 4 | 70.2 |
   | 8 | 103.9 |

   Footprint is part of it — eight slots to hold a median of two levels is ~75%
   padding, inflating ~400KB of live data into a ~2.3MB working set — and halving
   the slot recovers a third. Halving again does not, because at p50 = 2 a
   two-slot arena spills constantly.

   But even at its best the arena is 42% slower than a plain sorted `Vec`, so
   footprint is not the whole story. The rest is that it **de-localised what the
   `HashMap` had already co-located**: one hash entry holds both sides' `Vec`
   headers and the event-complete flag in a single 64-byte line, whereas the
   arena spreads a symbol across four independently-indexed arrays.

   *Contiguity is not locality.* A flat array is contiguous in the abstract; what
   a cache rewards is the fields used together sharing a line. A `HashMap` entry
   is already an arena of one record, and it had that property for free.

One real bug fell out of the investigation: `is_crossed()` and `is_locked()` each
called `best_bid()` and `best_ask()`, so asking both questions did up to four
reads of level data where two suffice — on 97% of feed traffic. Worse, it
corrupted the decomposition itself, because the no-container floor used a `max()`
that returns `None` without touching storage, so the invariant check's memory
traffic was being charged to whichever container was under test. Fixed; worth
1.8 ns.

The interesting part is not the throughput figure. It is *what dominates* at
each rung — allocation, then cache misses, then the kernel network stack — and
where the floor is.

## Status

The transport layer (`crates/iextp`), the DEEP message decoder (`crates/deep`)
and the order book (`crates/book`) are implemented, validated against real
captures, and benchmarked. The tick-to-trade pipeline and the R2 kernel-bypass
rung are next.

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

- **IEX ships two different formats under one `.pcap` extension.** The 2017 file
  is classic pcap; the 2018 file is pcapng. A classic-pcap reader does not fail
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
