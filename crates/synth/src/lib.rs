//! Generates valid IEX-TP/DEEP captures.
//!
//! Two jobs, and the second one is why this is a library rather than a script.
//!
//! **It makes the repository demonstrate itself.** Every example here needs a
//! capture, and a real one is 0.7 MB to 12 GB and cannot be committed. Without a
//! generator, a fresh clone gives you passing tests and nine examples that panic.
//! With one, `cargo run --example demo` works the moment the clone finishes, and
//! the browser demo has something to decode.
//!
//! **It closes a hole in the correctness argument.** Everything so far rests on
//! two independent *decoders* agreeing. That is a strong check, and it is blind
//! to a misunderstanding they might share. An encoder written from the same
//! field offsets and then round-tripped is a different kind of check: bytes in,
//! messages out, bytes back, identical. It would have caught the `event_complete`
//! mask bug — encoding "complete" and decoding "mid-event" fails instantly —
//! which took real market data to notice.
//!
//! The output is a genuine classic-pcap file: Ethernet with the 802.1Q VLAN tag
//! real IEX frames carry, IPv4, UDP to the multicast group, then IEX-TP segments
//! of length-prefixed DEEP messages. It is parsed by the same code path as a
//! capture from the exchange, with no synthetic shortcut.

use deep::{Price, Symbol};

/// Deterministic xorshift. Seeded explicitly so a generated capture is
/// reproducible: a demo that shows something different every load cannot be
/// used to reproduce a bug.
struct Rng(u64);

impl Rng {
    #[inline]
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    #[inline]
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    /// True with probability `p`, to a resolution of 1/10000.
    #[inline]
    fn chance(&mut self, p: f64) -> bool {
        self.below(10_000) < (p.clamp(0.0, 1.0) * 10_000.0) as u64
    }
}

/// What to generate.
#[derive(Debug, Clone)]
pub struct Config {
    pub symbols: Vec<Symbol>,
    /// Segments to emit. Each carries one to four messages.
    pub segments: usize,
    pub seed: u64,
    /// Fraction of segments emitted twice, modelling the second feed of an A/B
    /// pair. The receiver should deliver each exactly once.
    pub duplicate_rate: f64,
    /// Fraction of segments dropped, modelling packet loss. The receiver should
    /// notice the sequence gap rather than silently skipping.
    pub gap_rate: f64,
    /// Fraction of price level updates that delete their level. The measured
    /// real figure is 0.389.
    pub delete_rate: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            symbols: ["AAPL", "MSFT", "SPY", "QQQ", "NVDA", "TSLA", "AMZN", "GOOG"]
                .iter()
                .map(|t| Symbol::from_ticker(t))
                .collect(),
            segments: 20_000,
            seed: 0x2545_F491_4F6C_DD1D,
            duplicate_rate: 0.0,
            gap_rate: 0.0,
            delete_rate: 0.389,
        }
    }
}

/// Per-symbol state, so generated prices behave like a market rather than noise.
///
/// The live levels are tracked, not just the mid. The first version of this kept
/// only a random-walking mid and posted each side relative to it, which produced
/// a book crossed on 90% of updates: levels persist, so a bid posted before the
/// mid drifted down sat above an ask posted after. The receiver dutifully
/// reported it, which is the invariant working -- against data that could not
/// occur on a real venue.
///
/// A generator for a system whose headline check is "the book never crosses"
/// has to be able to produce a book that does not cross.
struct SymbolState {
    symbol: Symbol,
    /// Mid in price units, random-walked.
    mid: i64,
    /// Live bid prices, ascending. The last is the best bid.
    bids: Vec<i64>,
    /// Live ask prices, ascending. The first is the best ask.
    asks: Vec<i64>,
}

impl SymbolState {
    fn best_bid(&self) -> Option<i64> {
        self.bids.last().copied()
    }
    fn best_ask(&self) -> Option<i64> {
        self.asks.first().copied()
    }
}

/// Deepest ladder the generator maintains per side.
const DEPTH: usize = 4;
/// One cent, in price units.
const TICK: i64 = 100;

/// A generated capture: a complete pcap file plus what it should decode to.
pub struct Capture {
    /// The pcap bytes.
    pub bytes: Vec<u8>,
    /// Messages actually published, counting a duplicated segment once. This is
    /// what a correct receiver must deliver.
    pub messages_published: u64,
    /// Segments emitted twice.
    pub duplicates: u64,
    /// Segments dropped, each of which should register as a gap.
    pub gaps: u64,
    /// Segments actually written. Lower than `Config::segments`, because an
    /// iteration whose messages were all filtered emits nothing rather than an
    /// empty segment.
    pub segments_emitted: u64,
}

const SESSION: u32 = 0x4400_0001;

pub fn generate(cfg: &Config) -> Capture {
    let mut rng = Rng(cfg.seed | 1);
    let mut state: Vec<SymbolState> = cfg
        .symbols
        .iter()
        .enumerate()
        .map(|(i, &symbol)| SymbolState {
            symbol,
            // Spread the book across a plausible price range rather than
            // clustering everything at one point.
            mid: 10_0000 + (i as i64 * 37_0000) % 900_0000,
            bids: Vec::with_capacity(DEPTH),
            asks: Vec::with_capacity(DEPTH),
        })
        .collect();

    let mut out = Vec::with_capacity(cfg.segments * 128);
    write_pcap_header(&mut out);

    let mut sequence: i64 = 1;
    let mut published = 0u64;
    let mut duplicates = 0u64;
    let mut gaps = 0u64;
    let mut emitted = 0u64;
    let mut ts_nanos: u64 = 1_528_371_010_000_000_000;

    for _ in 0..cfg.segments {
        let count = 1 + rng.below(4) as u16;
        let mut body = Vec::with_capacity(count as usize * 32);
        for _ in 0..count {
            let idx = rng.below(cfg.symbols.len() as u64) as usize;
            let complete = !rng.chance(0.10);
            let ts = ts_nanos as i64;

            // Deleting an existing level, at the measured real rate.
            let deleting = rng.chance(cfg.delete_rate);
            let s = &mut state[idx];

            if deleting {
                let from_bids = rng.below(2) == 0;
                let side = if from_bids { &mut s.bids } else { &mut s.asks };
                if side.is_empty() {
                    // Nothing to delete; post instead of emitting a no-op.
                } else {
                    let at = rng.below(side.len() as u64) as usize;
                    let price = side.remove(at);
                    write_price_level(&mut body, from_bids, complete, ts, s.symbol, 0, price);
                    continue;
                }
            }

            // Posting. Constrain the price so the book cannot cross: a bid must
            // sit strictly below the best ask, an ask strictly above the best
            // bid. This is the whole point of tracking live levels.
            let post_bid = if s.bids.len() >= DEPTH {
                false
            } else if s.asks.len() >= DEPTH {
                true
            } else {
                rng.below(2) == 0
            };

            let price = if post_bid {
                let ceiling = s
                    .best_ask()
                    .map(|a| a - TICK)
                    .unwrap_or(s.mid - TICK)
                    .min(s.mid - TICK);
                let floor = ceiling - (DEPTH as i64) * TICK;
                let p = floor + (rng.below(DEPTH as u64) as i64) * TICK;
                if p <= 0 || s.bids.contains(&p) {
                    continue;
                }
                p
            } else {
                let floor = s
                    .best_bid()
                    .map(|b| b + TICK)
                    .unwrap_or(s.mid + TICK)
                    .max(s.mid + TICK);
                let p = floor + (rng.below(DEPTH as u64) as i64) * TICK;
                if s.asks.contains(&p) {
                    continue;
                }
                p
            };

            let size = (1 + rng.below(20)) as u32 * 100;
            if post_bid {
                s.bids.push(price);
                s.bids.sort_unstable();
            } else {
                s.asks.push(price);
                s.asks.sort_unstable();
            }
            write_price_level(&mut body, post_bid, complete, ts, s.symbol, size, price);

            // Drift the mid occasionally, and pull in any level the drift would
            // have left crossing. A real book moves by cancelling and reposting,
            // not by teleporting levels.
            if rng.chance(0.03) {
                let step = (rng.below(3) as i64 - 1) * TICK;
                s.mid = (s.mid + step).max(2 * TICK * DEPTH as i64);
                while let (Some(b), Some(a)) = (s.best_bid(), s.best_ask()) {
                    if b < a {
                        break;
                    }
                    // Cancel the offending bid rather than publishing a cross.
                    s.bids.pop();
                    write_price_level(&mut body, true, complete, ts, s.symbol, 0, b);
                }
            }
        }

        // `count` was an intention; some iterations skip. The header must state
        // what the body actually contains or the receiver reads past the end.
        let actual = count_messages(&body);
        if actual == 0 {
            continue;
        }
        let segment = build_segment(SESSION, sequence, actual, &body);
        sequence += actual as i64;
        published += actual as u64;

        // A dropped segment still consumes its sequence numbers, which is
        // exactly what makes it detectable downstream.
        if rng.chance(cfg.gap_rate) {
            gaps += 1;
            ts_nanos += 1_000;
            continue;
        }

        write_packet(&mut out, ts_nanos, &segment);
        emitted += 1;
        ts_nanos += 1_000;

        if rng.chance(cfg.duplicate_rate) {
            duplicates += 1;
            write_packet(&mut out, ts_nanos, &segment);
            ts_nanos += 1_000;
        }
    }

    Capture {
        bytes: out,
        messages_published: published,
        duplicates,
        gaps,
        segments_emitted: emitted,
    }
}

/// Count length-prefixed messages in a body.
fn count_messages(body: &[u8]) -> u16 {
    let mut n = 0u16;
    let mut i = 0usize;
    while i + 2 <= body.len() {
        let len = u16::from_le_bytes([body[i], body[i + 1]]) as usize;
        i += 2 + len;
        n += 1;
    }
    n
}

fn write_pcap_header(out: &mut Vec<u8>) {
    out.extend_from_slice(&0xa1b2_c3d4u32.to_le_bytes()); // magic, microseconds
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(&65535u32.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // Ethernet
}

/// One DEEP price level update: 30 bytes, little-endian.
fn write_price_level(
    body: &mut Vec<u8>,
    buy: bool,
    complete: bool,
    ts: i64,
    symbol: Symbol,
    size: u32,
    price: i64,
) {
    let msg_start = body.len();
    body.extend_from_slice(&0u16.to_le_bytes()); // length placeholder
    body.push(if buy { 0x38 } else { 0x35 });
    body.push(u8::from(complete)); // bit 0 is event-processing-complete
    body.extend_from_slice(&ts.to_le_bytes());
    body.extend_from_slice(&symbol.0);
    body.extend_from_slice(&size.to_le_bytes());
    body.extend_from_slice(&Price::from_raw(price).raw().to_le_bytes());
    let len = (body.len() - msg_start - 2) as u16;
    body[msg_start..msg_start + 2].copy_from_slice(&len.to_le_bytes());
}

/// The 40-byte IEX-TP header, then the message block.
fn build_segment(session: u32, first_sequence: i64, count: u16, body: &[u8]) -> Vec<u8> {
    let mut s = Vec::with_capacity(40 + body.len());
    s.push(1); // version
    s.push(0); // reserved
    s.extend_from_slice(&0x8004u16.to_le_bytes()); // DEEP
    s.extend_from_slice(&1u32.to_le_bytes()); // channel
    s.extend_from_slice(&session.to_le_bytes());
    s.extend_from_slice(&(body.len() as u16).to_le_bytes());
    s.extend_from_slice(&count.to_le_bytes());
    s.extend_from_slice(&0i64.to_le_bytes()); // stream offset
    s.extend_from_slice(&first_sequence.to_le_bytes());
    s.extend_from_slice(&0i64.to_le_bytes()); // send time
    s.extend_from_slice(body);
    s
}

/// Ethernet + 802.1Q VLAN + IPv4 + UDP around one segment, framed as a pcap
/// record. The VLAN tag is not decoration: every real IEX frame carries one, and
/// a decoder that assumes IPv4 at offset 14 misparses all of them. Generating
/// them tagged means the demo exercises the same path a real capture does.
fn write_packet(out: &mut Vec<u8>, ts_nanos: u64, segment: &[u8]) {
    let mut frame = Vec::with_capacity(64 + segment.len());
    frame.extend_from_slice(&[0x01, 0x00, 0x5e, 0x57, 0x15, 0x04]); // multicast dst
    frame.extend_from_slice(&[0x00, 0x1e, 0x67, 0xf2, 0x62, 0x24]); // src
    frame.extend_from_slice(&0x8100u16.to_be_bytes()); // VLAN
    frame.extend_from_slice(&[0x03, 0xf5]);
    frame.extend_from_slice(&0x0800u16.to_be_bytes()); // IPv4

    let ip_start = frame.len();
    let udp_len = 8 + segment.len();
    let total_len = 20 + udp_len;
    frame.extend_from_slice(&[0x45, 0x00]);
    frame.extend_from_slice(&(total_len as u16).to_be_bytes());
    frame.extend_from_slice(&[0, 0, 0, 0, 64, 17, 0, 0]);
    frame.extend_from_slice(&[10, 0, 0, 1]);
    frame.extend_from_slice(&[233, 215, 21, 4]);
    let _ = ip_start;

    frame.extend_from_slice(&10378u16.to_be_bytes()); // src port
    frame.extend_from_slice(&10378u16.to_be_bytes()); // dst port
    frame.extend_from_slice(&(udp_len as u16).to_be_bytes());
    frame.extend_from_slice(&[0, 0]); // checksum: optional over IPv4
    frame.extend_from_slice(segment);

    let secs = (ts_nanos / 1_000_000_000) as u32;
    let usecs = ((ts_nanos % 1_000_000_000) / 1_000) as u32;
    out.extend_from_slice(&secs.to_le_bytes());
    out.extend_from_slice(&usecs.to_le_bytes());
    out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    out.extend_from_slice(&frame);
}

#[cfg(test)]
mod tests {
    use super::*;
    use iextp::{capture::Capture as Cap, frame, segment::Segment, sequencer::Sequencer, Action};

    /// Decode a generated capture with the production path.
    fn decode(bytes: &[u8]) -> (u64, u64, Sequencer) {
        let mut seq = Sequencer::default();
        let mut packets = 0u64;
        let mut messages = 0u64;
        for pkt in Cap::open(bytes).expect("generated a readable capture") {
            packets += 1;
            let dg = frame::parse(pkt.data).expect("generated a parseable frame");
            let seg = Segment::parse(dg.payload).expect("generated a parseable segment");
            if matches!(
                seq.observe(seg.session, seg.first_sequence, seg.message_count),
                Action::Deliver | Action::SessionReset
            ) {
                messages += seg.messages().count() as u64;
            }
        }
        (packets, messages, seq)
    }

    /// The round trip the project did not previously have: bytes we wrote,
    /// decoded by the same code that reads the exchange's bytes.
    #[test]
    fn a_clean_capture_decodes_to_exactly_what_was_published() {
        let cfg = Config {
            segments: 5_000,
            ..Default::default()
        };
        let cap = generate(&cfg);
        let (_, messages, seq) = decode(&cap.bytes);
        assert_eq!(messages, cap.messages_published);
        assert_eq!(seq.stats.duplicates, 0);
        assert_eq!(seq.stats.gaps_opened, 0);
    }

    /// Every field must survive the trip, not just the message count.
    #[test]
    fn message_contents_survive_the_round_trip() {
        let cfg = Config {
            segments: 500,
            symbols: vec![Symbol::from_ticker("AAPL")],
            ..Default::default()
        };
        let cap = generate(&cfg);
        let mut seen = 0;
        let mut complete_seen = false;
        let mut mid_event_seen = false;
        for pkt in Cap::open(&cap.bytes).unwrap() {
            let dg = frame::parse(pkt.data).unwrap();
            let seg = Segment::parse(dg.payload).unwrap();
            for body in seg.messages() {
                let msg = deep::parse(body).expect("generated a decodable message");
                let deep::Message::PriceLevel(u) = msg else {
                    panic!("expected a price level update")
                };
                assert_eq!(u.symbol.as_str(), Some("AAPL"));
                assert!(u.price.raw() > 0, "prices must be positive");
                // The bug this round trip would have caught: encoding bit 0 and
                // decoding bit 7 disagree immediately here.
                if u.event_complete() {
                    complete_seen = true;
                } else {
                    mid_event_seen = true;
                }
                seen += 1;
            }
        }
        assert!(seen > 0);
        assert!(complete_seen, "some updates must complete their event");
        assert!(mid_event_seen, "some updates must be mid-event");
    }

    /// The B feed: every segment sent twice, every message delivered once.
    #[test]
    fn duplicated_segments_are_suppressed_exactly_once_each() {
        let cfg = Config {
            segments: 3_000,
            duplicate_rate: 1.0,
            ..Default::default()
        };
        let cap = generate(&cfg);
        let (packets, messages, seq) = decode(&cap.bytes);
        assert_eq!(
            packets,
            cap.segments_emitted * 2,
            "every emitted segment sent twice"
        );
        assert_eq!(messages, cap.messages_published, "each delivered once");
        assert_eq!(seq.stats.duplicates, cap.segments_emitted);
    }

    /// Dropped segments must register as gaps rather than vanishing.
    #[test]
    fn dropped_segments_are_detected_as_gaps() {
        let cfg = Config {
            segments: 5_000,
            gap_rate: 0.02,
            ..Default::default()
        };
        let cap = generate(&cfg);
        assert!(cap.gaps > 0, "the generator should have dropped some");
        let (_, _, seq) = decode(&cap.bytes);
        assert!(
            seq.stats.gaps_opened > 0,
            "the receiver must notice, not silently skip"
        );
    }

    /// The generator must be able to produce a market that does not cross.
    ///
    /// Its first version could not: a random-walking mid with persistent levels
    /// left stale bids above fresh asks on 90% of updates. The receiver reported
    /// every one, correctly, against data no venue could publish.
    #[test]
    fn a_clean_capture_never_crosses_the_book() {
        use book::{Book, SortedLevels};
        let cap = generate(&Config {
            segments: 20_000,
            ..Default::default()
        });
        let mut seq = Sequencer::default();
        let mut b: Book<SortedLevels> = Book::with_capacity(64);
        for pkt in Cap::open(&cap.bytes).unwrap() {
            let dg = frame::parse(pkt.data).unwrap();
            let seg = Segment::parse(dg.payload).unwrap();
            if !matches!(
                seq.observe(seg.session, seg.first_sequence, seg.message_count),
                Action::Deliver | Action::SessionReset
            ) {
                continue;
            }
            for body in seg.messages() {
                if let Ok(deep::Message::PriceLevel(u)) = deep::parse(body) {
                    b.apply_price_level(&u);
                }
            }
        }
        assert!(b.stats.updates > 10_000, "should have exercised the book");
        assert_eq!(
            b.stats.crossed_when_stable, 0,
            "a generated market must not cross at an event boundary"
        );
    }

    #[test]
    fn the_same_seed_gives_the_same_bytes() {
        let a = generate(&Config {
            segments: 500,
            ..Default::default()
        });
        let b = generate(&Config {
            segments: 500,
            ..Default::default()
        });
        assert_eq!(a.bytes, b.bytes, "a demo must be reproducible");
    }
}
