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
| **R0** naive | allocating, `BTreeMap`, blocking sockets | planned |
| **R1** optimized | zero-copy, flat tick-indexed arrays, slab allocation | planned |
| **R2** kernel bypass | AF_XDP, busy-poll, pinned cores, IRQ affinity | planned |

The interesting part is not the throughput figure. It is *what dominates* at
each rung — allocation, then cache misses, then the kernel network stack — and
where the floor is.

## Status

Early. The transport layer (`crates/iextp`) and the DEEP message decoder
(`crates/deep`) are implemented and validated against real captures. The order
book and pipeline are next.

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
