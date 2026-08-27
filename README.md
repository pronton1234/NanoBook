# Latency Ladder

**A tick-to-trade market data pipeline in Rust — wire bytes in, order bytes out.**

[![CI](https://github.com/pronton1234/LatencyLadder/actions/workflows/ci.yml/badge.svg)](https://github.com/pronton1234/LatencyLadder/actions/workflows/ci.yml)

### ▶ [Run it live](https://pronton1234.github.io/LatencyLadder/)

Perturb the feed — offered load, packet loss, A/B duplication, level container —
and the **real native pipeline** runs server-side and reports what happened.
Nothing is simulated in the browser.

| | |
|---|---|
| tick-to-trade, single thread | **110.4 ns/packet**, 88.5 ns/message |
| real IEX messages decoded, zero errors | **40,223,554** |
| crossed books at an event boundary | **0** |
| measurement bugs caught before publication | **8** |
| external dependencies | **1** (`thiserror`) |

```
receive    strip Ethernet / 802.1Q VLAN / IPv4 / UDP
sequence   IEX-TP segments · gap detection · retransmit · A/B arbitration
parse      DEEP binary decode, fixed-width, zero allocation
book       aggregated-depth order book, sorted structure-of-arrays
decide     strategy hook (deliberately trivial - a placeholder in the budget)
encode     OUCH-style Enter Order
```

Most order-book projects open a file of already-de-framed messages and start
parsing. That skips everything the transport actually does: market data arrives
as UDP multicast, published twice down physically separate paths, with packets
reordered and dropped, and a receiver has to reconcile all of that before a
single book update is correct.

**The measurement discipline is the point, not the throughput figure.** Eight
benchmarking bugs are documented below, and none of them crashed — every one
produced a plausible number. A negative stage cost. A fake 1.38x threading
speedup. A histogram reporting its own bucket count as a p99. An audit that
returned "zero crossed books" because it inspected the book after the market
closed. Those are in the repository because catching them is the skill the
project is actually about.

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
