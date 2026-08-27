//! The Rust half of the cross-language comparison.
//!
//!     cargo run --release -p book --example xlang -- <capture.pcap> [reps]
//!
//! Deliberately mirrors `cpp/src/bench.cpp` operation for operation: pcap record
//! → frame → segment → DEEP → book, and nothing else. The sequencer, strategy
//! and order encoder are all excluded, not because they do not matter but
//! because the C++ port does not implement them, and a comparison where one side
//! does extra work measures the extra work.
//!
//! Both read the *same file*. A cross-language benchmark over two different
//! corpora measures the corpora.
//!
//! What this can and cannot show:
//!
//! * It **can** show whether the two implementations of the same data structures
//!   and the same decode path land in the same place.
//! * It **cannot** settle "Rust vs C++" in general. Two programs, one workload,
//!   one machine, two compilers. Anyone presenting that as a language verdict is
//!   overreaching, and this file says so rather than letting the number imply it.

use std::time::Instant;

use book::{Book, SortedLevels};
use deep::Message;
use iextp::{capture::Capture, frame, segment::Segment};

struct Counters {
    packets: u64,
    messages: u64,
    book_updates: u64,
    crossed: u64,
    levels: usize,
    total_size: u64,
}

fn run_once(frames: &[&[u8]]) -> Counters {
    let mut book: Book<SortedLevels> = Book::with_capacity(8192);
    let mut packets = 0u64;
    let mut messages = 0u64;
    let mut book_updates = 0u64;

    for f in frames {
        packets += 1;
        let Ok(dg) = frame::parse(f) else { continue };
        let Ok(seg) = Segment::parse(dg.payload) else {
            continue;
        };
        for body in seg.messages() {
            let Ok(msg) = deep::parse(body) else { continue };
            messages += 1;
            if let Message::PriceLevel(u) = msg {
                book.apply_price_level(&u);
                book_updates += 1;
            }
        }
    }

    Counters {
        packets,
        messages,
        book_updates,
        crossed: book.stats.crossed_when_stable,
        levels: book.total_levels(),
        total_size: book.total_size(),
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: xlang <capture.pcap> [reps]");
    let reps: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(7);

    let buf = std::fs::read(&path).expect("read capture");
    let frames: Vec<&[u8]> = Capture::open(&buf)
        .expect("not a pcap file")
        .map(|p| p.data)
        .collect();
    println!("{path}\n  {} packets\n", frames.len());

    // Warm before timing: the first pass faults in every page of the symbol
    // table and level storage, and timing it measures the allocator.
    let warm = run_once(&frames);

    let mut best = f64::MAX;
    let mut worst = 0f64;
    for _ in 0..reps {
        let start = Instant::now();
        let c = run_once(&frames);
        let ns = start.elapsed().as_nanos() as f64 / frames.len() as f64;
        std::hint::black_box(&c);
        if ns < best {
            best = ns;
        }
        if ns > worst {
            worst = ns;
        }
    }

    println!("  {:<26} {:>10.2} ns/packet", "tick-to-book (Rust)", best);
    println!("  {:<26} {:>10.2} ns   run-to-run", "spread", worst - best);
    println!(
        "  {:<26} {:>10.2} ns/message",
        "per message",
        best * frames.len() as f64 / warm.messages.max(1) as f64
    );
    println!("\n  messages       {:>12}", warm.messages);
    println!("  book updates   {:>12}", warm.book_updates);
    println!("  levels held    {:>12}", warm.levels);
    println!("  total size     {:>12}", warm.total_size);
    println!(
        "  crossed@stable {:>12}  {}",
        warm.crossed,
        if warm.crossed == 0 {
            "<- clean"
        } else {
            "<- BOOK CORRUPTION"
        }
    );
    let _ = warm.packets;
}
