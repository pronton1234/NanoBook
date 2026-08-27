//! Build the book over a real capture, check its invariants, and measure its
//! shape.
//!
//! Two jobs, and the second is the more useful one right now.
//!
//! **Validate.** A book is one of the few things in a feed handler with a hard,
//! checkable invariant: at an event boundary the best bid must be below the best
//! ask. Anything else means levels are being lost, mis-keyed, or mis-ordered.
//!
//! **Measure, before optimising.** The fast container has not been written yet,
//! deliberately. A flat tick-indexed array is the textbook answer, and whether
//! it is the right one here depends on numbers nobody has: how deep a side
//! actually gets, and how wide a window it spans. If books are three levels deep
//! then an array is a cache-hostile way to store three integers, and a small
//! sorted vector wins. If they are hundreds deep and tight, the array wins.
//!
//! Guessing that and then benchmarking the guess is how you end up with a fast
//! implementation of the wrong structure.
//!
//!     cargo run --release --example audit -- data/raw/<file>.pcap

use std::time::Instant;

use book::{BTreeLevels, Book, Levels};
use deep::Price;
use iextp::{capture::Capture, frame, segment::Segment, sequencer::Sequencer};

/// Histogram over small counts, with a tail bucket.
struct Hist {
    buckets: Vec<u64>,
}

impl Hist {
    fn new(n: usize) -> Self {
        Self {
            buckets: vec![0; n + 1],
        }
    }
    fn add(&mut self, v: usize) {
        let i = v.min(self.buckets.len() - 1);
        self.buckets[i] += 1;
    }
    fn total(&self) -> u64 {
        self.buckets.iter().sum()
    }
    /// Value at a percentile, treating the last bucket as saturating.
    /// Count that landed in the saturating tail bucket. A percentile at or
    /// above the last bucket is not a measurement, and reporting it as one is
    /// how "p99 = 512" gets mistaken for a fact about the market rather than a
    /// fact about the histogram.
    fn saturated(&self) -> u64 {
        *self.buckets.last().unwrap_or(&0)
    }

    fn pct(&self, p: f64) -> usize {
        let target = (self.total() as f64 * p) as u64;
        let mut seen = 0;
        for (i, &n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= target {
                return i;
            }
        }
        self.buckets.len() - 1
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: audit <capture.pcap>");
    let buf = std::fs::read(&path).expect("read capture");
    let reader = Capture::open(&buf).expect("open capture");
    println!("{path}\n");

    let mut seq = Sequencer::default();
    let mut b: Book<BTreeLevels> = Book::with_capacity(8192);
    let mut messages = 0u64;

    // Shape must be sampled WHILE the session runs. At the close the exchange
    // deletes every displayed level, so a book inspected only at the end is
    // empty -- and an invariant check over an empty book reports "clean"
    // because it measured nothing. The first version of this audit did exactly
    // that and looked like a pass.
    const SAMPLE_EVERY: u64 = 1_000_000;
    let mut depth = Hist::new(64);
    let mut span_ticks = Hist::new(200_000);
    let mut spread_ticks = Hist::new(100_000);
    let mut max_depth = 0usize;
    let mut widest_span = 0usize;
    let mut peak_levels = 0usize;
    let mut peak_two_sided = 0usize;
    let mut samples = 0u64;

    let start = Instant::now();
    for pkt in reader {
        let Ok(dg) = frame::parse(pkt.data) else {
            continue;
        };
        let Ok(seg) = Segment::parse(dg.payload) else {
            continue;
        };
        if !matches!(
            seq.observe(seg.session, seg.first_sequence, seg.message_count),
            iextp::Action::Deliver | iextp::Action::SessionReset
        ) {
            continue;
        }
        for body in seg.messages() {
            if let Ok(msg) = deep::parse(body) {
                messages += 1;
                b.apply(&msg);
                if messages % SAMPLE_EVERY == 0 {
                    samples += 1;
                    peak_levels = peak_levels.max(b.total_levels());
                    let mut two_sided = 0usize;
                    for (_, sb) in b.iter() {
                        for side in [&sb.bids, &sb.asks] {
                            let n = side.len();
                            if n == 0 {
                                continue;
                            }
                            depth.add(n);
                            max_depth = max_depth.max(n);
                            let (lo, _) = side.min().unwrap();
                            let (hi, _) = side.max().unwrap();
                            let span = ((hi.raw() - lo.raw()) / 100).unsigned_abs() as usize;
                            span_ticks.add(span);
                            widest_span = widest_span.max(span);
                        }
                        if let (Some((bid, _)), Some((ask, _))) = (sb.best_bid(), sb.best_ask()) {
                            two_sided += 1;
                            spread_ticks.add(((ask.raw() - bid.raw()) / 100).max(0) as usize);
                        }
                    }
                    peak_two_sided = peak_two_sided.max(two_sided);
                }
            }
        }
    }
    let dt = start.elapsed();
    let s = b.stats;

    println!("  messages {messages:>12}   symbols {:>7}", b.len());
    println!("\n  book");
    println!("    price level updates {:>12}", s.updates);
    println!(
        "    deletes             {:>12}  {:.1}%",
        s.deletes,
        s.deletes as f64 / s.updates.max(1) as f64 * 100.0
    );
    println!(
        "    deletes of absent   {:>12}  {:.3}%  (expected only near session start)",
        s.deletes_of_absent,
        s.deletes_of_absent as f64 / s.deletes.max(1) as f64 * 100.0
    );
    println!(
        "    trades              {:>12}  {} shares",
        s.trades, s.traded_shares
    );

    println!("\n  INVARIANT — best bid must be below best ask at an event boundary");
    println!(
        "    locked, stable       {:>12}   (bid == ask: unusual, not a violation)",
        s.locked_when_stable
    );
    println!(
        "    crossed, mid-event   {:>12}   (legitimate: the feed says wait)",
        s.crossed_mid_event
    );
    println!(
        "    crossed, STABLE      {:>12}   {}",
        s.crossed_when_stable,
        if s.crossed_when_stable == 0 {
            "<- clean"
        } else {
            "<- BOOK CORRUPTION"
        }
    );

    println!("\n  SHAPE — measured before choosing the fast container");
    println!(
        "    levels per side       p50 {:>6}   p90 {:>6}   p99 {:>6}   max {}  (saturated {})",
        depth.pct(0.50),
        depth.pct(0.90),
        depth.pct(0.99),
        max_depth,
        depth.saturated()
    );
    println!(
        "    price span (¢ ticks)  p50 {:>6}   p90 {:>6}   p99 {:>6}   max {}  (saturated {})",
        span_ticks.pct(0.50),
        span_ticks.pct(0.90),
        span_ticks.pct(0.99),
        widest_span,
        span_ticks.saturated()
    );
    println!(
        "    spread (¢ ticks)      p50 {:>6}   p90 {:>6}   p99 {:>6}  (saturated {})",
        spread_ticks.pct(0.50),
        spread_ticks.pct(0.90),
        spread_ticks.pct(0.99),
        spread_ticks.saturated()
    );
    println!(
        "    two-sided symbols     peak {peak_two_sided} of {}",
        b.len()
    );
    println!("    peak levels held      {peak_levels}");
    println!("    samples               {samples} (every {SAMPLE_EVERY} messages)");
    println!(
        "    levels at session end {}  <- the close deletes everything",
        b.total_levels()
    );

    println!("\n  closing touch, busiest symbols");
    let mut syms: Vec<_> = b.iter().collect();
    syms.sort_unstable_by_key(|(_, sb)| std::cmp::Reverse(sb.bids.len() + sb.asks.len()));
    println!(
        "    {:<8} {:>10} {:>10} {:>12} {:>12}",
        "symbol", "bid lvls", "ask lvls", "best bid", "best ask"
    );
    for (sym, sb) in syms.iter().take(8) {
        let fmt = |o: Option<(Price, u32)>| o.map_or("-".to_string(), |(p, _)| p.to_string());
        println!(
            "    {:<8} {:>10} {:>10} {:>12} {:>12}",
            sym,
            sb.bids.len(),
            sb.asks.len(),
            fmt(sb.best_bid()),
            fmt(sb.best_ask())
        );
    }

    let secs = dt.as_secs_f64();
    println!(
        "\n  {:.3}s   {:.1} M msg/s   {:.1} M updates/s",
        secs,
        messages as f64 / secs / 1e6,
        s.updates as f64 / secs / 1e6
    );
}
