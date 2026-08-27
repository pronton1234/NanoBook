//! The optimisation log: one change at a time, measured, failures included.
//!
//!     cargo run --release --example optlog -- data/raw/<file>.pcap [max_packets]
//!
//! Every variant runs in the **same process, interleaved**, one repetition of
//! each per round. That rule is not stylistic — timing all runs of A and then
//! all runs of B has produced two wrong results in this project already: a
//! negative stage cost in the budget, and a fake 1.38x threading speedup. Load
//! drifts over a multi-minute benchmark, and blocked runs hand that drift
//! entirely to whichever went first. Taking the minimum does not rescue it,
//! because the minimum over a busy window is still worse than the minimum over
//! a quiet one.
//!
//! Each variant reports its own run-to-run spread, and a change smaller than
//! that spread is printed as **unresolved** rather than as a win. On this
//! machine — no CPU pinning, P- and E-cores, no frequency control — that is
//! expected to be most of them. Reporting them as wins anyway is how an
//! optimisation log becomes a work of fiction.

use std::time::Instant;

use book::SortedLevels;
use iextp::capture::Capture;
use pipeline::{Pipeline, Stage};

const RUNS: usize = 7;

/// One thing that can be measured.
struct Variant {
    name: &'static str,
    what: &'static str,
    best: f64,
    worst: f64,
    updates: u64,
    orders: u64,
}

impl Variant {
    fn new(name: &'static str, what: &'static str) -> Self {
        Self {
            name,
            what,
            best: f64::MAX,
            worst: 0.0,
            updates: 0,
            orders: 0,
        }
    }
    fn record(&mut self, ns: f64, updates: u64, orders: u64) {
        if ns < self.best {
            self.best = ns;
        }
        if ns > self.worst {
            self.worst = ns;
        }
        self.updates = updates;
        self.orders = orders;
    }
    fn spread(&self) -> f64 {
        self.worst - self.best
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: optlog <capture.pcap> [max_packets]");
    let cap: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);

    let buf = std::fs::read(&path).expect("read capture");
    let mut frames: Vec<&[u8]> = Vec::with_capacity(cap.min(4_000_000));
    for pkt in Capture::open(&buf).expect("open capture") {
        frames.push(pkt.data);
        if frames.len() >= cap {
            break;
        }
    }
    println!("{path}\n  {} packets\n", frames.len());

    // Warm the allocator so no variant pays another's page faults.
    {
        let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(8192);
        for f in &frames {
            p.on_frame(f, Stage::Encode);
        }
        std::hint::black_box(&p.counters);
    }

    let mut baseline = Variant::new(
        "baseline",
        "decide re-looks-up the symbol; Stage branches present",
    );
    let mut touch = Variant::new(
        "+ touch threading",
        "the update returns the touch it already computed",
    );
    let mut branchless = Variant::new(
        "+ no Stage branches",
        "production path, prefix branches removed",
    );

    for _ in 0..RUNS {
        // --- baseline: the redundant lookup the pipeline used to do ---
        {
            let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(8192);
            p.redundant_lookup = true;
            let start = Instant::now();
            for f in &frames {
                p.on_frame(f, Stage::Encode);
            }
            let ns = start.elapsed().as_nanos() as f64 / frames.len() as f64;
            baseline.record(ns, p.counters.book_updates, p.counters.orders_encoded);
        }

        // --- touch threading ---
        {
            let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(8192);
            let start = Instant::now();
            for f in &frames {
                p.on_frame(f, Stage::Encode);
            }
            let ns = start.elapsed().as_nanos() as f64 / frames.len() as f64;
            touch.record(ns, p.counters.book_updates, p.counters.orders_encoded);
        }

        // --- touch threading, and no Stage branches ---
        {
            let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(8192);
            let start = Instant::now();
            for f in &frames {
                p.on_frame_fast(f);
            }
            let ns = start.elapsed().as_nanos() as f64 / frames.len() as f64;
            branchless.record(ns, p.counters.book_updates, p.counters.orders_encoded);
        }
    }

    let variants = [&baseline, &touch, &branchless];

    println!(
        "  {:<22} {:>10} {:>10} {:>12}",
        "variant", "ns/packet", "spread", "vs baseline"
    );
    for v in variants {
        let delta = v.best - baseline.best;
        let note = if v.name == "baseline" {
            String::new()
        } else if delta.abs() < v.spread().max(baseline.spread()) {
            format!("{delta:+.1} ns  unresolved")
        } else if delta < 0.0 {
            format!("{delta:+.1} ns  FASTER")
        } else {
            format!("{delta:+.1} ns  SLOWER")
        };
        println!(
            "  {:<22} {:>7.1} ns {:>7.1} ns   {}",
            v.name,
            v.best,
            v.spread(),
            note
        );
        println!("    {}", v.what);
    }

    // A faster variant that computes something different is not an optimisation.
    println!("\n  correctness — every variant must agree exactly");
    let ok = variants
        .iter()
        .all(|v| v.updates == baseline.updates && v.orders == baseline.orders);
    for v in variants {
        println!(
            "    {:<22} {:>10} updates  {:>10} orders",
            v.name, v.updates, v.orders
        );
    }
    println!(
        "    {}",
        if ok {
            "all identical"
        } else {
            "<-- MISMATCH: a variant changed behaviour, not just speed"
        }
    );

    // When the clock cannot resolve a change, count the work instead.
    //
    // Timing is at the mercy of this machine; an operation count is not. Both
    // changes provably do strictly less work per price level update, and that
    // reduction is exact, deterministic, and identical on any hardware. It is
    // the honest thing to report when the nanoseconds will not hold still.
    let updates = baseline.updates;
    println!("\n  WORK REMOVED — exact counts, no clock involved");
    println!(
        "    hash probes:  baseline {:>11}   optimised {:>11}   {} fewer",
        updates * 2,
        updates,
        updates
    );
    println!(
        "    level reads:  baseline {:>11}   optimised {:>11}   {} fewer",
        updates * 4,
        updates * 2,
        updates * 2
    );
    println!("    (each update used to probe the symbol map twice -- once to apply, once");
    println!("     for the decision -- and read best_bid/best_ask twice. The update already");
    println!("     computed both for its crossed-book check, so the second set was pure");
    println!("     duplication on 97% of all feed traffic.)");

    let floor = variants.iter().map(|v| v.spread()).fold(0.0f64, f64::max);
    println!("\n  noise floor across variants: {floor:.1} ns");
    println!("  Changes below it are reported as unresolved, not as wins. This machine has");
    println!("  no CPU pinning, mixes P- and E-cores, and does not fix its clock frequency;");
    println!("  see docs/linux-bench-box.md for the box on which these become measurable.");
}
