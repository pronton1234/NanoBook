//! Head-to-head: the same real update stream through each level container.
//!
//! Two things this deliberately does, because a benchmark that skips either is
//! measuring something other than what it claims.
//!
//! **Decode first, time second.** The updates are decoded into a `Vec` before
//! the clock starts. Timing a loop that also parses would attribute the parser's
//! cost to the book, and since the parser is fast and the book is faster, the
//! parser would dominate and every container would look identical.
//!
//! **Replay the real order.** The update stream is taken from a capture rather
//! than generated, so the mix of inserts, resizes and deletes, the depth
//! distribution, and the symbol locality are all whatever the market actually
//! did. A synthetic stream of uniformly random prices would flatter the
//! ordered map, because it would destroy the locality a real feed has.
//!
//! Each container is run several times and the **fastest** run is reported.
//! Minimum, not mean: the slow runs are the ones that got descheduled or lost a
//! cache to another process, and averaging those in measures the machine's
//! background noise rather than the code.
//!
//!     cargo run --release --example bakeoff -- data/raw/<file>.pcap [max_updates]

use std::time::Instant;

use book::{ArenaBook, BTreeLevels, Book, InlineLevels, Levels, SortedLevels};
use deep::{Message, PriceLevelUpdate};
use iextp::{capture::Capture, frame, segment::Segment, sequencer::Sequencer};

const RUNS: usize = 5;

/// A container that stores nothing, to establish the floor of the update path.
///
/// Two hypotheses about where the time goes have now been wrong -- first that
/// the symbol hash dominated, then that inlining the levels would help (it made
/// things worse, by doubling the hash table's working set). Rather than guess a
/// third time, this measures the path *without* a container: the hash, the
/// entry construction, the delete/absent bookkeeping and the crossed check.
///
/// Whatever is left after subtracting this really is the container.
#[derive(Debug, Default, Clone)]
struct NullLevels {
    len: usize,
}

impl Levels for NullLevels {
    #[inline]
    fn set(&mut self, _price: deep::Price, size: u32) {
        // Track a length so the book's delete-of-absent accounting still runs;
        // anything less and the floor would omit work the real path does.
        if size == 0 {
            self.len = self.len.saturating_sub(1);
        } else {
            self.len += 1;
        }
    }
    #[inline]
    fn max(&self) -> Option<(deep::Price, u32)> {
        None
    }
    #[inline]
    fn min(&self) -> Option<(deep::Price, u32)> {
        None
    }
    #[inline]
    fn len(&self) -> usize {
        self.len
    }
    fn total_size(&self) -> u64 {
        0
    }
    fn for_each(&self, _f: impl FnMut(deep::Price, u32)) {}
    fn clear(&mut self) {
        self.len = 0;
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: bakeoff <capture.pcap> [max_updates]");
    let cap: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5_000_000);

    let buf = std::fs::read(&path).expect("read capture");
    println!("{path}");
    print!("  decoding up to {cap} price level updates... ");

    let mut updates: Vec<PriceLevelUpdate> = Vec::with_capacity(cap.min(8_000_000));
    let reader = Capture::open(&buf).expect("open capture");
    let mut seq = Sequencer::default();
    'outer: for pkt in reader {
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
            if let Ok(Message::PriceLevel(u)) = deep::parse(body) {
                updates.push(u);
                if updates.len() >= cap {
                    break 'outer;
                }
            }
        }
    }
    // The capture is no longer needed and is multi-gigabyte; dropping it before
    // the timed section keeps it out of the cache the benchmark cares about.
    drop(buf);

    let n = updates.len();
    let symbols = {
        let mut s: Vec<u64> = updates.iter().map(|u| u.symbol.as_u64()).collect();
        s.sort_unstable();
        s.dedup();
        s.len()
    };
    println!("{n} updates, {symbols} symbols");

    let deletes = updates.iter().filter(|u| u.size == 0).count();
    println!(
        "  mix: {:.1}% deletes, {:.1}% inserts/resizes\n",
        deletes as f64 / n as f64 * 100.0,
        (n - deletes) as f64 / n as f64 * 100.0
    );

    println!(
        "  {:<16} {:>12} {:>12} {:>12} {:>10}",
        "container", "best ns/upd", "M upd/s", "total ms", "levels"
    );

    let null = run::<NullLevels>(&updates, symbols);
    report("(no container)", null, n, None);
    let btree = run::<BTreeLevels>(&updates, symbols);
    report("BTreeMap (R0)", btree, n, Some(btree.0));
    let sorted = run::<SortedLevels>(&updates, symbols);
    report("sorted SoA (R1)", sorted, n, Some(btree.0));
    let inline = run::<InlineLevels>(&updates, symbols);
    report("inline SoA (R1b)", inline, n, Some(btree.0));
    let arena = run_arena(&updates, symbols);
    report("arena (R1c)", arena, n, Some(btree.0));

    // ---- where does the time actually go? ----
    //
    // A 1.18x win from replacing the level container is a weak return for a
    // structure picked from measurement, and at ~52ns/update a scan over two
    // elements cannot be the cost. Decomposing the path is the only way to find
    // out what is, rather than tuning the container harder and hoping.
    println!("\n  DECOMPOSITION — measured floor, not a guess");
    let floor = null.0 as f64 / n as f64;
    println!(
        "  {:<26} {:>10.1} ns/upd",
        "book path with no container", floor
    );
    for (name, t) in [
        ("BTreeMap", btree.0),
        ("sorted SoA", sorted.0),
        ("inline SoA", inline.0),
        ("arena", arena.0),
    ] {
        let total = t as f64 / n as f64;
        println!(
            "  {:<26} {:>10.1} ns/upd   container = {:>5.1} ns ({:.0}% of it)",
            name,
            total,
            total - floor,
            (total - floor) / total * 100.0
        );
    }

    println!(
        "\n  Both containers must agree on the final book, or the faster one is\n  \
         only faster because it is wrong."
    );
    let a = final_state::<BTreeLevels>(&updates, symbols);
    let b = final_state::<SortedLevels>(&updates, symbols);
    let c = final_state::<InlineLevels>(&updates, symbols);
    let d = {
        let mut b = ArenaBook::with_capacity(symbols * 2);
        for u in &updates {
            b.apply_price_level(u);
        }
        (b.total_levels(), b.total_size())
    };
    println!("    BTreeMap    levels {:>8}  total size {:>14}", a.0, a.1);
    println!("    sorted SoA  levels {:>8}  total size {:>14}", b.0, b.1);
    println!("    inline SoA  levels {:>8}  total size {:>14}", c.0, c.1);
    println!("    arena       levels {:>8}  total size {:>14}", d.0, d.1);
    println!(
        "    {}",
        if a == b && b == c && c == d {
            "all four identical"
        } else {
            "<-- MISMATCH: a fast container is wrong"
        }
    );
}

/// The arena book is a different architecture, not a different container, so it
/// does not go through `Book<L>` and needs its own driver.
fn run_arena(updates: &[PriceLevelUpdate], symbols: usize) -> (u128, usize) {
    let mut best = u128::MAX;
    let mut levels = 0;
    for _ in 0..RUNS {
        let mut b = ArenaBook::with_capacity(symbols * 2);
        let start = Instant::now();
        for u in updates {
            b.apply_price_level(u);
        }
        let e = start.elapsed().as_nanos();
        levels = b.total_levels();
        best = best.min(e);
    }
    (best, levels)
}

/// Returns (best nanos, levels held at the end).
fn run<L: Levels>(updates: &[PriceLevelUpdate], symbols: usize) -> (u128, usize) {
    let mut best = u128::MAX;
    let mut levels = 0;
    for _ in 0..RUNS {
        let mut b: Book<L> = Book::with_capacity(symbols * 2);
        let start = Instant::now();
        for u in updates {
            b.apply_price_level(u);
        }
        let e = start.elapsed().as_nanos();
        // Keep the book observably alive so the loop cannot be optimised away.
        levels = b.total_levels();
        best = best.min(e);
    }
    (best, levels)
}

fn final_state<L: Levels>(updates: &[PriceLevelUpdate], symbols: usize) -> (usize, u64) {
    let mut b: Book<L> = Book::with_capacity(symbols * 2);
    for u in updates {
        b.apply_price_level(u);
    }
    (b.total_levels(), b.total_size())
}

fn report(name: &str, (nanos, levels): (u128, usize), n: usize, baseline: Option<u128>) {
    let per = nanos as f64 / n as f64;
    let speedup = baseline.map(|b| b as f64 / nanos as f64);
    println!(
        "  {:<16} {:>12.1} {:>12.1} {:>12.1} {:>10}{}",
        name,
        per,
        n as f64 / (nanos as f64 / 1e9) / 1e6,
        nanos as f64 / 1e6,
        levels,
        speedup.map_or(String::new(), |s| format!("   {s:.2}x vs R0"))
    );
}
