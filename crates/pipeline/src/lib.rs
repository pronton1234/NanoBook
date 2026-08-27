//! The tick-to-trade path: wire bytes in, order bytes out.
//!
//! ```text
//!   receive   strip Ethernet / VLAN / IPv4 / UDP        [iextp::frame]
//!   sequence  IEX-TP segment, gaps, A/B arbitration     [iextp::segment, sequencer]
//!   parse     DEEP message decode                        [deep]
//!   book      apply the price level update               [book]
//!   decide    the (deliberately trivial) strategy        [strategy]
//!   encode    OUCH-style Enter Order to a buffer         [order]
//! ```
//!
//! ## How the stages are timed, and why not the obvious way
//!
//! The obvious approach is a clock read between each stage. It does not work
//! here. `Instant::now()` costs on the order of 20ns, the whole pipeline is
//! tens of nanoseconds, and six clock reads per message would cost more than
//! the thing being measured — reporting a budget dominated by the act of
//! measuring it.
//!
//! So stage costs come from **cumulative prefixes**, each in its own pass over
//! the same data: run receive alone and time it, then receive+sequence, then
//! +parse, and so on. Each stage's cost is the difference between consecutive
//! prefixes, and the inner loop contains no clock at all. The same technique
//! gave clean container numbers in the book bake-off.
//!
//! End-to-end *distribution* still needs per-message timing, since a quantile
//! cannot be recovered from a total. That path pays clock reads, and
//! [`clock_overhead_nanos`] measures what they cost so the figure can be stated
//! rather than hidden. On the development machine one read is ~49 ns — half the
//! pipeline — which is why the budget above avoids them entirely and why the
//! replay driver reads the clock **once** per item rather than twice.
//!
//! ## Never benchmark two arrangements in blocks
//!
//! Machine load drifts over a multi-minute run, so timing all repetitions of A
//! and then all repetitions of B hands that drift entirely to one of them.
//! Taking the minimum does not save you: the minimum of five runs taken while
//! the machine was busy is still worse than the minimum of five taken after it
//! quietened down.
//!
//! This has now produced two wrong results in this crate. The stage budget saw
//! a *negative* cost when drift accumulated along the stage order, and the
//! threaded comparison reported the single-threaded path at 187 ns against the
//! 110 ns the budget measures for the same code — a 70% gap that was the
//! schedule, not the design. Both are fixed the same way: interleave the
//! arrangements, one of each per repetition.
//!
//! ## Warm up, or measure the page fault instead
//!
//! Prefix timing has one trap, and it produced a *negative* stage cost the first
//! time this ran. The first prefix that builds a book pays every page fault for
//! ~6,000 symbol entries and their level storage; later prefixes reuse pages the
//! allocator already holds. That inflated `book update` to 2.7x its real cost,
//! so differencing the next stage against it gave −239 ns. A negative cost is
//! impossible, which is the only reason it was visible at all — an inflated but
//! still-positive number would have been published as a finding. The budget now
//! runs the full pipeline once, untimed, before measuring anything.
//!
//! ## Coordinated omission
//!
//! Most published latency benchmarks are wrong in the same way: they measure
//! service time — how long one item took once work began on it — and call it
//! latency. If the system stalls, the items that pile up behind the stall are
//! not measured as slow. They are simply measured late, or not at all, and the
//! stall vanishes from the histogram.
//!
//! [`Replay`] corrects for it by giving every message a *scheduled* arrival time
//! derived from a target rate and measuring from that, not from when processing
//! happened to start. Below capacity the two agree. Above capacity the backlog
//! is charged to every message behind it, which is what a real feed would do.

pub mod hist;
pub mod order;
pub mod spsc;
pub mod strategy;

use std::collections::BTreeMap;

pub use hist::Histogram;
pub use order::{encode_enter_order, EncodeError, NewOrder, TokenGen, ENTER_ORDER_LEN};
pub use strategy::{Params, Quote, Strategy};

use book::{Book, Levels};
use deep::Message;
use iextp::{frame, segment::Segment, sequencer::Sequencer, Action};
use std::time::Instant;

/// How far down the path to run. Used for cumulative-prefix stage timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    Receive,
    Sequence,
    Parse,
    BookUpdate,
    Decide,
    Encode,
}

impl Stage {
    /// Every stage, shallowest first.
    pub const ALL: [Stage; 6] = [
        Stage::Receive,
        Stage::Sequence,
        Stage::Parse,
        Stage::BookUpdate,
        Stage::Decide,
        Stage::Encode,
    ];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Stage::Receive => "receive (eth/vlan/ip/udp)",
            Stage::Sequence => "sequence (IEX-TP, A/B)",
            Stage::Parse => "parse (DEEP)",
            Stage::BookUpdate => "book update",
            Stage::Decide => "decide (strategy)",
            Stage::Encode => "encode (OUCH order)",
        }
    }
}

/// Segment payloads held while a gap is open.
///
/// The sequencer buffers sequence *numbers*, which is all it needs to classify
/// arrivals — but a receiver has to replay the messages once the gap closes, and
/// for that it needs the bytes. Without this the pipeline skipped every buffered
/// segment permanently, so a single dropped packet cost every segment behind it
/// until the resync. A demo at 2% loss delivered 39,890 messages out of ~147,000
/// that survived the drop; the missing ones were not lost by the network, they
/// were discarded here.
///
/// Bounded, because an unbounded buffer behind a permanently lost segment is
/// just a slower way to fail.
const MAX_PENDING_SEGMENTS: usize = 256;

/// Held segments tolerated before the stream is resynchronised.
///
/// A gap is not immediately a loss -- UDP reorders, and a segment arriving out
/// of order is normal. But a segment that was genuinely dropped never arrives,
/// and a receiver that waits for it forever stops delivering anything. On a live
/// feed the sequence is: notice the gap, request retransmission, and if that
/// goes unanswered, resynchronise and count what was skipped.
///
/// This project had no recovery at all until a demo run with 2% packet loss
/// delivered 61 messages out of 117,522 packets. The sequencer was correct; the
/// pipeline simply never told it to give up.
pub const MAX_HELD_BEFORE_RESYNC: usize = 64;

#[derive(Debug, Default, Clone, Copy)]
pub struct Counters {
    pub packets: u64,
    pub messages: u64,
    pub book_updates: u64,
    pub quotes: u64,
    pub orders_encoded: u64,
    pub encode_errors: u64,
    /// Times the stream was resynchronised past an unfillable gap.
    pub resyncs: u64,
    /// Books dropped because a resync made their state untrustworthy.
    pub book_resets: u64,
    /// Messages skipped by those resynchronisations. Counted, never hidden: a
    /// receiver that quietly resynchronises is indistinguishable from one that
    /// never dropped anything.
    pub messages_lost: u64,
}

/// The full path, generic over the book's level container.
pub struct Pipeline<L: Levels> {
    pub book: Book<L>,
    pub seq: Sequencer,
    pub strategy: Strategy,
    tokens: TokenGen,
    out: [u8; ENTER_ORDER_LEN],
    /// Segment payloads held while a gap is open, keyed by first sequence.
    pending: BTreeMap<i64, Vec<u8>>,
    /// When set, `decide` re-looks-up the symbol instead of using the touch the
    /// update returned. Exists only so the optimisation log can measure the two
    /// against each other; production leaves it false.
    pub redundant_lookup: bool,
    pub counters: Counters,
}

impl<L: Levels> Default for Pipeline<L> {
    fn default() -> Self {
        Self::with_capacity(8192)
    }
}

impl<L: Levels> Pipeline<L> {
    #[must_use]
    pub fn with_capacity(symbols: usize) -> Self {
        Self {
            book: Book::with_capacity(symbols),
            seq: Sequencer::default(),
            strategy: Strategy::default(),
            tokens: TokenGen::default(),
            out: [0u8; ENTER_ORDER_LEN],
            pending: BTreeMap::new(),
            redundant_lookup: false,
            counters: Counters::default(),
        }
    }

    /// Most recently encoded order bytes, for inspection in tests.
    #[must_use]
    pub fn last_order(&self) -> &[u8; ENTER_ORDER_LEN] {
        &self.out
    }

    /// Run one captured frame through the path, stopping after `upto`.
    ///
    /// `black_box` guards each early return: without it the optimiser is free to
    /// notice that a truncated run's results are unused and delete the work,
    /// which would make the shallow prefixes look free and every stage cost come
    /// out wrong.
    #[inline]
    pub fn on_frame(&mut self, bytes: &[u8], upto: Stage) {
        self.counters.packets += 1;

        let Ok(dg) = frame::parse(bytes) else { return };
        if upto == Stage::Receive {
            std::hint::black_box(&dg);
            return;
        }

        let Ok(seg) = Segment::parse(dg.payload) else {
            return;
        };
        let action = self
            .seq
            .observe(seg.session, seg.first_sequence, seg.message_count);
        // Recover from an unfillable gap. Without this the reorder buffer grows
        // forever behind a segment that was genuinely lost, and the pipeline
        // silently stops delivering.
        if self.seq.held_len() >= MAX_HELD_BEFORE_RESYNC {
            let before = self.seq.stats.messages_lost;
            while self.seq.held_len() >= MAX_HELD_BEFORE_RESYNC && self.seq.force_resync().is_some()
            {
                self.counters.resyncs += 1;
            }
            self.counters.messages_lost += self.seq.stats.messages_lost - before;
        }
        if !matches!(action, Action::Deliver | Action::SessionReset) {
            return;
        }
        if upto == Stage::Sequence {
            std::hint::black_box(&seg);
            return;
        }

        for body in seg.messages() {
            let Ok(msg) = deep::parse(body) else { continue };
            self.counters.messages += 1;
            if upto == Stage::Parse {
                std::hint::black_box(&msg);
                continue;
            }

            let Message::PriceLevel(u) = msg else {
                self.book.apply(&msg);
                continue;
            };
            let touch = self.book.apply_price_level(&u);
            self.counters.book_updates += 1;
            if upto == Stage::BookUpdate {
                continue;
            }

            // The touch comes back from the update, which already had to read
            // both sides. `decide` therefore performs no lookup of its own.
            let quote = if self.redundant_lookup {
                // The version this replaced, kept so the two can be measured
                // interleaved in one process rather than compared across runs.
                let Some(sb) = self.book.get(u.symbol) else {
                    continue;
                };
                self.strategy.on_touch(
                    u.symbol,
                    book::Touch {
                        bid: sb.best_bid(),
                        ask: sb.best_ask(),
                        stable: sb.is_stable(),
                    },
                )
            } else {
                self.strategy.on_touch(u.symbol, touch)
            };
            if upto == Stage::Decide {
                std::hint::black_box(&quote);
                continue;
            }

            let Some(q) = quote else { continue };
            self.counters.quotes += 1;
            let o = NewOrder {
                token: self.tokens.next_token(),
                side: q.side,
                shares: q.shares,
                symbol: q.symbol,
                price: q.price,
            };
            match encode_enter_order(&o, &mut self.out) {
                Ok(_) => self.counters.orders_encoded += 1,
                Err(_) => self.counters.encode_errors += 1,
            }
            std::hint::black_box(&self.out);
        }
    }
}

impl<L: Levels> Pipeline<L> {
    /// The production path: no `Stage` parameter, so no prefix branches.
    ///
    /// `on_frame` carries five `upto == Stage::_` comparisons so the budget can
    /// time cumulative prefixes. They are perfectly predictable — the value is
    /// constant for a whole run — so the expectation is that removing them
    /// changes nothing measurable. That expectation is worth testing rather than
    /// assuming, which is what the optimisation log is for.
    #[inline]
    pub fn on_frame_fast(&mut self, bytes: &[u8]) {
        self.counters.packets += 1;
        let Ok(dg) = frame::parse(bytes) else { return };
        let Ok(seg) = Segment::parse(dg.payload) else {
            return;
        };
        let action = self
            .seq
            .observe(seg.session, seg.first_sequence, seg.message_count);

        match action {
            Action::Deliver | Action::SessionReset => {
                self.deliver_segment(dg.payload);
            }
            Action::Buffered { .. } => {
                // Keep the bytes. The sequencer knows the segment arrived; only
                // the caller can replay what was in it.
                if self.pending.len() < MAX_PENDING_SEGMENTS {
                    self.pending.insert(seg.first_sequence, dg.payload.to_vec());
                }
            }
            Action::Duplicate | Action::Heartbeat => return,
        }

        // Give up on a gap that will not fill. Without this the buffer grows
        // behind a segment that was genuinely lost and delivery stops.
        if self.seq.held_len() >= MAX_HELD_BEFORE_RESYNC {
            let before = self.seq.stats.messages_lost;
            while self.seq.held_len() >= MAX_HELD_BEFORE_RESYNC && self.seq.force_resync().is_some()
            {
                self.counters.resyncs += 1;
            }
            self.counters.messages_lost += self.seq.stats.messages_lost - before;
            // A resync means messages were skipped, so the book now reflects a
            // state that never existed -- levels whose deletes were lost stay
            // forever. There is no way to know which symbols are affected,
            // because the missing messages are by definition unknown, so the
            // only sound response is to drop it and rebuild.
            self.book.clear();
            self.counters.book_resets += 1;
        }

        self.drain_pending();
    }

    /// Replay any buffered segments the sequencer has now advanced past.
    #[inline]
    fn drain_pending(&mut self) {
        while let Some((&seq, _)) = self.pending.iter().next() {
            if seq >= self.seq.expected() {
                break;
            }
            let bytes = self.pending.remove(&seq).expect("just observed");
            self.deliver_segment(&bytes);
        }
    }

    /// Process one segment's messages.
    #[inline]
    fn deliver_segment(&mut self, payload: &[u8]) {
        let Ok(seg) = Segment::parse(payload) else {
            return;
        };
        for body in seg.messages() {
            let Ok(msg) = deep::parse(body) else { continue };
            self.counters.messages += 1;
            let Message::PriceLevel(u) = msg else {
                self.book.apply(&msg);
                continue;
            };
            let touch = self.book.apply_price_level(&u);
            self.counters.book_updates += 1;
            let Some(q) = self.strategy.on_touch(u.symbol, touch) else {
                continue;
            };
            self.counters.quotes += 1;
            let o = NewOrder {
                token: self.tokens.next_token(),
                side: q.side,
                shares: q.shares,
                symbol: q.symbol,
                price: q.price,
            };
            match encode_enter_order(&o, &mut self.out) {
                Ok(_) => self.counters.orders_encoded += 1,
                Err(_) => self.counters.encode_errors += 1,
            }
            std::hint::black_box(&self.out);
        }
    }
}

/// Empirical cost of one `Instant::now()` pair, so per-message timing can state
/// its own overhead instead of pretending to have none.
#[must_use]
pub fn clock_overhead_nanos() -> f64 {
    const N: u32 = 200_000;
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..N {
            let a = Instant::now();
            std::hint::black_box(a);
        }
        let e = start.elapsed().as_nanos() as f64 / f64::from(N);
        if e < best {
            best = e;
        }
    }
    best
}

/// Replay driver with coordinated-omission correction.
///
/// `rate_per_sec` is the *offered* load. Each message's scheduled arrival is
/// `start + i / rate`, and latency is measured from that instant — so a stall is
/// charged to every message queued behind it, which plain service-time timing
/// would never show.
pub struct Replay {
    pub rate_per_sec: f64,
    /// Latency from scheduled arrival. The honest number.
    pub corrected: Histogram,
    /// Time spent processing once started. Shown alongside to make the gap
    /// between the two visible rather than arguable.
    pub service: Histogram,
    pub behind: u64,
}

impl Replay {
    #[must_use]
    pub fn new(rate_per_sec: f64) -> Self {
        Self {
            rate_per_sec,
            corrected: Histogram::new(),
            service: Histogram::new(),
            behind: 0,
        }
    }

    /// Record one item, given when it was scheduled, started and finished.
    #[inline]
    pub fn record(&mut self, scheduled_nanos: u64, started_nanos: u64, finished_nanos: u64) {
        self.service
            .record(finished_nanos.saturating_sub(started_nanos));
        self.corrected
            .record(finished_nanos.saturating_sub(scheduled_nanos));
        if started_nanos > scheduled_nanos {
            self.behind += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use book::SortedLevels;

    /// Build a frame carrying one IEX-TP segment with the given DEEP messages.
    fn frame_with(session: u32, first_seq: i64, msgs: &[&[u8]]) -> Vec<u8> {
        let mut body = Vec::new();
        for m in msgs {
            body.extend_from_slice(&(m.len() as u16).to_le_bytes());
            body.extend_from_slice(m);
        }
        let mut seg = Vec::new();
        seg.push(1u8);
        seg.push(0);
        seg.extend_from_slice(&0x8004u16.to_le_bytes());
        seg.extend_from_slice(&1u32.to_le_bytes());
        seg.extend_from_slice(&session.to_le_bytes());
        seg.extend_from_slice(&(body.len() as u16).to_le_bytes());
        seg.extend_from_slice(&(msgs.len() as u16).to_le_bytes());
        seg.extend_from_slice(&0i64.to_le_bytes());
        seg.extend_from_slice(&first_seq.to_le_bytes());
        seg.extend_from_slice(&0i64.to_le_bytes());
        seg.extend_from_slice(&body);

        let mut f = vec![0u8; 12];
        f.extend_from_slice(&0x8100u16.to_be_bytes()); // VLAN, as real IEX frames carry
        f.extend_from_slice(&[0x03, 0xf5]);
        f.extend_from_slice(&0x0800u16.to_be_bytes());
        let ip = f.len();
        f.extend_from_slice(&[0x45, 0, 0, 0, 0, 0, 0, 0, 64, 17, 0, 0]);
        f.extend_from_slice(&[10, 0, 0, 1]);
        f.extend_from_slice(&[233, 215, 21, 4]);
        f.extend_from_slice(&[0x28, 0x8a]);
        f.extend_from_slice(&10378u16.to_be_bytes());
        f.extend_from_slice(&((8 + seg.len()) as u16).to_be_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(&seg);
        let total = (f.len() - ip) as u16;
        f[ip + 2..ip + 4].copy_from_slice(&total.to_be_bytes());
        f
    }

    fn plu(kind: u8, sym: &str, size: u32, price: deep::Price) -> Vec<u8> {
        let mut v = vec![kind, 0x01];
        v.extend_from_slice(&1_500_000_000_000_000_000i64.to_le_bytes());
        v.extend_from_slice(&deep::Symbol::from_ticker(sym).0);
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&price.raw().to_le_bytes());
        v
    }

    /// Wire bytes in, order bytes out, through every stage.
    #[test]
    fn a_wide_spread_drives_an_order_onto_the_wire() {
        let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(16);
        let bid = plu(0x38, "AAPL", 500, deep::Price::from_parts(100, 0));
        let ask = plu(0x35, "AAPL", 500, deep::Price::from_parts(100, 500));
        let f = frame_with(1, 1, &[&bid, &ask]);
        p.on_frame(&f, Stage::Encode);

        assert_eq!(p.counters.messages, 2);
        assert_eq!(p.counters.book_updates, 2);
        assert_eq!(p.counters.orders_encoded, 1, "the wide spread should quote");
        assert_eq!(p.counters.encode_errors, 0);

        let o = p.last_order();
        assert_eq!(o[0], b'O');
        assert_eq!(o[15], b'B');
        assert_eq!(&o[20..28], b"AAPL    ");
        assert_eq!(
            u32::from_be_bytes(o[28..32].try_into().unwrap()),
            1_000_000,
            "order price must be the joined bid"
        );
    }

    #[test]
    fn a_tight_spread_reaches_the_book_but_emits_nothing() {
        let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(16);
        let bid = plu(0x38, "AAPL", 500, deep::Price::from_parts(100, 0));
        let ask = plu(0x35, "AAPL", 500, deep::Price::from_parts(100, 100));
        p.on_frame(&frame_with(1, 1, &[&bid, &ask]), Stage::Encode);
        assert_eq!(p.counters.book_updates, 2);
        assert_eq!(p.counters.orders_encoded, 0);
    }

    /// Each prefix must do strictly less than the next. If a shallow stage
    /// already updated the book, the stage costs derived by difference are
    /// meaningless.
    #[test]
    fn each_prefix_stops_where_it_says_it_does() {
        let bid = plu(0x38, "AAPL", 500, deep::Price::from_parts(100, 0));
        let ask = plu(0x35, "AAPL", 500, deep::Price::from_parts(100, 500));
        let f = frame_with(1, 1, &[&bid, &ask]);

        for (stage, messages, updates, orders) in [
            (Stage::Receive, 0, 0, 0),
            (Stage::Sequence, 0, 0, 0),
            (Stage::Parse, 2, 0, 0),
            (Stage::BookUpdate, 2, 2, 0),
            (Stage::Decide, 2, 2, 0),
            (Stage::Encode, 2, 2, 1),
        ] {
            let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(16);
            p.on_frame(&f, stage);
            assert_eq!(p.counters.messages, messages, "{stage:?} messages");
            assert_eq!(p.counters.book_updates, updates, "{stage:?} book updates");
            assert_eq!(p.counters.orders_encoded, orders, "{stage:?} orders");
        }
    }

    /// A duplicate segment — the losing side of an A/B pair — must be dropped by
    /// the sequencer and never reach the book twice.
    #[test]
    fn a_duplicate_segment_does_not_double_apply() {
        let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(16);
        let bid = plu(0x38, "AAPL", 500, deep::Price::from_parts(100, 0));
        let f = frame_with(1, 1, &[&bid]);
        p.on_frame(&f, Stage::Encode);
        p.on_frame(&f, Stage::Encode);
        assert_eq!(p.counters.packets, 2);
        assert_eq!(
            p.counters.book_updates, 1,
            "the B-feed copy must be dropped"
        );
    }

    /// Coordinated omission: a stall must be charged to everything behind it.
    #[test]
    fn corrected_latency_charges_the_backlog_but_service_time_hides_it() {
        let mut r = Replay::new(1_000_000.0); // one message per microsecond
                                              // Ten messages scheduled 1us apart. Message 0 stalls for 10us; the rest
                                              // each take 100ns of service but start late because of the stall.
        r.record(0, 0, 10_000);
        for i in 1..10u64 {
            let scheduled = i * 1_000;
            let started = 10_000 + (i - 1) * 100;
            r.record(scheduled, started, started + 100);
        }
        assert!(
            r.service.quantile(0.99) < 11_000,
            "service time sees only the one slow item"
        );
        assert!(
            r.corrected.quantile(0.99) >= 9_000,
            "corrected latency charges the queued messages for the stall"
        );
        assert!(r.behind > 0, "the backlog must be visible");
    }
}
