//! The measurement the server exposes.
//!
//! This runs the same `Pipeline` the native benchmarks run, over a capture the
//! `synth` crate generates, and reports the same statistics with the same
//! discipline — including refusing to quote a difference smaller than the
//! run-to-run spread.
//!
//! That last part matters more here than anywhere else in the project. The
//! server is expected to run on shared, burstable cloud CPU, where the spread is
//! large. Reporting a clean number from such a host would be the one dishonest
//! thing in this repository, so the spread travels with every figure and a
//! difference inside it is labelled unresolved.

use std::time::Instant;

use book::{BTreeLevels, Levels, SortedLevels};
use deep::Symbol;
use pipeline::{Histogram, Pipeline, Stage};

/// Which level container to run. The bake-off, exposed as a knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    BTree,
    Sorted,
}

impl Container {
    pub fn parse(s: &str) -> Self {
        match s {
            "btree" => Container::BTree,
            _ => Container::Sorted,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Container::BTree => "BTreeMap (R0)",
            Container::Sorted => "sorted SoA (R1)",
        }
    }
}

/// Knobs a visitor can turn. All bounded: the server never takes code, a path,
/// or an unbounded size.
#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub container: Container,
    /// Fraction of segments sent twice, modelling the B feed.
    pub duplicate_rate: f64,
    /// Fraction of segments dropped, modelling loss.
    pub gap_rate: f64,
    /// Offered load as a fraction of measured capacity, for the
    /// coordinated-omission demonstration.
    pub load: f64,
}

impl Request {
    pub fn clamped(mut self) -> Self {
        self.duplicate_rate = self.duplicate_rate.clamp(0.0, 1.0);
        self.gap_rate = self.gap_rate.clamp(0.0, 0.25);
        self.load = self.load.clamp(0.1, 2.0);
        self
    }
}

#[derive(Debug, Clone)]
pub struct TopOfBook {
    pub symbol: String,
    pub bid: Option<(String, u32)>,
    pub ask: Option<(String, u32)>,
    pub bid_levels: usize,
    pub ask_levels: usize,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub container: &'static str,
    pub packets: u64,
    pub messages: u64,
    pub book_updates: u64,
    pub orders: u64,
    /// Best of the repetitions, nanoseconds per packet.
    pub ns_per_packet: f64,
    /// Worst minus best. Every figure here is read against this.
    pub spread: f64,
    pub duplicates_suppressed: u64,
    pub gaps_detected: u64,
    pub gaps_injected: u64,
    pub crossed_when_stable: u64,
    pub service_p50: u64,
    pub service_p99: u64,
    pub corrected_p50: u64,
    pub corrected_p99: u64,
    pub behind_schedule: u64,
    pub top: Vec<TopOfBook>,
}

const REPS: usize = 5;

/// Capture held in memory, generated once and reused across requests.
pub struct Corpus {
    pub frames: Vec<Vec<u8>>,
    pub gaps_injected: u64,
}

pub fn build_corpus(req: Request, segments: usize) -> Corpus {
    let cfg = synth::Config {
        segments,
        duplicate_rate: req.duplicate_rate,
        gap_rate: req.gap_rate,
        ..Default::default()
    };
    let cap = synth::generate(&cfg);
    // Split into frames once, so the timed loop measures the pipeline rather
    // than the capture reader.
    let mut frames = Vec::new();
    for pkt in iextp::capture::Capture::open(&cap.bytes).expect("generated capture is readable") {
        frames.push(pkt.data.to_vec());
    }
    Corpus {
        frames,
        gaps_injected: cap.gaps,
    }
}

pub fn run(req: Request, corpus: &Corpus) -> Report {
    match req.container {
        Container::BTree => run_with::<BTreeLevels>(req, corpus),
        Container::Sorted => run_with::<SortedLevels>(req, corpus),
    }
}

fn run_with<L: Levels>(req: Request, corpus: &Corpus) -> Report {
    let frames = &corpus.frames;

    // Warm up before timing anything. The first pass pays every page fault for
    // the symbol table and level storage; timing it reports the allocator, and
    // in this project it once produced an impossible negative stage cost.
    {
        let mut p: Pipeline<L> = Pipeline::with_capacity(1024);
        for f in frames {
            p.on_frame_fast(f);
        }
        std::hint::black_box(&p.counters);
    }

    let mut best = f64::MAX;
    let mut worst = 0f64;
    let mut counters = Default::default();
    let mut seq_stats = iextp::sequencer::Stats::default();
    let mut crossed = 0u64;
    let mut top = Vec::new();

    for _ in 0..REPS {
        let mut p: Pipeline<L> = Pipeline::with_capacity(1024);
        let start = Instant::now();
        for f in frames {
            p.on_frame_fast(f);
        }
        let ns = start.elapsed().as_nanos() as f64 / frames.len().max(1) as f64;
        if ns < best {
            best = ns;
        }
        if ns > worst {
            worst = ns;
        }
        counters = p.counters;
        seq_stats = p.seq.stats;
        crossed = p.book.stats.crossed_when_stable;
        top = snapshot(&p);
    }

    // Coordinated omission, at the requested fraction of measured capacity.
    let capacity = 1e9 / best.max(1.0);
    let rate = capacity * req.load;
    let per_item = 1e9 / rate;
    let mut service = Histogram::new();
    let mut corrected = Histogram::new();
    let mut behind = 0u64;
    {
        let mut p: Pipeline<L> = Pipeline::with_capacity(1024);
        let origin = Instant::now();
        let mut queue_free_at = 0u64;
        for (i, f) in frames.iter().enumerate() {
            let scheduled = (i as f64 * per_item) as u64;
            let work_start = origin.elapsed().as_nanos() as u64;
            p.on_frame_fast(f);
            let work_end = origin.elapsed().as_nanos() as u64;
            let took = work_end.saturating_sub(work_start);
            let started = queue_free_at.max(scheduled);
            let finished = started + took;
            queue_free_at = finished;
            if started > scheduled {
                behind += 1;
            }
            service.record(took);
            corrected.record(finished.saturating_sub(scheduled));
        }
    }

    Report {
        container: req.container.name(),
        packets: counters.packets,
        messages: counters.messages,
        book_updates: counters.book_updates,
        orders: counters.orders_encoded,
        ns_per_packet: best,
        spread: worst - best,
        duplicates_suppressed: seq_stats.duplicates,
        gaps_detected: seq_stats.gaps_opened,
        gaps_injected: corpus.gaps_injected,
        crossed_when_stable: crossed,
        service_p50: service.quantile(0.50),
        service_p99: service.quantile(0.99),
        corrected_p50: corrected.quantile(0.50),
        corrected_p99: corrected.quantile(0.99),
        behind_schedule: behind,
        top,
    }
}

/// One stage's measured cost.
pub struct StageCost {
    pub name: &'static str,
    pub cumulative: f64,
    pub this_stage: f64,
    pub spread: f64,
    /// True when the stage's cost is smaller than its own run-to-run spread,
    /// in which case it is not a measurement and must not be quoted as one.
    pub unresolved: bool,
}

/// The stage budget, by cumulative prefix.
///
/// Could not be resolved on the development laptop: five of six stages came in
/// under their own noise floor, because macOS offers no CPU pinning and the M2
/// mixes performance and efficiency cores. A dedicated, idle VM may do better --
/// which is the point of running it here rather than renting a machine to find
/// out.
///
/// Repetitions are round-robin across stages, never blocked per stage. Timing
/// all runs of one stage before the next lets load drift accumulate along the
/// stage order, and differencing then produces a negative cost. That is not
/// hypothetical; it happened, and a negative time is the only reason it was
/// noticed.
pub fn stage_budget(corpus: &Corpus) -> Vec<StageCost> {
    const REPS: usize = 9;
    let frames = &corpus.frames;

    // Warm on the FULL pipeline first, or the first prefix to build a book pays
    // every page fault and the stage after it comes out negative.
    {
        let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(1024);
        for f in frames {
            p.on_frame(f, Stage::Encode);
        }
        std::hint::black_box(&p.counters);
    }

    let mut best = vec![f64::MAX; Stage::ALL.len()];
    let mut worst = vec![0f64; Stage::ALL.len()];
    for _ in 0..REPS {
        for (i, stage) in Stage::ALL.iter().enumerate() {
            let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(1024);
            let start = Instant::now();
            for f in frames {
                p.on_frame(f, *stage);
            }
            let ns = start.elapsed().as_nanos() as f64 / frames.len().max(1) as f64;
            best[i] = best[i].min(ns);
            worst[i] = worst[i].max(ns);
        }
    }

    let mut out = Vec::new();
    let mut last = 0f64;
    for (i, stage) in Stage::ALL.iter().enumerate() {
        let delta = best[i] - last;
        let spread = worst[i] - best[i];
        out.push(StageCost {
            name: stage.name(),
            cumulative: best[i],
            this_stage: delta,
            spread,
            unresolved: delta < spread,
        });
        last = best[i];
    }
    out
}

/// Top of book for a handful of symbols, so a visitor sees an actual market
/// rather than only statistics.
fn snapshot<L: Levels>(p: &Pipeline<L>) -> Vec<TopOfBook> {
    let mut out = Vec::new();
    for t in ["AAPL", "MSFT", "SPY", "QQQ", "NVDA", "TSLA"] {
        let sym = Symbol::from_ticker(t);
        if let Some(sb) = p.book.get(sym) {
            out.push(TopOfBook {
                symbol: t.to_string(),
                bid: sb.best_bid().map(|(p, s)| (p.to_string(), s)),
                ask: sb.best_ask().map(|(p, s)| (p.to_string(), s)),
                bid_levels: sb.bids.len(),
                ask_levels: sb.asks.len(),
            });
        }
    }
    out
}
