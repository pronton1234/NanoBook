//! The latency budget: where a tick-to-trade nanosecond goes.
//!
//!     cargo run --release --example budget -- data/raw/<file>.pcap [max_packets]
//!
//! Three measurements, deliberately separate because they answer different
//! questions and conflating them is how latency benchmarks go wrong.
//!
//! 1. **Stage budget**, by cumulative prefix. Each stage is timed by running the
//!    pipeline up to that stage over the whole capture and differencing against
//!    the previous prefix. No clock appears inside the loop, so the numbers carry
//!    no measurement overhead at all.
//!
//! 2. **End-to-end distribution**, per packet. A quantile cannot be recovered
//!    from a total, so this one does pay two clock reads per packet — and the
//!    cost of those reads is measured and printed rather than assumed away.
//!
//! 3. **Coordinated-omission-corrected latency**, at a chosen offered rate. This
//!    is the number a trading system actually lives with. Service time — how long
//!    an item took once work started — is shown next to it, because the gap
//!    between them is the whole point.

use std::time::Instant;

use book::SortedLevels;
use iextp::capture::Capture;
use pipeline::{clock_overhead_nanos, Histogram, Pipeline, Replay, Stage};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: budget <capture.pcap> [max_packets]");
    let cap: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);

    let buf = std::fs::read(&path).expect("read capture");
    println!("{path}");

    // Collect frames up front so the timed loops measure the pipeline, not the
    // capture reader.
    let mut frames: Vec<&[u8]> = Vec::with_capacity(cap.min(4_000_000));
    for pkt in Capture::open(&buf).expect("open capture") {
        frames.push(pkt.data);
        if frames.len() >= cap {
            break;
        }
    }
    println!("  {} packets\n", frames.len());

    // Warm up on the FULL pipeline before any timing.
    //
    // Without this the first stage that builds a book pays every page fault for
    // ~6,000 symbol entries and their level storage, while later stages reuse
    // pages the allocator already holds. That artefact produced a `book update`
    // prefix 2.7x its true cost and, by differencing, a NEGATIVE cost for the
    // stage after it -- which is how it was caught. A negative stage cost is
    // impossible, so it could only be the measurement.
    {
        let mut warm: Pipeline<SortedLevels> = Pipeline::with_capacity(8192);
        for f in &frames {
            warm.on_frame(f, Stage::Encode);
        }
        std::hint::black_box(&warm.counters);
    }

    // ---- 1. stage budget, by cumulative prefix ----
    //
    // Repetitions are ROUND-ROBIN across stages, not stage-by-stage. Timing all
    // runs of one stage before moving to the next lets thermal drift and
    // background load accumulate along the stage order, which shows up as a
    // later stage appearing cheaper than an earlier one -- and differencing then
    // yields a negative cost. Round-robin spreads that drift evenly.
    //
    // The min-max spread per stage is printed so the noise floor is visible: a
    // stage difference smaller than the spread is not a measurement.
    const REPS: usize = 7;
    println!("  STAGE BUDGET — cumulative prefixes, no clock inside the loop");
    let mut best: Vec<f64> = vec![f64::MAX; Stage::ALL.len()];
    let mut worst: Vec<f64> = vec![0.0; Stage::ALL.len()];
    let mut counters = Default::default();
    for _ in 0..REPS {
        for (si, stage) in Stage::ALL.iter().enumerate() {
            let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(8192);
            let start = Instant::now();
            for f in &frames {
                p.on_frame(f, *stage);
            }
            let ns = start.elapsed().as_nanos() as f64 / frames.len() as f64;
            if ns < best[si] {
                best[si] = ns;
            }
            if ns > worst[si] {
                worst[si] = ns;
            }
            if *stage == Stage::Encode {
                counters = p.counters;
            }
        }
    }

    println!(
        "  {:<28} {:>12} {:>12} {:>14}",
        "stage", "cumulative", "this stage", "noise (max-min)"
    );
    let mut last = 0f64;
    for (si, stage) in Stage::ALL.iter().enumerate() {
        let cum = best[si];
        let noise = worst[si] - best[si];
        let delta = cum - last;
        // Flag ANY stage whose cost is smaller than the run-to-run spread, not
        // just negative ones. A positive number below the noise floor is still
        // not a measurement, and reporting it unqualified is how a plausible
        // figure gets quoted as a fact.
        let flag = if delta < 0.0 {
            "  <-- NEGATIVE, investigate"
        } else if delta < noise {
            "  <-- below noise floor"
        } else {
            ""
        };
        println!(
            "  {:<28} {:>9.1} ns {:>9.1} ns {:>11.1} ns{}",
            stage.name(),
            cum,
            delta,
            noise,
            flag
        );
        last = cum;
    }
    let full = *best.last().unwrap();
    println!(
        "  {:<28} {:>9.1} ns  per packet",
        "TOTAL tick-to-trade", full
    );
    println!(
        "\n  {} messages, {} book updates, {} orders encoded, {} encode errors",
        counters.messages, counters.book_updates, counters.orders_encoded, counters.encode_errors
    );
    println!(
        "  per message: {:.1} ns",
        full * frames.len() as f64 / counters.messages.max(1) as f64
    );
    let unresolved = Stage::ALL
        .iter()
        .enumerate()
        .filter(|(si, _)| {
            let prev = if *si == 0 { 0.0 } else { best[si - 1] };
            (best[*si] - prev) < (worst[*si] - best[*si])
        })
        .count();
    if unresolved > 0 {
        println!(
            "\n  NOTE: {unresolved} of {} stage costs are below their own run-to-run spread.",
            Stage::ALL.len()
        );
        println!("  The TOTAL is a large aggregate and holds; the per-stage split does not");
        println!("  resolve on this machine. macOS gives no core pinning or CPU isolation,");
        println!("  so this needs the pinned x86 Linux box before the breakdown is quotable.");
    }

    // ---- 2. end-to-end distribution ----
    let clock = clock_overhead_nanos();
    println!("\n  END-TO-END DISTRIBUTION — per packet");
    println!("  one Instant::now() costs {clock:.1} ns; two per packet are included below");
    let mut hist = Histogram::new();
    let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(8192);
    for f in &frames {
        let t0 = Instant::now();
        p.on_frame(f, Stage::Encode);
        hist.record(t0.elapsed().as_nanos() as u64);
    }
    print_hist("measured (includes clock)", &hist, clock);

    // ---- 3. coordinated-omission correction ----
    //
    // Offered rate is set from the pipeline's own measured throughput so the
    // run sits near saturation, which is where the correction matters. Well
    // below capacity the corrected and service figures agree, and a benchmark
    // that only ever runs there is not testing the correction at all.
    let capacity = 1e9 / full;
    let mut service_p50: Vec<u64> = Vec::new();
    let mut corrected_p50: Vec<u64> = Vec::new();
    for factor in [0.5, 0.9, 1.1] {
        let rate = capacity * factor;
        println!(
            "\n  COORDINATED OMISSION — offered {:.2} M packets/s ({:.0}% of measured capacity)",
            rate / 1e6,
            factor * 100.0
        );
        let mut r = Replay::new(rate);
        let mut p: Pipeline<SortedLevels> = Pipeline::with_capacity(8192);
        let per_item_ns = 1e9 / rate;
        let origin = Instant::now();
        // ONE clock read per packet, not two. At 48.8 ns a read and ~100 ns of
        // work, reading twice would add more overhead than the pipeline costs,
        // inflate it into falling behind schedule, and corrupt the very
        // correction being measured. The previous packet's finish is this
        // packet's start, which is also what a real receive loop sees.
        // Two reads here, unlike the budget above, and deliberately: with one
        // read the "service time" of an under-loaded run collapses to zero,
        // because the item finishes before its own scheduled arrival and the
        // idle wait is indistinguishable from the work. Measuring the work
        // interval directly costs ~2 clock reads per packet, which is disclosed
        // rather than hidden -- this section exists to show the service/corrected
        // divergence, not to publish the pipeline's headline latency.
        let mut queue_free_at = 0u64;
        for (i, f) in frames.iter().enumerate() {
            let scheduled = (i as f64 * per_item_ns) as u64;
            let work_start = origin.elapsed().as_nanos() as u64;
            p.on_frame(f, Stage::Encode);
            let work_end = origin.elapsed().as_nanos() as u64;

            // Model the queue: an item is served after it arrives AND after the
            // previous item is done, then takes as long as the work took.
            let service = work_end.saturating_sub(work_start);
            let started = queue_free_at.max(scheduled);
            let finished = started + service;
            queue_free_at = finished;
            r.record(scheduled, started, finished);
        }
        println!(
            "    behind schedule: {} of {} packets ({:.1}%)",
            r.behind,
            frames.len(),
            r.behind as f64 / frames.len() as f64 * 100.0
        );
        print_hist("service time (the flattering one)", &r.service, 0.0);
        print_hist("corrected latency (the honest one)", &r.corrected, 0.0);
        service_p50.push(r.service.quantile(0.50));
        corrected_p50.push(r.corrected.quantile(0.50));
    }

    println!("\n  WHY THIS MATTERS");
    println!("  service-time p50 across the three loads:   {service_p50:?} ns");
    println!("  corrected   p50 across the three loads:    {corrected_p50:?} ns");
    println!("  Service time barely moves as the system is driven past capacity -- it is");
    println!("  measuring only the items the system got around to, and never the queue");
    println!("  behind them. A benchmark reporting the first number would show a pipeline");
    println!("  that is equally fast at 50% and 110% load, which is not a property any");
    println!("  system has.");
}

fn print_hist(label: &str, h: &Histogram, subtract: f64) {
    let adj = |v: u64| -> String {
        if v == u64::MAX {
            return "overflow".to_string();
        }
        let x = v as f64 - subtract;
        if x >= 1_000_000.0 {
            format!("{:.2} ms", x / 1e6)
        } else if x >= 1_000.0 {
            format!("{:.2} us", x / 1e3)
        } else {
            format!("{x:.0} ns")
        }
    };
    println!(
        "    {label:<36} p50 {:>10}  p99 {:>10}  p99.9 {:>10}  max {:>10}",
        adj(h.quantile(0.50)),
        adj(h.quantile(0.99)),
        adj(h.quantile(0.999)),
        adj(h.max())
    );
    if h.overflows() > 0 {
        println!("      {} values above the recordable range", h.overflows());
    }
}
